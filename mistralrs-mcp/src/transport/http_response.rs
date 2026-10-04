//! Bounded decoding of one HTTP JSON-RPC exchange, not an MCP session/resumption layer.

use anyhow::{bail, Result};
use futures_util::{Stream, StreamExt, TryStreamExt};
use serde_json::Value;

const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

pub(super) async fn read_response(response: reqwest::Response, expected_id: u64) -> Result<Value> {
    let response = response.error_for_status()?;
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    // Unlike a long-lived chat stream, this body belongs to one finite RPC. Count every byte
    // before SSE decoding, including comments/notifications, to bound any unfinished line/frame.
    let mut received = 0usize;
    let bytes = response.bytes_stream().map(move |item| {
        let chunk = item.map_err(std::io::Error::other)?;
        received = received
            .checked_add(chunk.len())
            .ok_or_else(|| std::io::Error::other("MCP response size overflow"))?;
        if received > MAX_RESPONSE_BYTES {
            return Err(std::io::Error::other(format!(
                "MCP response exceeded {MAX_RESPONSE_BYTES} bytes"
            )));
        }
        Ok(chunk)
    });
    match content_type.as_str() {
        "text/event-stream" => {
            read_events(sse_stream::SseByteStream::new(bytes), expected_id).await
        }
        "application/json" => {
            let body = bytes
                .try_fold(Vec::new(), |mut body, chunk| async move {
                    body.extend_from_slice(&chunk);
                    Ok(body)
                })
                .await?;
            let value: Value = serde_json::from_slice(&body)?;
            match classify_message(value, expected_id)? {
                Some(result) => Ok(result),
                None => bail!("MCP JSON response was a notification, not a response"),
            }
        }
        _ => bail!("unsupported MCP response content type: {content_type}"),
    }
}

pub(crate) async fn read_events(
    events: impl Stream<Item = std::result::Result<sse_stream::Sse, sse_stream::Error>>,
    expected_id: u64,
) -> Result<Value> {
    futures_util::pin_mut!(events);
    while let Some(event) = events.next().await {
        let Some(data) = event?.data else {
            continue;
        };
        let value = serde_json::from_str(&data)?;
        if let Some(result) = classify_message(value, expected_id)? {
            // Returning drops this response immediately; a peer need not close its SSE body
            // before the completed RPC can return to the caller.
            return Ok(result);
        }
    }
    bail!("MCP stream ended before the matching JSON-RPC response")
}

fn classify_message(value: Value, expected_id: u64) -> Result<Option<Value>> {
    if value.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        bail!("invalid MCP JSON-RPC version");
    }
    if value.get("method").is_some() {
        if value.get("id").is_some() {
            bail!("MCP server-initiated requests are unsupported by this transport");
        }
        if value.get("method").and_then(Value::as_str).is_none()
            || value.get("result").is_some()
            || value.get("error").is_some()
        {
            bail!("invalid MCP notification");
        }
        return Ok(None);
    }
    if value.get("id").and_then(Value::as_u64) != Some(expected_id) {
        bail!("MCP response id does not match request {expected_id}");
    }
    if value.get("result").is_some() == value.get("error").is_some() {
        bail!("MCP response must contain exactly one of result and error");
    }
    super::extract_jsonrpc_result(value).map(Some)
}
