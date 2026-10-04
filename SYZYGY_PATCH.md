# Engine shutdown, MCP HTTP responses, and build metadata for Syzygy

Upstream baseline: [v0.9.4](https://github.com/EricLBuehler/mistral.rs/releases/tag/v0.9.4), commit
`4400935451da5e2dc7379a3f92fbbada66557f6c`, the latest stable release checked
on 2026-10-04. The SDK and its sibling crates remain at 0.9.4. No model
algorithm, scheduler, or tensor implementation is changed by these patches.

## Problem and boundary

`MistralRs` privately owns each native `std::thread::JoinHandle`. Upstream 0.9.4
provides asynchronous `shutdown(self: Arc<Self>)`, but requires exclusive Arc
ownership, performs blocking joins on its async caller, and does not return
native join failures. Remove/unload/reboot can discard a handle without joining
it, and Drop only tries to send `Request::Terminate`. The consumer cannot repair
these private native ownership paths at its own public model Arc boundary.

## Patch

- `MistralRs::shutdown_blocking()` closes admission, waits for already admitted model
  management, signals the engines, joins their actual threads, then releases the
  corresponding model resources outside the registry lock.
- Concurrent and repeated shutdown callers share a retained result, including
  native-thread and resource-destructor failures. A model retirement failure
  fences later model management instead of silently enabling a replacement.
- Remove, unload, and reboot use the same join path. The existing reload set
  reserves the model while its worker is retiring; a scoped guard also clears
  this reservation on early returns. Unload checks loader configuration before
  removing the engine. A new worker that cannot be registered is joined before
  that error returns.
- The original engine thread and its Tokio worker/blocking threads carry only
  thread-local execution identity, so a callback cannot synchronously join the
  runtime executing it. This marker owns no model and is not a model registry.
- Engine readiness failures retain and join the newly created worker before
  returning; a failed warmup send no longer panics and discards its handle.

The existing asynchronous `shutdown(self: Arc<Self>)` API delegates to
`shutdown_blocking()` through a blocking worker, supports shared owners, and
rejects engine-thread calls before that handoff. Cancelling an async waiter does
not cancel the close already owned by the blocking worker. Direct callers of
`shutdown_blocking()` must use a blocking worker. The upstream best-effort Drop
behavior remains an emergency signal, not a join guarantee.
Syzygy owns the public model inside its existing shared `ModelRuntime`, stops
new generation, drains admitted native responses, calls this shutdown, and only
then releases the counted model lease and awaits retirement.

## MCP HTTP response boundary

Upstream's HTTP transport reads an entire SSE body before returning and parses
individual `data:` lines. An open response can therefore wait until timeout
after the matching result arrived, while multiline events and preceding
notifications can be misinterpreted. This belongs inside the transport because
the caller receives only its decoded JSON-RPC result.

The transport uses `sse-stream` for incremental SSE framing, skips valid
notifications, validates JSON-RPC version and the numeric request ID, and
returns as soon as the matching result arrives. HTTP failures, unsupported
media types, malformed messages, server-initiated requests, and premature EOF
are errors. JSON and SSE responses share an 8 MiB total byte budget, including
comments and notifications. Initialization notifications also reject HTTP
failure statuses. This is one finite HTTP RPC exchange; MCP session management,
resumption, and server-initiated request handling are outside this patch.

## Git revision build inputs

Upstream `mistralrs-core/build.rs` reads the correct revision through Git but
emits `rerun-if-changed=.git/HEAD` relative to the core package. That path does
not exist in this multi-crate checkout, including its submodule layout. Cargo
treats a missing watched input as stale, rerunning the build script and
rebuilding consumers even when their inputs are unchanged.

The script now asks Git for the actual HEAD, branch ref, and packed-refs paths,
including linked worktrees' shared ref storage. It watches only existing
inputs. When a branch ref is packed, its nearest existing refs directory is
watched so creating a loose ref also invalidates the revision. Detached HEAD
needs only its HEAD file. `build.rs` is always watched; a source archive not
tracked by its enclosing Git repository reports `unknown` without inventing
missing Git inputs. There is no new build dependency or recursive watch of
the entire Git metadata directory.

References: [Cargo build-script change detection](https://doc.rust-lang.org/cargo/reference/build-scripts.html#change-detection),
[Cargo fingerprinting](https://doc.rust-lang.org/nightly/nightly-rustc/cargo/core/compiler/fingerprint/index.html),
and [Git metadata path resolution](https://git-scm.com/docs/git-rev-parse).

## Verification

The offline tests in `mistralrs-core/src/tests/shutdown.rs` exercise real standard
threads and Tokio workers, retained close results, admission fencing, all-engine
join, resource release ordering, and self-join rejection. Consumer tests in
`syzygy-local-llm` additionally cover cancelled waiters, disconnected HTTP
clients, native close failure, counted exclusive leases, and streaming errors.

Run `python3 scripts/test_git_revision.py` from this fork to compile the actual
build script with `rustc -D warnings` and exercise temporary normal, submodule,
linked-worktree, detached, packed/loose-ref, and source-archive layouts. It does
not compile model dependencies. The consumer build must additionally be run
twice without input changes to verify that Cargo keeps it fresh.

Run the MCP gates from this fork's workspace (its unit tests are not members of
the parent Syzygy workspace):

```sh
mbx check -p mistralrs-mcp --locked
mbx test -p mistralrs-mcp --lib --locked
mbx clippy -p mistralrs-mcp --all-targets --locked -- -D warnings
```

The MCP tests cover split UTF-8 and multiline events, notifications, response
validation, HTTP failures, the byte budget, premature EOF, and release of a
still-open HTTP response after a matching result.

Run from the Syzygy workspace using its pinned patch graph for engine and
consumer verification:

```sh
mbx test -p mistralrs-core --lib tests::shutdown::
mbx test -p mistralrs-core --lib tests::registry::
mbx check -p syzygy-local-llm -p syzygy-backend-ai
mbx test -p syzygy-local-llm --lib
mbx test -p syzygy-backend-ai --lib tests::gateway::
mbx clippy -p syzygy-local-llm -p syzygy-backend-ai --all-targets -- -D warnings
```

The engine patch is tracked under `20260915-LOCAL-LLM-RUNTIME-OWNER` and upstream
[PR #2428](https://github.com/EricLBuehler/mistral.rs/pull/2428), which was open
and unmerged when checked on 2026-10-04 (the PR's branch still targets the prior
0.9.3 baseline). The local 0.9.4 integration retains the shutdown patch because
the stable release has not incorporated it. The MCP response work is tracked under
`20260929-INVOCATION-STREAM-BOUNDARY`. The parent integration task records the
actual consumer results; source formatting alone is not completed verification.
