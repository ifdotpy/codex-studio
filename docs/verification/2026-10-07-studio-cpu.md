# Studio CPU use, 2026-10-07

The Studio Python backend caused the reported processor load.
Its process ID remained 3058 throughout the checks and both source updates.
The updates did not restart the backend or interrupt agents, monitors, or commands.

## Confirmed causes and changes

- Session cost reads encoded and hashed the entire price catalog repeatedly.
  The catalog contained 8,417 models and occupied 5.33 MB.
  Studio now retains the exact canonical hash with the catalog object.
  A catalog replacement invalidates that pair.
  Custom providers still receive a content check on every read.
- The disk scanner repeatedly inspected worktree entries and their APFS private sizes.
  Its background thread now uses a 15% CPU budget.
  A retained legacy timer skips a fresh, unrequested full scan.
  Requested workers and expired measurements still receive a scan.
- The history importer repeatedly searched native session directories.
  It now reads each directory once per discovery pass.
  Its 30-second path cache starts after the pass completes.
  Missing and ambiguous paths retain their existing checks.
- After that change, the history importer still used about 57% of one core.
  Repeated database scopes and import steps remained expensive.
  The importer now uses a separate 15% CPU budget.
  Each pause occurs after its database scopes close and its import guard releases.
  Foreground import calls retain their existing behavior.

## Live measurements

Each row covers a separate 30-second window under the real workload.
Total CPU use comes from the backend's cumulative `ps` CPU time.
Thread CPU use comes from Darwin `proc_pidinfo`.
100% means one processor core.

| Applied changes                         | Python total | History importer | Disk scanner | Scheduler |
| --------------------------------------- | -----------: | ---------------: | -----------: | --------: |
| Before                                  |      191.33% |           57.89% |       42.89% |    27.10% |
| Price hash, disk budget, path discovery |      118.88% |           57.27% |       13.90% |    25.07% |
| Additional history budget               |       81.80% |           13.55% |       13.58% |    29.61% |

The workload changed between windows.
These results do not prove a constant total CPU limit or an exact benefit from each individual change.
The scheduler remains a substantial source of load.
This update preserves its behavior.

Twenty price hash reads used 1,863.734 ms of CPU before the change.
After the change, the first hash used 98.982 ms; twenty cached reads used 0.020 ms in total.
Both paths produced the same full catalog hash.

A private disk check returned the same 16,777,216 bytes after 4,107 native calls in both versions.
Its CPU duty fell from about 58% to 14%.
The scan took longer because its pauses reduced the background load.
History catch-up can also take longer under its CPU budget.

## Application and preservation

The installed backend uses a different protocol from current `main`.
The updates therefore replaced only reviewed function code and narrow supporting helpers.
They preserved existing objects, globals, defaults, and native connections.

- First manifest: `9dffac684a080949ffe24adf39e83956942ba82592aa0e86391573cd1aa1e63d`.
- History manifest: `9d65e6cc41bd9daf433286021fbabf1a3a41ddb9f7d720e87b7c408442aabff8`.
- Both receipts reported `applied` on attempt 1 under the same update manager.
- The installed Runtime source retained hash `73a4cffbca4ef8ad71f90a7a224a952eae0e1ad1fdcce21ad432b17097445383`.
- All temporary manifests and sample helpers were removed after their exact receipts and outputs were saved.
- Permanent source changes remained installed.
- The final state read contained 2,186 agents and six active turns.
- Ten new orchestration events had delivery receipts after the history update.

Private evidence is in `/private/tmp/studio-cpu-20261007`, `/private/tmp/studio-cpu-repair-live-20261007`, and `/private/tmp/studio-analytics-cpu-live-20261007`.
The bounded frame samples contain file names, function names, line numbers, and thread identities.
They contain no frame variables or message content.

## Checks and limits

The four new contracts pass: price hash (8), disk CPU (3), path cache (12), and history CPU (8).
The original versions fail the corresponding causal checks.
Related history, connection reuse, worker, disk, and pricing contracts pass.
The two private update packages pass 45 guard and installed-behavior cases in total.
Independent source review found no concrete defect.

The existing cost invalidation case `test_claude_root_move_invalidates_both_roots_once` still raises its team-change retry error.
The unchanged `main` implementation reproduces that error.
The existing retry guard remains intact.

To repeat the new source checks with the prepared Python environment:

```sh
python3 -B tests/pricing-signature-cache-contract.py
python3 -B tests/worktree-disk-cpu-contract.py
python3 -B tests/analytics-rollout-path-cache-contract.py
python3 -B tests/analytics-history-cpu-budget-contract.py
```

The private fixtures do not establish live model reliability.
The measurements establish reduced CPU use in the observed live windows.
