use std::{io::Cursor, time::Duration};

use futures_util::{stream, StreamExt};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use wiremock::{matchers::method, Mock, MockServer, ResponseTemplate};

use crate::transport::{http_response::read_events, HttpTransport, McpTransport};

#[tokio::test]
async fn sse_notifications_and_multiline_unicode_survive_every_split() {
    let wire = "\u{feff}:ping\r\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\r\n\r\ndata: {\"jsonrpc\":\"2.0\",\n data: ignored\ndata: \"id\":1,\"result\":{\"text\":\"中\"}}\n\n";
    for split in 0..=wire.len() {
        let chunks = stream::iter(vec![
            Ok::<_, std::io::Error>(Cursor::new(wire.as_bytes()[..split].to_vec())),
            Ok(Cursor::new(wire.as_bytes()[split..].to_vec())),
        ]);
        let result = read_events(sse_stream::SseByteStream::new(chunks), 1)
            .await
            .unwrap();
        assert_eq!(result, json!({"text":"中"}), "split {split}");
    }
}

#[tokio::test]
async fn matching_result_returns_without_polling_an_open_tail() {
    let chunks = stream::once(async {
        Ok::<_, std::io::Error>(Cursor::new(
            b"data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n\n".to_vec(),
        ))
    })
    .chain(stream::pending());
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        read_events(sse_stream::SseByteStream::new(chunks), 1),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(result, json!({}));
}

#[tokio::test]
async fn malformed_or_unrelated_messages_fail_instead_of_becoming_results() {
    for (payload, message) in [
        (r#"{"jsonrpc":"2.0","id":2,"result":{}}"#, "does not match"),
        (
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-1,"message":"failed"}}"#,
            "MCP server error",
        ),
        (
            r#"{"jsonrpc":"2.0","id":9,"method":"sampling/createMessage"}"#,
            "server-initiated",
        ),
        (
            r#"{"jsonrpc":"2.0","id":1,"result":{},"error":{}}"#,
            "exactly one",
        ),
        (r#"{"id":1,"result":{}}"#, "version"),
        (r#"{"jsonrpc":"2.0","method":7}"#, "notification"),
        ("{", "EOF"),
    ] {
        let chunks = stream::iter(vec![Ok::<_, std::io::Error>(Cursor::new(
            format!("data: {payload}\n\n").into_bytes(),
        ))]);
        let error = read_events(sse_stream::SseByteStream::new(chunks), 1)
            .await
            .unwrap_err();
        assert!(error.to_string().contains(message), "{error:#}");
    }
    for wire in [
        "",
        "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n",
        "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n\n",
    ] {
        let chunks = stream::iter(vec![Ok::<_, std::io::Error>(Cursor::new(
            wire.as_bytes().to_vec(),
        ))]);
        assert!(read_events(sse_stream::SseByteStream::new(chunks), 1)
            .await
            .unwrap_err()
            .to_string()
            .contains("ended before"));
    }
}

#[tokio::test]
async fn http_status_media_type_and_single_request_budget_are_enforced() {
    for (response, expected) in [
        (ResponseTemplate::new(401).set_body_string("denied"), "401"),
        (ResponseTemplate::new(500).set_body_string("failed"), "500"),
        (
            ResponseTemplate::new(200).set_body_raw("{}", "text/plain"),
            "content type",
        ),
        (
            ResponseTemplate::new(200).set_body_raw(
                format!("data: {}", "x".repeat(8 * 1024 * 1024)),
                "text/event-stream",
            ),
            "exceeded",
        ),
        (
            ResponseTemplate::new(200).set_body_json(json!({"jsonrpc":"2.0","id":2,"result":{}})),
            "does not match",
        ),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(response)
            .expect(1)
            .mount(&server)
            .await;
        let client = HttpTransport::new(server.uri(), Some(3), None).unwrap();
        let error = client.send_request("ping", json!({})).await.unwrap_err();
        assert!(format!("{error:#}").contains(expected), "{error:#}");
    }
}

#[tokio::test]
async fn initialization_notification_rejects_http_failure() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(403))
        .expect(1)
        .mount(&server)
        .await;
    let client = HttpTransport::new(server.uri(), Some(3), None).unwrap();
    assert!(client
        .send_initialization_notification()
        .await
        .unwrap_err()
        .to_string()
        .contains("403"));
}

#[tokio::test]
async fn http_result_closes_the_read_without_waiting_for_server_eof() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut headers = Vec::new();
            while !headers.ends_with(b"\r\n\r\n") { headers.push(socket.read_u8().await.unwrap()); }
            let headers = String::from_utf8(headers).unwrap();
            let length = headers.lines().find_map(|line| {
                let (key, value) = line.split_once(':')?;
                key.eq_ignore_ascii_case("content-length").then(|| value.trim().parse::<usize>().unwrap())
            }).unwrap();
            socket.read_exact(&mut vec![0; length]).await.unwrap();
            let event = "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\n\n";
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{event}\r\n", event.len()).as_bytes()).await.unwrap();
            // No terminating HTTP chunk: only the client can release this pending body.
            let mut byte = [0];
            match socket.read(&mut byte).await {
                Ok(0) => {},
                Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {},
                other => panic!("completed RPC retained its response: {other:?}"),
            }
        });
        let client = HttpTransport::new(url, Some(3), None).unwrap();
        assert_eq!(client.send_request("ping", json!({})).await.unwrap(), json!({"ok":true}));
        server.await.unwrap();
    }).await.expect("a completed response must not wait for EOF");
}
