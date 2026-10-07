# Studio state and cost optimization, 2026-10-07

The installed renderer and server use the same API schema again.
The actual Studio window reload removes the update gate and enables the send button.
No test message was sent.

## Measured changes

CPU means processor time for the measured operation, not total Mac processor use.
These comparisons use the same inputs before and after each change.

| Operation                           |           Before |            After | Scope                                               |
| ----------------------------------- | ---------------: | ---------------: | --------------------------------------------------- |
| Snapshot projection and JSON output |    234.65 ms CPU |    171.28 ms CPU | One frozen snapshot                                 |
| Snapshot output                     | 31,124,779 bytes | 19,014,121 bytes | Four private repair fields excluded                 |
| Sidebar rule updates                |      1.983 s CPU |      1.301 s CPU | 80 updates, 267 agents, 1,825 entity rows           |
| Sidebar agent updates               |      1.978 s CPU |      1.124 s CPU | Same fixture and 80 updates                         |
| Receipt key table                   |    633.14 ms CPU |    192.62 ms CPU | 100,000 unique receipt keys, median of three trials |

The sidebar renders no unchanged row views after a rule update.
One changed agent renders one row view.
All 80 state updates reach the caller.
Current action and keyboard handlers remain available when a row view skips a render.

The cost reader builds receipt keys before the final history transaction.
It closes the main read transaction after the compact history capture.
Its temporary table uses `WITHOUT ROWID` and a 16 MiB cache limit.
Pricing, receipt keys, duplicate removal, and retry identities remain unchanged.

A replay of 67,670 actual history rows has no Claude receipt files.
That full cost replay shows no material CPU improvement (2.184 s before, 2.203 s after).
The 3.29-fold result applies only to the receipt key table.
The largest memory measurement with 300,000 keys is 62,275,584 bytes.

Normal UI sync uses entity changes.
It does not download the full snapshot for each event.
The projection excludes private repair history from the public snapshot.
The database and explicit history tools retain that history.

## Response contracts and application signature

Strict response models now accept the fields and states produced by Studio.
These include supervisor health, Claude input recovery, native model settings,
capacity and safety attempts, and completed recovery markers.
Unknown fields, invalid types, and unknown enum values remain rejected.
The mutation action enum still accepts only `review` and `compact`.

The full retained execution ledger passes the installed response models:
11,851 runs, 24,145 attempts, 449 nodes, and 33,380 effects.
The audit reads at most 128 records per batch.

The updater previously created `.studio-update.lock` after application signing.
macOS then reports an added unsigned resource.
The signer now creates that file before signing.
It preserves the inode and contents of an existing lease.
The real updater tick passes strict signature verification in both signed test versions.
The old signer fails the same check after the tick.
The certificate identity remains the same.

## Checks and live installation

- Cost tests: 92 pass. Exact replay results retain the same SHA-256 hash.
- Sidebar projection tests: 17 pass. The production browser contract passes.
- API, projection, and schema tests: 90 pass. Account and cost route tests also pass.
- Scoped Python type check: four changed contract files pass.
- UI build, TypeScript, API generation check, scoped lint, and format checks pass.
- The staged-content hook contract passes.
- Actual HTTP reads for state, desktop, diagnostics, identity, costs, limits, and entity pull return 200 and pass strict models.
- The actual sync stream emits `api-schema`, `resources`, and `token-rates` with the expected schema identity.
- All 284 original thread IDs remain present. Seven native process identities remain unchanged.
- The original state directory and database inodes remain unchanged.

The installed contract release has build
`e19bdc73fea2ad3aa24118659a3734e914680c2f4efb0c04e98ac59f682af8c8`.
Its API schema is
`fa42f5f7c8450291d259009ea235ba54898d9f033977907e2d9be88c5b35e68a`.

## Ended compaction and queue recovery

Supervisor reattachment restores an ended compaction task as `running`.
Context repair then waits for that task and excludes its type from native receipt checks.
The native rollout proves that the reported compaction finishes at 11:51 UTC.
The same actor and turn also have a completed-turn marker and a terminal execution record.

One shared helper now settles this specific task as `interrupted`.
It requires the exact actor, account, thread, epoch, item, and turn identities.
It requires a unique terminal execution record and its finish time.
It checks the saved task before writing.
Active compactions, commands, unknown receipts, and changed identities remain unchanged.
Each context check settles at most 16 tasks.

The affected suites pass 24 tests.
Both positive regressions fail against the old code.
After installation, `main-selection-direction` starts native turn
`01a11785-36de-7a03-8e21-0223f1c4adce` with no context wait or error.
Its four original event IDs are delivered to that single turn.
The task retains its original finish time.

The final narrow recovery release has build
`e90534533aac845589140ff8cd9b4bdb0ecd63df30c07b7785fdf9be10ee9242`.
It uses the same renderer and API schema.
It includes only the compaction helper, its runtime call, and the shutdown diagnostic.
The runtime overlay adds only the three reviewed lines to the installed runtime.
Separate image changes in source are excluded from this installation.
This last restart completes normally and preserves the seven native process identities.
The seven actual HTTP reads and live sync stream pass again.

## Remaining cause limits

The former 282-second transaction has no retained owner proof.
This optimization does not establish that historical owner.

One shutdown reaches Python finalization but waits for a non-daemon AnyIO thread.
The native sample proves that wait, but does not identify its Python handler.
The server port is already closed and the runtime workers have stopped.
Six private TCP fixtures exit normally under the existing SIGTERM path.
Therefore a generic idle-worker leak is not established.
The exact old backend is stopped after process, state, build, and native identity checks.
Its replacement passes the live checks above.

Eight additional dual TCP/Unix fixtures also exit normally through `codex_canvas.main`.
They use a runtime stub and do not test actual database locks or active model handlers.
The new shutdown diagnostic records code locations of remaining non-daemon threads after runtime close.
It records at most 16 threads and 32 frames per thread.
It excludes thread names, local variables, arguments, and message text.
Four diagnostic tests and two existing HTTP tests pass.
The old-source control fails because that diagnostic is absent.
The next actual failure can retain the Python handler stack without another process sampler.
