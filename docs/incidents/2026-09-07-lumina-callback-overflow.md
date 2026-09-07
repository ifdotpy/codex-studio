# Lumina callback queue overflow

Status: OPEN. Fresh reads verify connection recovery. The original queue backlog cause remains unknown.

## Evidence

- Runtime PID: 35212, started from the project source on port 4620.
- Account: `profile-1a70871ee4be5b7a22dc`.
- Lead: `2eda0742-0ace-4fe0-b9dc-873ff3ddd59b`.
- The account log records `Codex callback queue saturated` for
  `account/rateLimits/updated`, followed by connection closure.
- One child process of the runtime, PID 6035, is defunct. Its account identity
  was not verified from inside the process. The other two child processes remain.
- The lead and `lhs-emulator` report `Codex app-server is offline`. Six other
  workers retain running state. Those database labels do not prove live execution.
- The limits endpoint returns values with recent timestamps. This is insufficient
  evidence of recovery: queued notifications used processing time as their age.

The log proves the immediate cause of closure. It does not identify the callback
or lock that caused the queue to fill. Do not close the earlier admission-latency
incident based on this evidence.

## Source fixes and checks

The notification batching and callback latency instrumentation from `389fcd1`
are present in the source. They were not activated in this running server.

The limits changes from `6e2a155` isolate reads by account. The source now also
uses the pipe receive timestamp for delayed quota notifications. Older queued
notifications cannot overwrite newer reads. Rejected stale samples remain in
analytics with their receive and processing timestamps.

`tests/limits-refresh-contract.py` covers stale notification age, read ordering,
retained analytics, account isolation, and timeout recovery.

## Recovery boundary

The existing connection code waits for accepted callbacks to drain before it
marks a connection offline. It cannot replace that connection while its callback
dispatcher still drains. Restarting the whole runtime interrupts other accounts.
No running worker command or monitor was stopped. No uncertain input was resent.

Read-only inspection through CPython 3.14 `sys.remote_exec` was rejected by macOS:
`Cannot get task port for PID 35212`. A noninteractive administrator attempt also
failed because sudo requires a password. No inspection script executed.

Resume with authorized administrator access for process inspection, or restart
Studio after the other active work ends. First inspect the callback queue and
preserve pending receipts. After recovery, verify a fresh native read and the
lead's turn state. A cached quota response alone does not establish recovery.

## Administrator inspection, 2026-09-07 18:56 UTC

The owner authorized the macOS administrator prompt. Two read-only CPython
snapshots executed in the existing runtime, PID 35212, at 18:56:39 and 18:56:42
UTC. No worker command, monitor, or runtime was stopped. No uncertain operation
was repeated.

- The Lumina account maps to PID 76543. Its stderr file independently confirms the
  account identity. This process started at 18:31:10 UTC, before this inspection.
- Both snapshots show zero queued callbacks, zero pending native requests, and no
  transport error for Lumina. Its dispatcher waits on the empty callback queue.
  The reader remains active. `offline_accounts` is empty.
- The other account's queue falls from five callbacks to zero between samples.
  Its first stack is inside `codex_work.index_item`; its next stack is idle.
  This does not establish the cause of the earlier overflow.
- The lead still records its previous failed turn. `lhs-emulator` is also failed.
  Six workers now record interruption on disconnect; their earlier running
  labels were stale. `plan-auth-discovery` is completed.

Private evidence is stored under
`~/.local/state/codex-agents/diagnostics/lumina-20260907T185639Z/`.
The snapshots contain stack locations and queue metadata, not frame locals or
message contents. Snapshot line numbers refer to the loaded code; source files
have changed since this runtime started.

The backlog drained before administrator access became available. These snapshots
cannot identify the callback or lock responsible for that backlog. Capture the
queue and stacks during a recurrence, or use the callback latency instrumentation
after its safe activation. Keep this incident open until evidence supports the
cause and the corresponding fix. Connection recovery is not completion of the
interrupted project work.

## Fresh read verification, 2026-09-07 19:02 UTC

A diagnostic thread sent `account/rateLimits/read` directly through the existing
Lumina AppServer object. It bypassed the Studio cache and did not start a model
turn. PID 76543 returned a native result in 1,653.8 ms. The private evidence file
is `native-read.json` in the directory above.

The normal Studio `/api/limits` endpoint then returned a successful update in
1,284.0 ms. Its prior sample was 1,894 seconds old; the new response timestamp
advanced. `http-read.json` preserves the timings and result metadata.

Connection recovery is verified at this time. The lead's old failed turn and the
workers' interrupted turns were not replayed. The original backlog cause and a
verified prevention fix remain open.

## Project recovery and rejected instruction, 2026-09-07 19:29 UTC

The owner reported a final answer that claimed continued development above a
failed tool group. The project was stopped: the lead was failed, no worker was
active, and eleven lead events retained uncertain delivery.

The tool receipt for `exec-6ac06ff9-1508-4be2-a1a4-808308069309` records an
`orchestration_send` to `lhs-emulator`, with `delivery=steer`. The request waited
709,411 ms before execution. Native `turn/steer` rejected it with code `-32600`,
`no active turn to steer`. The local worker transcript entry does not establish
delivery. This receipt proves rejection of this instruction, not the cause of
the queue delay or the outcome of other messages.

A new recovery instruction used the stable message identity
`studio-lumina-recovery-20260907-verified-offline`. Its receipt is delivered.
The lead started turn `01a07d56-3b4c-7973-821e-47abb83325c4` and produced fresh
source-reconciliation messages. It confirmed the rejected instruction and began
checking remaining fixes. No uncertain message was replayed. The runtime and
unrelated accounts were not restarted. Worker resumption and project completion
were not yet verified at this snapshot.

The historical dynamic-tool item still said `inProgress`, although the durable
tool result contained the native error. The transcript fallback converted it to
`interrupted` without its result. The source now recovers missing completions by
the exact account, thread, and call identity, or an unambiguous agent-scoped alias.
It preserves stored evidence and uncertainty in the response. Large results use
a bounded excerpt. This source fix is not activated in the existing runtime.

Checks: the message transcript contracts cover failed and successful receipts,
account identity, aliases, missing or ambiguous receipts, native results, and
large outputs. Request and preparation contracts pass. A read-only projection of
the actual failed call returns `failed` and the exact native rejection above.
The transcript latency check passes. The queue-delay incident remains open.

A subsequent live check confirms active native turns for `lhs-core`, `lhs-native`,
`lhs-ui`, `lhs-service`, `lhs-emulator`, and `lhs-rig`. Each records
`status=running`, `inFlight=true`, and no error. The lead also remains active.
This verifies team resumption, not task completion or a queue-delay fix.

## Repeated outage and search index bottleneck, 2026-09-07 20:45 UTC

Lumina stopped again after the earlier successful team resumption. Its log
records two callback-queue overflows for `item/agentMessage/delta`. The lead and
all implementation workers were inactive at the initial check. A new Lumina
connection, PID 54722, was already idle by the administrator snapshots.

The other active account still had 250 callbacks at the first snapshot, mostly
text fragments. Its dispatcher was inside `Runtime.records`. The second snapshot
placed that dispatcher in `WorkMixin.index_item`, at the statement
`DELETE FROM runtime_search WHERE id=?`. SQLite reports a full virtual-table scan
for this lookup because the FTS `id` column is UNINDEXED. The live index contains
34,095 records. Each text fragment repeats this scan while holding the runtime
writer lock. This establishes a concrete admission bottleneck. It does not prove
that this is the only cause of every earlier outage.

The fix stores FTS row addresses in an ordinary indexed table. Existing documents
keep their search content. Table creation and backfill use a savepoint; a failed
migration leaves no partial table. Each subsequent update uses the exact row
address in the same transaction as the document update.

An isolated 5,000-document regression measures 60,028 SQLite virtual-machine steps
for the old lookup and 199 for the complete new update. Four focused contracts
cover migration, rollback, duplicate rejection, update cost, and a guarded live
cutover. All 34 workspace contracts pass, including search persistence and private
room access. These measurements are not a live throughput benchmark.

`codex_search_index_update.apply` accepts only the exact known legacy method. It
uses the existing runtime writer lock, commits the row-address map, then replaces
only that runtime instance's index method. It does not reload modules, replace
connections, stop commands, or replay messages. The activation result and follow-up
observations are required before claiming deployment or incident resolution.

## Live index activation and team recovery

The guarded update executed in PID 35212 at 20:50:34 UTC. Its receipt reports
`applied`, 34,213 mapped records, and 204.85 ms total duration. The next read-only
consistency check found zero missing or incorrect document mappings and zero
orphan mappings. The matching source files are also installed in the application
package. No runtime, account connection, command, or monitor was restarted.

A fresh Lumina limits request succeeded in 1,023 ms. The new message
`studio-lumina-recovery-20260907-index-fix` is delivered. The lead produced a fresh
source-reconciliation response. A subsequent registry check confirms six active
workers: `lhs-core`, `lhs-native`, `lhs-service`, `lhs-rig`, `lhs-ui-review`, and
`lhs-emulator-review`. These have active native turn identities. Older uncertain
messages were not replayed. Existing stopped worker records retain their errors.

Private evidence is retained in
`~/.local/state/codex-agents/diagnostics/lumina-index-20260907T205034Z/`.
The search-index bottleneck is fixed and activated. The broader overflow incident
remains open until sustained operation or a recurrence provides enough evidence.
No claim is made that all earlier coordination delays share this cause.
