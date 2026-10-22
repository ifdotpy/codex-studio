# Rust process supervisor

## Change contract

Own native child processes independently of backend connections and accept only
the explicitly negotiated supervisor v2 protocol. The binary only accepts an explicit
`--state` directory. It does not install a service, choose a production state,
connect to another supervisor, or change HTTP operations. Linux is the initial
target. Other platforms refuse startup until native identity/transport support
has been implemented and verified.

The pure protocol rules live in `runtime-providers::supervisor_protocol`, process and
socket effects in `runtime-process::supervisor_daemon`, and the independent
`supervisor.sqlite3` journal in `runtime-store::supervisor_journal`. The runtime
application database is never opened. No provider credentials are discovered;
the caller supplies the complete child environment.

The focused checks are the Rust client, receipt-loss, recovery, reattach and
failed-start integration tests in this crate, plus the `runtime-process` suite
with `STUDIO_SUPERVISOR_BINARY` set to the built binary.

## Running and ownership

```sh
cargo build -p studio-process-supervisor
codex-process-supervisor --state /explicit/disposable/state
```

The directory is private, the socket is mode 0600, and `supervisor.lock` is an
exclusive process lease. `--wait-for-lease` waits for that
lease without connecting to or signaling the current owner. Service installation and production
cutover remain separate operator actions.

All clients, including the retained VM guest and operator tools, request the
version owned by [`supervisor_protocol.rs`](../../packages/runtime-providers/src/supervisor_protocol.rs)
and require the daemon to echo it. Other versions are refused before backend admission. V2 `nextBatch`
waits for a committed event or terminal reader completion on a separate
same-backend connection, leaving the control connection available for ACK and
other actions. Its ordered event payloads are raw JSON and batches are bounded
by the shared limit in [`runtime-process/src/protocol.rs`](../../packages/runtime-process/src/protocol.rs).
The frame-size limit is consumed through the
[`runtime-process` supervisor contract](../../packages/runtime-process/src/supervisor_contract.rs). A
disconnected event-wait connection does not stop the child. Only one backend
identity may own connections at a time; probes do not acquire ownership.
Operator-close requests require the operator handshake and exact
recorded PID, start time, and launch signature. Backend disconnect, detach and
backend process death leave children and their journal intact.

## Durability and recovery

The caller owns handle, operation ID, launch settings and message. The supervisor
owns generation, RPC ID, event sequence, receipt time, PID/start identity and
acknowledged cursor. The operation identity is `(handle, operationId)`;
its digest excludes the caller's native RPC ID. An identical retry returns the
saved receipt and never writes stdin again. Different content is rejected.

Acceptance commits with SQLite FULL synchronous before child stdin is written.
A disconnect, timeout or supervisor death after commit leaves an accepted receipt
with an uncertain delivery outcome. No automatic resend occurs. Child input has
a bounded deadline; timeout cannot undo acceptance. The `operationStatus`
action remains restricted to monitor operation IDs and exposes a response only
after the backend has acknowledged its event.

Output is appended transactionally with monotonically increasing sequence and
process generation. An output frame retains its candidate sequence across
storage retries, including lost commit confirmation. ACK removes only the
committed prefix. Reader completion is sampled before querying the next event so
an empty query cannot hide a final committed event. A full journal pauses pipe
reading; health exposes backpressure and persistence errors.

On supervisor restart, the lease is acquired before the socket or journal is
replaced. For each open handle, recovery compares the saved native process start
identity with the current process and verifies that its process group belongs
to that PID. Identities include the boot identity and exact kernel start time.
A mismatched or retired Python identity never authorizes a signal or a durable
identity rewrite. A verified
live group receives TERM, then a bounded, reverified KILL escalation if needed.
Receipts and output survive, the old handle is closed, and fallback readiness is
recorded. Recovery checks the recorded root's exit; it does not prove that a
descendant which escaped the process group has exited, or permit signaling an
unverified process after the recorded root disappears. A missing identity or failed cleanup blocks recovery. An uncertain handle remains open in the journal so another restart cannot
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

Resource limits belong to the shared protocol, journal and daemon owners linked
above. Capacity and framing refusals leave existing children untouched. An idle
established connection has no frame deadline; a started frame has a bounded
absolute deadline that trickled bytes cannot extend.

## Protocol and launch identity

Only v2 is accepted. `nextBatch` validates generation and cursor, returns ordered
committed events without advancing the acknowledged cursor, and waits for events
or terminal completion. Its separate connection can be cancelled without
stopping the child or control connection. Existing v2 control actions retain
their response shapes.

Reattachment requires the configured current command, environment and working
directory to match the running child's launch signature. There is no list of
older accepted launch commands. A changed signature is refused before another
child can start.

## Focused tests

`cargo test -p studio-process-supervisor` uses Cargo's built Rust supervisor.
Each fixture creates disposable state, launches harmless shell children and
cleans up its owned processes. Recovery setup uses `SupervisorJournal` to prove that exact native identities
permit cleanup while mismatched and retired identities never permit a signal. Failed-start
checks read typed journal rows. No Python backend or Python test client runs.
