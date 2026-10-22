# studio-runtime-process

## Change Contract

- **Responsibility:** Process boundary ports, bounded Tokio provider attachments,
  owned asynchronous pipe children, advisory owned-child priorities, and the
  typed client for supervisor v1 plus explicitly negotiated v2 event waits. This
  crate does not start the production supervisor, make HTTP requests, or access
  SQLite.
- **Public interface:** `SupervisorClient<T>` and typed request/result/event
  models for each v1 action; `provider_seam::ProviderTaskFactory`,
  `provider_task::SupervisorProviderTaskFactory`, and the Unix temporary-child adapter
  `provider_stdio::StdioProviderTaskFactory`; the `PtyHost` port;
  [`stream_child`](src/stream_child.rs) for bounded local commands and sign-in;
  `child_priority::ChildPriority` for command wrappers and
  `child_priority::priority_diagnostics()` for nonblocking failure snapshots.
- **Forbidden dependencies:** HTTP frameworks, SQLite, Python runtime
  bindings, and unsafe code. Platform scheduling calls stay in the priority adapter.
- **Invariants:** The supervisor owns retained provider and command children.
  Temporary stdio children belong to their adapter task; local pipe children belong
  to the retained stream actor. Both kill their own process group and reap on
  cancellation, deadline expiry, or handle drop. Handshake identity is
  `(backendId, stateDir)`; each child receipt
  and journal event also carries a process `generation`. A sent action whose
  response is lost is `UnknownOutcome`; callers resolve it by operation receipt
  or journal replay instead of issuing a fresh operation identity.
- **Configuration owner:** The server app supplies the supervisor endpoint,
  backend identity, state directory, and child launch data. Child priority is
  read once from `CODEX_AGENT_CHILD_PRIORITY` at server startup; see the
  [design note](../../../../docs/plans/rust-child-priority.md).
- **Focused check:** `cargo nextest run -p studio-runtime-process`.

## Process supervisor handshake contract

Source of truth: [the supervisor daemon](src/supervisor_daemon.rs) and
[its shared contract](src/supervisor_contract.rs). The wire uses
newline-delimited JSON over a Unix domain socket. The independent Rust process
retains child ownership across backend restarts. `just install` installs its
command link. The backend user service uses
`studio-runtime-server service install|status|uninstall`; those commands never
stop the supervisor or restart active work. See the
[operator commands](../../apps/runtime-server/README.md#command-and-user-service-installation).

The compatibility handshake starts with
`{protocol: PROCESS_SUPERVISOR_PROTOCOL_VERSION, stateDir, backendId}` and
replies `{ok: true}`. The version is owned by
[supervisor_contract.rs](src/supervisor_contract.rs). A wrong protocol or state
directory is rejected before requests are accepted. Operator connections use
the protocol field with `{stateDir, operator: true}`.

The Rust provider factory uses a separate v2 handshake and requires the daemon
to echo protocol 2. It does not fall back to v1. V2 `nextBatch` uses the shared
request/result/event DTOs in [`protocol.rs`](src/protocol.rs), including the
owned event-count limit. It waits on a dedicated event connection while the
primary connection continues to carry ACK and other control actions. The
existing generated frame-size contract remains the frame limit owner.

The action DTOs and client methods cover the complete v1 inventory:

| Action             | Typed method                                  | Purpose                                                  |
| ------------------ | --------------------------------------------- | -------------------------------------------------------- |
| `health`           | `health()`                                    | Recovery and durability preflight                        |
| `open`             | `open(OpenRequest)`                           | Start or resume a stable handle                          |
| `next`             | `next(CursorRequest)`                         | Read the next journal event                              |
| `finishFallback`   | `finish_fallback()`                           | Finish verified recovery                                 |
| `adminCloseHandle` | `admin_close_handle(AdminCloseHandleRequest)` | Close an exact operator-selected identity                |
| `retireCommand`    | `retire_command(RetireCommandRequest)`        | Retire a completed command                               |
| `write`            | `write(WriteRequest)`                         | Journal and send a provider frame                        |
| `operationStatus`  | `operation_status(OperationStatusRequest)`    | Recover a durable write receipt                          |
| `ack`              | `ack(AckRequest)`                             | Advance the contiguous settled cursor                    |
| `replay`           | `replay(CursorRequest)`                       | Recover retained journal events                          |
| `status`           | `status(HandleRequest)`                       | Inspect one handle                                       |
| `detach`           | `detach(HandleRequest)`                       | Release the backend attachment while retaining the child |

The event kinds are `stdout`, `stderr`, and `exit`. Each carries a monotonically
increasing per-handle sequence and the child generation; `payload` is the JSON
value recorded by the supervisor (`stdout` provider frame, `stderr` data, or
exit return code). Unknown event names and unknown fields are retained.

The supervisor has one reader thread per child stream. Event order is preserved
within stdout and within stderr, but there is no ordering guarantee between
those streams. A child write does not correspond to one event: stdout is parsed
into lines and stderr is read in chunks of up to 65,536 bytes, so writes may be
split or combined. Journal sequence records append order only. `next` returns
the first committed event after its cursor, and `replay` returns retained
events in cursor order; clients should group and aggregate stream payloads by
kind rather than infer cross-stream order. The `exit` event comes from the
stdout reader after process termination and does not alone guarantee that the
stderr reader has finished. Consumers that need complete output should also
wait for `stdoutReaderAlive` and `stderrReaderAlive` to become false.

For a handle, `sequence` is the latest journal cursor and `acknowledged` is the
durable contiguous cursor. `next` returns the first event after its cursor;
`replay` returns all retained events after its cursor. Both reject a cursor
below `acknowledged` or ahead of `sequence`. `ack` removes journal entries up to
the new acknowledged cursor. The journal uses SQLite `synchronous=FULL` for
each accepted request/event transaction (`sqlite-full-sync-per-request`); the
health response publishes this durability label. The backend may reconnect
using the same `backendId` and `stateDir`; `open` with the same handle and launch
settings resumes a live child and returns its generation. Replacing a child
advances its generation, so consumers can reject stale events.

## Native provider process adapter

`provider_task::SupervisorProviderTaskFactory` implements the bounded Tokio
attachment interface in `provider_seam`. `ProviderHandle::for_account` builds
the stable `account:<account-key>` identity. The adapter retains generation and
cursor identity; it never acknowledges an entry merely because it delivered it.
The account worker commits settlement before the session authorizes an ACK.
The old byte-stream factory and its thread/mailbox implementation were removed.
See [the Tokio attachment contract](src/provider_seam.rs) and
[the account consumer](../runtime-services/src/account_worker.rs).

Result and event models retain unrecognized fields in flattened extension maps.
A remote `error` is a typed remote failure. Connection loss or an invalid reply
after sending an action is `UnknownOutcome { action, request_id }`; callers
resolve writes through `operationStatus` and retained output through
`replay`/`next`. The disposable Rust integration suite covers every v1 action,
including the privileged operator handshake.

## Integration checks

`tests/supervisor_v1.rs` launches the Rust supervisor into a unique directory
below `std::env::temp_dir()`. The shared harness requires
`STUDIO_SUPERVISOR_BINARY`, or Cargo's built supervisor path when run from the
supervisor crate. It never falls back to an interpreter. Socket bounds, runtime
state isolation, readiness handshakes, stderr diagnostics and child cleanup stay
owned by [`tests/support`](tests/support/mod.rs).

The suite exercises every v1 action, event replay, cursor/ACK boundaries,
operator identity checks, response loss and same-child reattachment. V2 provider
tests cover initialization recovery, exact write receipts and generation-scoped
identities. Harmless `/bin/sh` children provide deterministic protocol replies;
Rust OS APIs prepare full pipes and saturated Unix socket backlogs.
[`log-rotation.json`](tests/fixtures/log-rotation.json) freezes the former log
oracle's seed and expected bytes, deserialized into a closed fixture type.

Build `cargo build -p studio-process-supervisor`, then run
`STUDIO_SUPERVISOR_BINARY="$CARGO_TARGET_DIR/debug/codex-process-supervisor" cargo nextest run -p studio-runtime-process`.
The focused tests require no Python executable or backend source tree. The lead
runs the repository's integrated fast and comprehensive gates.

## OS ports

`PtyHost` follows terminal session creation, input, output, resize, exit, and
close operations in [`terminal_pty`](../runtime-services/src/terminal_pty.rs).
The intended adapter is `portable-pty` 0.9.0 (MIT); before enabling it, verify
spawn/input/output/resize/exit and descendant cleanup on Linux, macOS, and each
supported Windows/WSL entry point, including packaged server behavior.

The crate uses workspace `serde`, `serde_json`, and `thiserror` (`=2.0.17`,
MIT OR Apache-2.0); this slice introduces no new dependency version. The
approved transport is `std::os::unix::net::UnixStream` behind the generic
`Read + Write` seam. Windows named-pipe transport is an explicit implementation
gap because the Windows server entry-point/release promise is not yet frozen.
For that decision, evaluate [`interprocess` 2.4.5](https://crates.io/crates/interprocess)
(0BSD OR Apache-2.0; latest release as of 2026-10-12; active upstream with
recent commits and explicit Windows/Linux/macOS support) against
the existing pipe endpoint identity/security contract before adding it. It is
not currently in the dependency graph.

## Provider task seam (stages 2 and 3)

`ProviderTaskFactory::open(handle, OpenOptions, Shutdown) -> OpenFuture` is
object-safe as `Arc<dyn ProviderTaskFactory>` without async-trait. The caller
resolves launch configuration outside the executor and validates session recovery
before opening. The session validates its actual prepared initialize command;
the factory validates launch configuration and wire size, then returns
`ProviderAttachment` before connecting. Use `ProviderHandle::for_account` for the
one supervisor-owned child; the account registry still owns attachment admission.

The attachment exposes bounded `commands` and `journal` channels, a `status`
watch, and cancellation-safe `completion()` / `shutdown()` futures. Both channels
hold 16 entries with a 1 MiB serialized upper bound per entry (plus proportional
JSON allocation overhead). One outstanding RPC and at most 32 numeric response
mappings are retained. Startup replay uses the same channel and no deferred queue.
A receive slot is reserved before reading events. The legacy v1 provider path
uses immediate `next` calls with a cancellable 50 ms pause after an empty result.
The production Rust provider factory negotiates v2 and waits for `nextBatch` on
its separate event connection; it does not periodically query an idle journal.
Write/ACK receipts and shutdown never depend on journal capacity.

`JournalEntry { cursor, generation, received_at_ms, payload }` distinguishes `Frame`, `RawStdout`,
`Stderr`, `Exit`, and future supervisor event kinds. `PreparedWrite::new` validates
a frame before command admission. `AdapterCommand::write` and `::ack` return a
command and `Receipt`; `Receipt::receive` distinguishes `Written`,
`Acknowledged`, `CachedInitialize`, `NotSent`, and `Unknown`. Write operation IDs
belong to the caller. Use `initialized_operation(&handle, generation)` for the shared
Python-compatible initialized identity. A lost receipt terminates the attachment;
there is no reconnect, flush, or automatic resend surface. A closed receipt is
Unknown. Successful write receipts have an observation handshake: the adapter
waits for Receipt.receive or shutdown before serving the next command; abandoning
a buffered receipt or its receive future closes the attachment. The supervisor may have committed a write before returning an error, so
remote write errors are conservatively Unknown too.

The session stage retains its single `SessionState` and owns initialize and
initialized commands. Feed a `CachedInitialize` control receipt back to that state
with the local initialize ticket; it is not a journal entry to ACK. Observe
`InitializedWritePending` after the reply and `Ready` only after the real
initialized write receipt, then call `mark_initialized_notification_written`.
Use `SessionState::next_deadline_ms()` as the session's deadline authority.
Process startup replay while waiting for readiness instead of draining it into
another queue. Commit a contiguous durable prefix before sending ACK; the adapter
never sends ACK itself, including for the launch spec's recovered handled cursor.
Terminal EOF stops event waits but keeps ACK admission until the final delivered
prefix settles or the caller explicitly shuts down.

`TaskExit::recovery_needed` identifies incomplete initialization separately from
socket cleanup, including a generation when known. Persist it before declaring
protocol cleanup complete. A verified resumed child without cached initialization
returns `AttachmentStatus::initialization_recovery()` and can complete the same
handshake; it does not fail with generic Launch. I/O loss cannot prove protocol
completion. Retained-child environment checks run in at most four owned blocking
jobs per factory; shutdown reaps an already-started job. Linux `/proc` identity
verification is required; other Unix platforms refuse reattach before open.

The old thread adapter and session remain available, with the shared initialized
identity correction applying to both adapters. Independent review and caller
migration are required before cutover.
These stages add no production caller wiring or live-model verification. Remove
the old byte-stream workers only when those callers have migrated.

Environment reads that have not started are aborted on shutdown. Shutdown still joins
the cancelled blocking job (Tokio may first wait for a pool worker) or an already
started, at-most-4-MiB OS read; there is no detached job or hard OS-read deadline.

## Temporary stdio attachment

[`StdioProviderTaskFactory`](src/provider_stdio.rs) implements the same bounded
async seam for ephemeral exchanges, without supervisor state or replay. The
caller supplies an absolute Tokio deadline and raw frame/total-output bounds.
The task concurrently reads stdout, drains stderr, and writes commands; the
single deadline also interrupts stalled writes. Cleanup runs before completion
or status-channel closure. This adapter requires Unix process groups.

The model catalog consumes it through the accepted session task. Its cache is
process-local, so consumed entries are settled without a durable journal.
Run `cargo nextest run -p studio-runtime-process --test integration -E 'test(/^provider_stdio::/)'` for
cancellation, deadline, caller-drop, and process-group cleanup checks.

## Owned local pipe children

[`StreamChild`](src/stream_child.rs) uses Tokio and process-wrap process groups;
process-wrap's kill-on-drop feature stays disabled. Its final command wrapper
applies the configured child priority inside the owner thread before returning
the child; see the [priority design note](../../../../docs/plans/rust-child-priority.md). Each child has an owner thread
with a private Tokio runtime. The owner awaits process-group termination, reaping,
and pipe closure before destroying that runtime; handle drop signals and joins it.
`close()` exposes cleanup errors. Cleanup therefore survives application runtime
shutdown. These children cannot be recovered by saved PID after a backend restart.
An optional inherited lease remains in the child until its descriptors close.

`ChildSpec` supplies the complete environment after `env_clear`. A sign-in caller
selects its isolated configuration home and CLI search path; a monitor caller
supplies the environment selected by workspace preparation and permission checks.
This leaf neither reads provider homes nor copies the ambient environment. It
never includes arguments, environment values, or captured output in errors.

`next_event` streams bounded chunks with order preserved within each pipe.
`write` distinguishes a request that was never admitted from an unknown submitted
write; unknown input must not be replayed. `capture` uses the same actor for a
bounded tail of each pipe. Buffer and deadline limits are owned by
[`stream_child.rs`](src/stream_child.rs). Production provider commands still use
the account worker's native command port and sandbox policy.

## Blocking-pool measurements

Measurements are disabled unless `CODEX_RUNTIME_BLOCKING_POOL_DIAGNOSTICS=1` is
set before server startup. With it enabled, read `blockingPoolMetrics` from
`GET /api/diagnostics`; it reports queued jobs, running wrapped closures, peak
in-flight occupancy (queued plus running), and per-call-group queue-wait and
closure-duration averages and maxima. To measure the fixture without a backend, run
`cargo test -p studio-runtime-process fixture_reports_pool_wait_duration_and_occupancy -- --nocapture`.
The fixture holds one worker at a channel gate, queues one closure, releases it,
and prints its snapshot. Compare queue wait and occupancy across representative
fixture workloads before deciding whether to introduce a store actor.
