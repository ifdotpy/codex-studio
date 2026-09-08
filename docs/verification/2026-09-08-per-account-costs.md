# Per-account costs

Owner request: show the selected account's daily estimate. Remove the extra
scope labels from the footer and cost details.

The cost endpoint takes `account_key`. It resolves that key through the existing
account registry. Each account has a separate report and scanner cache. A profile
with changed credentials is rejected. No global total is used as a fallback.

The native helper reads the selected profile's sessions and archive. It retains
the upstream incremental parser and fork accounting. Its cache and trace database
paths are explicit. It excludes Pi logs. Only the non-secret pricing catalog is
shared. The helper does not fetch prices or invoke a model.

The reader allows one scan at a time across accounts, with a 120-second cache.
The HTTP response returns before a scan finishes. The UI rejects replies tagged
with a different account and discards late replies after an account switch.
The footer displays only `≈$… today` next to the selected account's limits.

Attribution uses the registry's profile-home association. Native session metadata
has no historical account ID. The amount is an estimate for that profile's local
history; it does not prove a complete account invoice.

## Checks

- Seven contracts cover separate totals and cache files, concurrent reads,
  restart, changed identity, errors, real HTTP routing, and guarded live updates.
- Eleven existing cost reader contracts pass.
- The account browser suite passes with distinct daily totals and a late response
  from a previous account.
- The limits browser suite passes, including reset receipts and narrow layouts.
- TypeScript and production builds pass.

The helper requires macOS and a local Swift compiler on its first build.
Its source and upstream license are included in `scripts/cost-scanner`.

## Live verification

- Four native helper tests pass: profile separation, incremental append,
  fork delta, and missing pricing.
- Verified helper SHA-256:
  `ab24db6da4b0e0814949691d55a7ddc45343b9d9ecd13f32ce259a52698062a7`.
- Independent real-profile scans returned $174.93, $161.15, and $671.17 today.
  All three reports had zero unpriced breakdowns. These are estimates.
- The Lumina repeat used its incremental cache and finished in 10.97 seconds.
  Its amount advanced from $668.50 to $671.17 while the account remained active.
- The guarded route update applied to PID 35212. The three native connection
  identifiers remained unchanged. No worker or monitor was restarted.
- All three live HTTP reads returned the correct account key and estimate in
  23 ms, while refresh continued in the background.
- A read-only browser probe switched the actual Attar and FieldView chats.
  Their footers showed $161.15 and $671.17, with no scope label.
- Concurrent native-error edits in the main checkout remain. The combined
  production build and account browser suite pass. Git index state is unchanged.

The existing Electron window needs a renderer reload to load the served bundle.
