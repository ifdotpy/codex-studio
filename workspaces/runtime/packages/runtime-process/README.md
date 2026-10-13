# studio-runtime-process

## Change Contract

- **Responsibility:** Process boundary ports, bounded Tokio provider attachments, and the typed client
  for the existing supervisor v1 protocol. This crate does not start the
  production supervisor, make HTTP requests, or access SQLite.
- **Public interface:** `SupervisorClient<T>` and typed request/result/event
  models for each v1 action; `provider_seam::ProviderTaskFactory` and
  `provider_task::SupervisorProviderTaskFactory`; the Unix temporary-child adapter
  `provider_stdio::StdioProviderTaskFactory`; `PtyHost` and `ProgressWatch` ports.
- **Forbidden dependencies:** HTTP frameworks, SQLite, Python runtime
  bindings, and OS-specific code outside transport adapters.
- **Invariants:** The supervisor owns retained provider and command children. Temporary
  stdio children belong to their adapter task until their process group is stopped
  and reaped, including cancellation, deadline expiry, and caller drop. Handshake identity is `(backendId, stateDir)`; each child receipt
  and journal event also carries a process `generation`. A sent action whose
  response is lost is `UnknownOutcome`; callers resolve it by operation receipt
  or journal replay instead of issuing a fresh operation identity.
- **Configuration owner:** The server app supplies the supervisor endpoint,
  backend identity, state directory, and child launch data.
- **Focused check:** `cargo nextest run -p studio-runtime-process`.

## Supervisor v1 contract

Source of truth: [`Supervisor.handle`](../../../apps/server/src/codex_process_supervisor.py#L663),
the connection handshake and frame loop in the same module, and its Python
caller [`ProcessProxy`](../../../apps/server/src/codex_process_supervisor.py#L1468).
The wire uses newline-delimited JSON over a Unix domain socket (Windows named
pipe in the Python implementation). The supervisor is a separate long-lived
process managed by [`codex_supervisor_service.py`](../../../apps/server/src/codex_supervisor_service.py#L232),
so it retains child ownership across backend restarts. The first client frame is
`{protocol: 1, stateDir, backendId}`; the server replies `{ok: true}`. A wrong
protocol or state directory is rejected with an `error` frame before requests
are accepted. Operator CLI connections use `{protocol: 1, stateDir, operator: true}`.

The action DTOs and methods in `lib.rs` cover the complete `Supervisor.handle`
inventory:

| Action             | Typed method                                  | Python backend use                                                                                                                                             | Rust test coverage |
| ------------------ | --------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------ |
| `health`           | `health()`                                    | `status(root)` performs the preflight health exchange in `codex_process_supervisor.py:1743-1760`; health result includes recovery and durability.              | Integration        |
| `open`             | `open(OpenRequest)`                           | `ProcessProxy.__init__` attaches/starts its stable handle at `codex_process_supervisor.py:1495`; receipt includes `generation`, PID, resume flag, and cursors. | Integration        |
| `next`             | `next(CursorRequest)`                         | `ProcessProxy.next_event` reads the next journal entry at `codex_process_supervisor.py:1534`.                                                                  | Integration        |
| `finishFallback`   | `finish_fallback()`                           | Recovery CLI finishes a verified fallback at `codex_process_supervisor.py:1818-1830`.                                                                          | Integration        |
| `adminCloseHandle` | `admin_close_handle(AdminCloseHandleRequest)` | Operator-only CLI closes a handle after PID, start time, and launch signature verification at `codex_process_supervisor.py:1795-1815`.                         | Integration        |
| `retireCommand`    | `retire_command(RetireCommandRequest)`        | Completed `server-command:` handles are retired by `retire_command` at `codex_process_supervisor.py:1763-1793`.                                                | Integration        |
| `write`            | `write(WriteRequest)`                         | `ProcessProxy.send_write` journals and writes the provider RPC at `codex_process_supervisor.py:1594`.                                                          | Integration        |
| `operationStatus`  | `operation_status(OperationStatusRequest)`    | Monitor restart recovery checks the durable receipt in `codex_runtime.py:3846`.                                                                                | Integration        |
| `ack`              | `ack(AckRequest)`                             | `ProcessProxy.ack` advances the contiguous cursor after runtime persistence at `codex_process_supervisor.py:1627`.                                             | Integration        |
| `replay`           | `replay(CursorRequest)`                       | Exposed journal recovery action; current `ProcessProxy` consumes the same journal incrementally through `next`, and does not call `replay` directly.           | Integration        |
| `status`           | `status(HandleRequest)`                       | Exposed per-handle status action; the current runtime uses local health/preflight snapshots instead of calling it.                                             | Integration        |
| `detach`           | `detach(HandleRequest)`                       | `ProcessProxy.detach` sends the detach action during backend shutdown at `codex_process_supervisor.py:1697`; it leaves the child owned by the supervisor.      | Integration        |

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

Approved plan decision D-01 moves the production backend, supervisor, and CLI
to Rust together; M-07 freezes supervisor compatibility before cutover. Until
that stage, the Python supervisor remains the owner and this slice supplies only
the backend-side client and provider process adapter. See
[`rust-workspaces.md`](../../../../docs/plans/rust-workspaces.md#decisions-and-consequences).

## Native provider process adapter

`provider_task::SupervisorProviderTaskFactory` implements the bounded Tokio
attachment interface in `provider_seam`. `ProviderHandle::for_account` builds
the stable `account:<account-key>` identity. The adapter retains generation and
cursor identity; it never acknowledges an entry merely because it delivered it.
The account worker commits settlement before the session authorizes an ACK.
The old byte-stream factory and its thread/mailbox implementation were removed.
See [the Tokio attachment contract](src/provider_seam.rs) and
[the account consumer](../runtime-services/src/account_worker.rs).

The Python client reads JSON objects without closed-schema validation: unknown
request fields are ignored by action handlers and unknown response/event fields
survive its dictionary handling. Rust result/event models therefore retain
unrecognized fields in flattened extension maps. A remote `error` is a typed
remote failure. A connection loss or invalid/missing response after sending an
action is `UnknownOutcome { action, request_id }`; the Python monitor recovery
path resolves operation writes through `operationStatus` and retained output
through `replay`/`next` (`codex_runtime.py:3846`,
`codex_process_supervisor.py:1534-1627`).

The existing Python backend-neutral fixture covers `health`, `open`, and `next`
(3 of 12 actions). This crate's disposable-socket integration test covers all
12 actions, including the privileged operator handshake. It is a separate
Rust fixture and leaves the shared Python fixture untouched.

## Integration checks

`tests/supervisor_v1.rs` launches the Python supervisor from this checkout into
a unique directory below `std::env::temp_dir()` (the test runner's `TMPDIR`),
checks that the socket stays inside that test-created directory and outside the
runtime's default state directory, and checks the encoded socket path against
the platform AF_UNIX length limit before startup. If the supervisor exits
before binding, the test reports its status and captured stderr; the timeout is
only a bounded hang guard. It then exercises every action, event kind,
bad-protocol handshake, cursor/ACK,
replay, same-identity reattach, and cleanup of the disposable supervisor and
children. It uses no fixture from the Python backend-neutral suite. Pure frame
codec and extension-field tests run without a process.

The same disposable supervisor suite also drives the checked-out
`runtime-services` provider-session implementation through the adapter with a
harmless fake app-server. It checks request/reply exchange, native-account
reattach without replaying initialization, child exit, and detach ownership.

This is suitable for the Rust fast tier when its single run is at most 10 s:
it starts only the required local supervisor and harmless test children, uses
blocking request/response reads plus immediate `next` protocol calls, and has a
bounded hang guard without sleeps. It finds Python by running `python3` from
`PATH` and reading `sys.executable`; it finds the supervisor script relative
to Cargo's compile-time `CARGO_MANIFEST_DIR` in the checked-out repository. A
missing interpreter or script fails the test with an explicit prerequisite
message; it never skips. The hosted `Fast tier: remaining checks` job installs
Python 3.14 through `.github/actions/fast-tier-setup/action.yml` and runs
workspace nextest through `just test-fast-rest`, which includes this test. The
separate hosted `Rust checks` job runs fmt, clippy, and doctests but does not
run nextest and does not currently install Python. If nextest is added to that
job, it must first provide Python 3. The complete suite selection should
remain `cargo nextest run -p studio-runtime-process`; do not split this
scenario into an independent slow inventory unless its measured run exceeds
the repository's fast-tier limit.

## OS ports

`PtyHost` follows terminal session creation, input, output, resize, exit, and
close operations in [`TerminalManager`](../../../apps/server/src/codex_terminals.py#L146).
The intended adapter is `portable-pty` 0.9.0 (MIT); before enabling it, verify
spawn/input/output/resize/exit and descendant cleanup on Linux, macOS, and each
supported Windows/WSL entry point, including packaged server behavior.

`ProgressWatch` follows the subscribe-before-baseline, coalesced notification,
and unsubscribe lifecycle in [`ProgressFileWatchdog`](../../../apps/server/src/codex_progress_watch.py#L86).
The intended adapter is `notify` 8.2.0 (CC0-1.0); verify exact-file detection,
atomic replacement, teardown, and callback ordering on Linux inotify and macOS
kqueue. Establish the Windows/WSL watcher requirement before promising support.

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
A receive slot is reserved before issuing `next`. Write/ACK receipts and shutdown
never depend on journal capacity. The sole 50 ms empty-`next` pause is cancellable.

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
Terminal EOF stops next queries but keeps ACK admission until the final delivered
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
Run `cargo nextest run -p studio-runtime-process --test provider_stdio` for
cancellation, deadline, caller-drop, and process-group cleanup checks.
