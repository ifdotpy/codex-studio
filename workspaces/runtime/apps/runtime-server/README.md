# `studio-runtime-server`

## Change Contract

- **Responsibility:** own runtime-server process startup, loopback listener,
  backend URL announcement, and graceful signal shutdown.
- **Public interface:** `studio-runtime-server --port <port>`; port `0` requests
  an ephemeral loopback port for the existing HTTP harness.
- **Forbidden dependencies:** Python production runtime, direct route logic,
  and SQLite/provider implementation details.
- **Invariants:** bind only to loopback; never claim persistent state behavior;
  use the HTTP crate's frozen schema and middleware contract.
- **Configuration owner:** launcher environment; the selected state directory
  is only reported by this skeleton until runtime-store is connected.
- **Focused check:** `cargo nextest run -p studio-runtime-server`.

The binary owns process configuration and signal handling. Route behavior lives
in `studio-runtime-http`; Rust store, process, and provider crates are wired by
the application as those implementations land. The current read-only routes are
`GET /api/session`, `GET /api/sync/identity`, and `GET /api/desktop`; every other
documented operation returns 501 with the common error envelope.

The server uses Axum 0.8.9 (MIT) and getrandom 0.3.4 (MIT OR Apache-2.0) for
loopback HTTP and its per-process session token. Tokio 1.53.2 (MIT) supplies the
async listener and signal handling.
