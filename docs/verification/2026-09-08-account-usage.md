# Account usage verification, 2026-09-08

## Scope

Keep limits and reset credits bound to the selected chat account. Refresh the
local daily cost estimate without repeated native requests or duplicate scans.

## Findings and changes

- A fresh browser already selected distinct live account limits before this
  change. The earlier frozen Electron display was not reproduced. Its exact
  cause remains unconfirmed.
- The live Runtime used one global limits lock. Source already used account
  locks, but the running process had not received that method. The guarded
  update installs the account lock method without replacing connections.
- The UI now checks the profile key and available native account identity.
  It rejects foreign replies and releases stalled requests after 25 seconds.
- Successful native reads have a 60-second cache per account. Errors have a
  30-second cooldown. Concurrent callers share the account lock. One timeout
  permits one retry of this read, with a 10-second timeout for each attempt.
- Fresh UI cache skips requests on chat changes. Background quota refresh runs
  only while the document is visible. Reset commands retain their existing
  receipt and confirmation rules.
- Local cost scans use a 120-second interval instead of 900 seconds. One reader
  permits one scan at a time, including after errors. The command retains its
  incremental cache and does not use the force-refresh flag.
- A repeated report does not accumulate costs or change the report timestamp.
  The separate check timestamp advances. Yesterday's total is not shown as today.
- The footer labels the cost estimate `All accounts`. The report has no reliable
  per-account dollar attribution. It is an estimate from local logs at API
  rates, not a ChatGPT invoice.

## Evidence

- Python 3.14: 13 limits-refresh contracts, 11 cost contracts, 12 reset contracts,
  and 2 guarded usage-update contracts pass.
- Account ownership contract: 6 assertions pass.
- Account browser suite passes, including delayed foreign replies, wrong-account
  rejection, timeout recovery, fresh-cache navigation, and narrow layouts.
- Limit browser suite passes on the isolated task build. It covers reset
  receipts, missing data, account scope, and delayed cost geometry.
- TypeScript and production builds pass.
- The update applied to PID 35212. All three account connection IDs remained
  unchanged. A second guarded application returned `already_applied`.
- Live cost reads increased from $122.95 to $144.78 and then $146.87 today.
  The report contained the current source day and no scan error.
- A headless browser against port 4620 displayed 96% and 81% for two actual
  chat accounts. One had one reset credit. Both correctly showed the same
  $146.87 aggregate estimate. The probe blocked non-GET application requests.

No model turn, reset redemption, worker restart, or command replay was required.
The open Electron renderer was not inspected: the computer-use native pipe
failed to start. Reload that renderer to receive the new served bundle.

## Integration

Concurrent native-error work in the main checkout was preserved with a
three-way source merge. Its Usage popover controls and the account guard both
remain. The combined build and account browser suite pass. A combined limits
run failed at a fixture chat selector while that fixture was being changed;
that run is not a pass. The combined bundle then passed the limits suite with
the unchanged isolated task fixture, including all reset and geometry cases.
The repository index and unrelated edits were not changed during integration.
