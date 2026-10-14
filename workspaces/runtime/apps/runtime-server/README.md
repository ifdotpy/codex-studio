# `studio-runtime-server`

## Change Contract

- **Responsibility:** own runtime-server process startup, loopback listener,
  backend URL announcement, bounded provider-history import scheduling, and
  graceful signal shutdown.
- **Public interface:** `studio-runtime-server --port <port>`; port `0` requests
  an ephemeral loopback port for the existing HTTP harness. The maintainer
  diagnostic `studio-runtime-server state-check <state-dir>` validates typed
  records in an existing state directory.
- **Forbidden dependencies:** Python production runtime and direct route logic.
- **Invariants:** bind only to loopback; preserve the selected runtime-store
  state directory; use the HTTP crate's frozen schema and middleware contract;
  run history imports in one Tokio task through the measured blocking pool.
- **Configuration owner:** launcher environment and the selected state
  directory; history import resolves provider profiles through AccountDriver.
- **Focused check:** `cargo nextest run -p studio-runtime-server`.

The binary owns process configuration and signal handling. Route behavior lives
in `studio-runtime-http`; Rust store, process, and provider crates are composed
by the application. Provider history collection uses the bounded service in
`studio-runtime-services::analytics_history`; runtime-server owns its Tokio
task and supplies account profile resolution and durable budget capture.

The server uses Axum 0.8.9 (MIT) and getrandom 0.3.4 (MIT OR Apache-2.0) for
loopback HTTP and its per-process session token. Tokio 1.53.2 (MIT) supplies the
async listener and signal handling.

`state-check` opens `<state-dir>/canvas.sqlite3` strictly read-only and prints a
JSON report with the row and rejected-row counts for each typed record table.
Rejected rows are grouped by the first JSON pointer and validation keyword;
record contents and IDs are never printed. A missing typed table is reported
with zero rows.
