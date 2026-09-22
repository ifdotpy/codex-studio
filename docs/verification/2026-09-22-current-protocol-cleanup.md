# Current protocol cleanup

Baseline: `45fd097cbd27bd26a8ef6442212c0fb4def78a08`.

The owner requested removal of backward compatibility and an update of the
installed application. Active agents must continue.

## Scope

- Removed old tool wrappers, aliases, native v1 approvals, HTTP fallbacks,
  startup migrations, structured panels, and 50 historical update modules.
- The browser requires current session, sync, and message receipt protocols.
- Command receipts retain their identities, ownership, and uncertain outcomes.
  Deployment converts saved receipts once. No command is replayed.
- Historical transcripts, native session logs, expenses, and current drafts
  remain available. Historical data readers are not retired API adapters.
- The cost scanner no longer reads unused report formats or old cache producers.
  A stale cache causes a new scan of its source logs.
- Deployment converters remain test fixtures. Release patches are temporary
  files outside the maintained application source.

## Native tool catalogs

Catalog updates wait for account work, callbacks, commands, and native queues.
Existing sessions continue while another agent keeps the account busy. Routine
delays do not show a wait error. The update does not call a model.

A replacement header of equal or smaller size keeps its original byte length.
This preserves byte offsets used by descendant sessions. A larger catalog uses
one native fork. Studio retains the original session and changes its reference
only after the destination receipt and metadata update succeed.

Lost fork responses remain uncertain. They do not cause a second fork. A late
response can settle the original request. Runtime shutdown waits for metadata
writers before releasing the database lease.

## Verification

Targeted checks passed:

| Check | Result |
| --- | --- |
| Native catalog contracts | 35 passed |
| Installed native catalog tests | 2 passed |
| Metadata writer shutdown | 2 passed |
| Runtime contracts | 50 passed |
| Workspace contracts | 38 passed |
| Context repair / waits | 24 / 25 passed |
| Native review contracts / runtime / installed native | 14 / 15 / 2 passed |
| Account transfer / settings / cross-provider transfer | 37 / 9 / 19 passed |
| Swift cost scanner / account costs / cost reader | 5 / 6 / 12 passed |
| Production frontend build | Passed |
| Hidden packaged application | Passed |
| Python syntax | 225 files passed |

Browser checks cover session refresh, drafts, sync, message delivery, queued
input, complaint ownership, settings, voice, and native errors.

The two installed-native catalog tests use a loopback provider. They verify
preserved source bytes, recovery of an existing descendant, full prior context,
and current tools. Migration makes zero model requests and zero cloud requests.

The packaged application opens with an isolated state directory and a hidden
window. The test verifies role skills, the speech helper, packaged executable,
and backend persistence after the window closes.

Independent review covered receipts, native history, account reservations,
shutdown, cost scanning, and the temporary deployment transition.

## Known unrelated failure

`tests/workspace-layout-ui.mjs` fails its mobile transcript height requirement:
`377.640625px`, required `405.12px`. The exact baseline and changed application
have identical measurements at all four tested widths. No product CSS changed.
The test threshold remains unchanged.

## Deployment

The initial update correctly rejected an unexpected live `make_server` function.
A read-only audit found one mismatch across the release function set. Its exact
bytecode matches commit `236776ae6b1bb1951032134402e92dbf7e31bdae`.
The release accepts this exact outer function. Nested HTTP handlers retain
their independent baseline and current-code checks.

The live update applied to backend PID `55850` on its first attempt after the
exact historical source check was added. Four active turns retained their native
IDs and epochs. One turn finished and queued its continuation. All six native
child processes remained alive. The removed panel callback returned HTTP 404.

The transaction converted 4,191 saved receipts and normalized 302 tool names.
It retained 113 receipts with unknown ownership and converted 277 budget records.
Exact changed rows were saved in a private deployment backup.

The owner then authorized forced interruption and requested an application start.
Maintenance reconciled saved native turns before changing thread references.
A stale completed-transfer pointer on one deleted agent was removed under the
exclusive runtime lease. Historical transfer rows remain unchanged. They cannot
block catalog updates without current ownership or native mutation evidence.

Maintenance verified all 197 nondeleted Codex thread catalogs across four accounts:
118, 5, 69, and 5 threads. It changed 195 rollout headers; two catalogs were
already current. No account or thread remained pending in the maintenance report.

Studio launched from `/Applications/Codex Studio.app`. New backend PID: `61997`.
Its startup source hash matched the installed source:
`92af58f5a2eff6f5e95e354e8e142c303ef0261c32e154a49e9c9fc225b6851f`.
The renderer reported healthy after 60 seconds. Background recovery was restored.

All 58 records queued before maintenance remain: 37 delivered and 21 pending
at the verification snapshot. No queue record was lost or cancelled.
Four previously active tasks remain interrupted because their operation outcomes
are not sufficiently confirmed for automatic continuation. One remains running.

Temporary update modules and manifests were removed from the application.
The installed application and packaged artifact pass strict signature verification.
The signing certificate remains the existing Codex Studio certificate.

A final exact method update clears completed catalog notices. It changes only
`mark_current` and verified current catalog notices. Matching errors become null;
unrelated errors remain. Four isolated deployment cases passed, including SQL
rollback and rejection of unknown code. The live receipt is
`2026-09-22-catalog-notice-v1`, applied to PID `61997` on its first attempt.
No catalog notice remains on a nondeleted chat. The temporary patch was removed.
