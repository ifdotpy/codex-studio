# Rust process supervisor

## Change contract

Own native child processes independently of backend connections and speak the
existing Python supervisor protocol v1. The binary only accepts an explicit
`--state` directory. It does not install a service, choose a production state,
connect to another supervisor, or change HTTP operations. Linux is the initial
target. Other platforms refuse startup until native identity/transport support
has been implemented and verified.

The pure protocol rules live in `runtime-providers::supervisor_v1`, process and
socket effects in `runtime-process::supervisor_daemon`, and the independent
`supervisor.sqlite3` journal in `runtime-store::supervisor_journal`. The runtime
application database is never opened. No provider credentials are discovered;
the caller supplies the complete child environment.

The focused checks are the existing `runtime-process` supervisor v1 client suite
with `STUDIO_SUPERVISOR_BINARY` set to the built binary, the existing Python
client contract adapter at `runtime-process/tests/python_supervisor_clients.py`,
and `runtime-services/tests/supervisor_journal.rs`.

## Running and ownership

```sh
cargo build -p studio-process-supervisor
codex-process-supervisor --state /explicit/disposable/state
```

The directory is private, the socket is mode 0600, and `supervisor.lock` is an
exclusive process lease compatible with Python. `--wait-for-lease` waits for that
lease without connecting to or signaling the current owner. Service installation
and production cutover remain separate operator actions; existing launchers
continue to select Python.

Connections perform the unchanged protocol/state-directory/backend handshake.
Only one backend identity may own connections at a time; probes do not acquire
ownership. Operator-close requests require the operator handshake and exact
recorded PID, start time, and launch signature. Backend disconnect, detach and
backend process death leave children and their journal intact.

## Durability and recovery

The caller owns handle, operation ID, launch settings and message. The supervisor
owns generation, RPC ID, event sequence, receipt time, PID/start identity and
acknowledged cursor. The operation identity is `(handle, operationId)` as in v1;
its digest excludes the caller's native RPC ID. An identical retry returns the
saved receipt and never writes stdin again. Different content is rejected.

Acceptance commits with SQLite FULL synchronous before child stdin is written.
A disconnect, timeout or supervisor death after commit leaves an accepted receipt
with an uncertain delivery outcome. No automatic resend occurs. Child input has
a bounded deadline; timeout cannot undo acceptance. The v1 `operationStatus`
action remains restricted to monitor operation IDs and exposes a response only
after the backend has acknowledged its event.

Output is appended transactionally with monotonically increasing sequence and
process generation. An output frame retains its candidate sequence across
storage retries, including lost commit confirmation. ACK removes only the
committed prefix. Reader completion is sampled before querying the next event so
an empty query cannot hide a final committed event. A full journal pauses pipe
reading; health exposes backpressure and persistence errors.

On supervisor restart, the lease is acquired before the socket or journal is
replaced. Existing Python-written journal records are read in place. For each
open handle, recovery compares its saved process start identity with the current
process and verifies that its process group belongs to that PID. Rust-created
identities contain the Linux boot ID and `/proc/<pid>/stat` start ticks, compared
exactly even when two starts fall in the same wall-clock second. A matching legacy
Python `lstart` identity and process-group leader are accepted once, then upgraded
in a FULL-sync transaction before any signal; failed commit leaves the child
untouched and recovery blocked. The first legacy acceptance inherits Python's
one-second PID-reuse ambiguity. Subsequent verification uses only boot ID and
kernel ticks; a legacy mismatch never authorizes a signal. A verified
live group receives TERM, then a bounded, reverified KILL escalation if needed.
Receipts and output survive, the old handle is closed, and fallback readiness is
recorded. Linux v1 checks the recorded root's exit; it does not prove that a
descendant which escaped the process group has exited, or permit signaling an
unverified process after the recorded root disappears. A missing identity or failed cleanup blocks recovery. Unlike the Python recovery
path, an uncertain handle remains open in the journal so another restart cannot
forget that refusal. A replacement spawn is reserved durably even when its prior
handle was already closed; the old PID identity is removed before spawning.
Recovery retains only the latest 16 audit entries per handle. An operator can
explicitly acknowledge an unknown handle with the existing `adminCloseHandle`
action and its exact saved PID, signature and start time (empty string when
missing). This atomically closes the handle and clears its recovery refusal.
A recorded live child must still pass identity verification and be stopped;
missing identity is never invented and an unidentifiable process is never
signaled. The operator is acknowledging that remaining uncertainty. Receipts
remain durable; this acknowledgement never resends input. A reused PID is
never signaled; the old handle is closed with an identity-mismatch reason. A
spawn interrupted before its identity is committed is deliberately refused as
unknown ownership. Recovery never claims that an uncertain write was not sent.

The initial cutover has explicit resource guardrails:

| Resource                   | Rust                                          | Python v1                           | Reason for a lower limit and refusal                                                                                                                                      |
| -------------------------- | --------------------------------------------- | ----------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Simultaneous connections   | 512                                           | No explicit limit                   | Bound socket threads and retained request buffers; `Supervisor connection capacity reached`.                                                                              |
| In-memory child handles    | 128                                           | No explicit limit                   | Bound pipe readers, writers and retained output; `Supervisor child capacity reached`. Existing handles may still reconnect.                                               |
| Incoming control frame     | 1 MiB                                         | 1 MiB on Windows, unlimited on Unix | Match the existing Rust client's transport bound on Unix; `Supervisor frame exceeds the size limit or is incomplete`.                                                     |
| SQLite database            | 1 GiB, or the existing size if already larger | No global cap                       | Bound receipts and orphan state without deleting them; `supervisor journal: database or disk is full` rolls back new writes. Oversized existing journals remain readable. |
| Buffered output per handle | 256 MiB                                       | 256 MiB                             | Same journal limit; reading pauses, writes report `Supervisor output journal is full; waiting for backend replay acknowledgement`.                                        |
| One stdout frame           | 256 MiB                                       | No explicit limit                   | Avoid an unlimited unterminated line; `Child stdout frame exceeds journal capacity` appears in reader health.                                                             |
| Child input deadline       | 5 seconds                                     | No explicit deadline                | A stalled pipe returns `Child input deadline exceeded; durable outcome remains unknown`; accepted input is never resent.                                                  |

Handshakes and frames in progress each have a 10-second absolute deadline;
Python v1 has no explicit Unix read deadline. A partial frame cannot extend its
deadline by trickling bytes. An idle established connection has no read deadline.
Expiry returns `Supervisor handshake or frame deadline exceeded`, releases the
connection slot and backend admission, and leaves existing children untouched.

Capacity and framing refusals never signal existing children. Native identity on
macOS and Windows transport are named startup refusals in this first cut.

## Protocol v2 proposal (not implemented)

- `nextWait(handle, generation, cursor, deadlineMs)` would wait for a committed
  event, reader completion or a bounded deadline. It must be cancellable on
  disconnect, retain v1 cursor validation and reader ordering, and advertise its
  capability only after a version negotiation. V1 `next` remains nonblocking.
- `receiptLookup(handle, generation, operationId, digest)` would inspect every
  operation category without sending child input or changing its receipt. It
  must distinguish missing, accepted, conflicting and foreign-generation
  receipts, and report that accepted does not prove delivery. The caller's
  durable operation and exact transport ticket remain bound to the receipt;
  an unknown outcome is never authorization to resend.

Both additions need a separately versioned protocol and client capability tests.
They are not aliases for v1 actions and must not alter existing action responses.
