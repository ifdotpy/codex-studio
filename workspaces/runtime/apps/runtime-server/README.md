# `studio-runtime-server`

## Change Contract

- **Responsibility:** own runtime-server process startup, loopback listener,
  backend URL announcement, built web asset serving, bounded provider-history
  import scheduling, and graceful signal shutdown.
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

The binary owns process configuration and signal handling. API route behavior
lives in `studio-runtime-http`; `web_assets` serves the built renderer from the
repository's `workspaces/client/apps/web/dist` directory or the packaged
workspace's `web/dist` directory. Build the renderer with
`pnpm --filter codex-agents-web run build`. Rust store, process, and provider
crates are composed by the application. Provider history collection uses the
bounded service in `studio-runtime-services::analytics_history`;
runtime-server owns its Tokio task and supplies account profile resolution and
durable budget capture.

Monitor startup recovery runs once after the store opens and before routes or
runtime admissions start. The retained monitor task handles queued results,
rule events, and durable deadlines; its first tick does not repeat startup
recovery after new monitor work may have been admitted.

The server opens one `RuntimeStore`; its Canvas writer is a Tokio-owned SQLite
actor with bounded admission. The writer limit is owned by
[`MAX_PENDING_WRITES`](../../packages/runtime-store/src/sqlite_actor.rs).
The four-connection Canvas reader cap, including the dedicated version reader,
is owned by
[`CANVAS_READER_CONNECTION_CAPACITY`](../../packages/runtime-store/src/lib.rs).
Analytics remains a separate database and writer. See the [SQLite actor
migration plan](../../../../docs/plans/rust-sqlite-actor.md) for cancellation,
receipt, and shutdown boundaries.

Background work is signaled by committed changes and durable deadlines, with
bounded retry backoff after failures:

- Continuation, monitor, and federation schedulers wait for their earliest
  durable deadline and relevant change or commit signals. A turn held by the
  account auth gate has no deadline: it is released after an account or cached
  auth status change
  ([`account_auth_gate.rs`](../../packages/runtime-services/src/account_auth_gate.rs)).
  Active VM Claude credential security refresh has a separate cadence owned by
  [`VM_CREDENTIAL_REFRESH_INTERVAL`](../../packages/runtime-services/src/account_worker.rs).
- The peer-link owner reacts to targeted `runtime_access_clients` invalidation
  hints and child exit. It rereads authoritative peer rows, keeps a running
  child in sync, restarts it after a failed peer update, and uses bounded
  backoff after failed snapshots, failed starts, or child exit. A table hint may
  follow an ambiguous commit; it triggers a reread and never replays the
  mutation.
- Provider-version checks react to active account/session profile changes and
  the `checkProviderUpdates` preference. The upstream release cache TTL is
  owned by
  [`UPSTREAM_TTL_SECONDS`](../../packages/runtime-services/src/provider_versions.rs)
  and expires after 24 hours. This cache TTL does not control credential
  refresh.

On a shutdown signal, the server closes HTTP mutation admission, drains admitted
mutations and durable account login/cancellation work while their dependencies
remain live, then stops those dependencies. The existing 20-second
`SERVER_SHUTDOWN_GRACE` watchdog bounds the sequence; graceful shutdown does not
promise that every operation finishes before that deadline. If the watchdog
forces process exit, existing durable intents, receipts, unknown outcomes, and
recovery behavior remain authoritative. Shutdown adds no automatic request
replay.

Most read-only HTTP requests remain request-scoped. Cold entity pulls are a
narrow exception: when they must maintain entity projections, the tracked
operation retains the projection lock through commit and cache marking after
the client disconnects.

The server uses Axum 0.8.9 (MIT) and getrandom 0.3.4 (MIT OR Apache-2.0) for
loopback HTTP and its per-process session token. Tokio 1.53.2 (MIT) supplies the
async listener and signal handling. The async executor defaults to at most four
worker threads, capped by the available CPU parallelism; setting
`TOKIO_WORKER_THREADS` leaves the worker count to Tokio's environment-variable
handling. This setting controls async executor workers only; it does not cap
blocking-pool threads or in-flight provider work.

`state-check` opens `<state-dir>/canvas.sqlite3` strictly read-only and prints a
JSON report with the row and rejected-row counts for each typed record table.
Rejected rows are grouped by the first JSON pointer and validation keyword;
record contents and IDs are never printed. A missing typed table is reported
with zero rows.

For an on-demand allocator snapshot, set `CODEX_CANVAS_STARTUP_MEMORY=1` before
starting the server, then request `GET /api/diagnostics`. The request writes one
`RUST_ALLOCATOR_MEMORY` event to stderr with current RSS, the recorded startup
memory marks, and compact GNU libc aggregate counters on Linux with glibc.
Other targets report the allocator data as unavailable. Raw allocator XML is
parsed in memory and is never logged. The HTTP response and runtime state are
unchanged, and no periodic memory sampling is started. The reported
allocator-used estimate is not an exact count of live allocations; process RSS
also includes memory outside GNU libc's allocator.
