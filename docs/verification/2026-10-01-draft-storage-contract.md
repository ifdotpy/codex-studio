# Durable per-chat draft storage contract

## Input ownership

- `useSyncedDrafts` owns the in-memory value and synchronous write boundary. A
  composer edit updates the value and notifies that chat's subscriber before
  returning from the input event.
- Local durable state is stored under one key per workspace and chat. A write
  touches only the edited chat's key; it must not serialize every other draft.
- The per-chat local record is the reload and offline recovery source. The
  pending version journal remains the source for unsynchronized edits and keeps
  writer/chat versions separate across tabs. RxDB and the server continue to
  own cross-device replication and conflict reconciliation.
- Sending reads the current value synchronously. Existing outbox ownership,
  request identity, and clear-only-after-accepted-send behavior remain intact.

## Migration and crash recovery

- The old `codex-drafts:<workspace>` object is a read-only migration source.
- Migration copies each legacy chat value to its per-chat key only when that
  key does not already exist. The new key is the per-chat commit record.
- Newer per-chat records retain the last legacy-map value observed as an
  optional SHA-256 checkpoint digest bound to the exact session and UTF-16 text;
  absent and empty values have distinct framing. Older records with the full checkpoint
  string remain readable and are compacted when next written. An already-open
  old tab that changes that value creates a distinct legacy writer version in
  the pending journal. The new local value or tombstone remains selected while
  the new branch is reconciled as a conflict alternative.
- A chat first introduced by an old tab after migration is adopted into the
  running hook, returned as a legacy update, journaled, and replicated. It is
  not enough to create a per-chat checkpoint and wait for a reload.
- If migration stops between chats, the legacy object remains intact. A retry
  skips committed keys and copies the remaining entries. A present tombstone
  also counts as committed, so an old value cannot resurrect a cleared draft.
- A changed old-tab value is journaled before the checkpoint advances. If
  writing the journal fails, the old checkpoint causes the value to be retried;
  if sync fails after the journal write, the branch survives reload and retries
  from the local journal/RxDB. The existing bounded resume timer also rescans
  the aggregate map, so a journal-storage failure is retried without another
  storage event or reload.
- Migration never deletes or rewrites the legacy object. This allows already
  open older tabs to continue using it. The new client does not mirror normal
  edits back into the aggregate object.
- A per-chat write failure leaves the current in-memory input available, keeps
  its pending journal entry when possible, surfaces the existing local error,
  and retries through the existing flush/resume path. Reload recovery is only
  promised for data whose storage write completed.

## Scope, versions, and failure behavior

- Every local key includes the workspace identity and encoded chat/session ID.
  An unassigned cache may be adopted only by the workspace identity verified
  by sync; records from a different workspace are neither read nor moved.
- A clear is stored as a durable tombstone, distinct from an empty draft. This
  prevents legacy fallback from restoring an intentionally cleared value.
- Checkpoint digests use the standard SHA-256 compression function and include
  the chat session plus exact JavaScript UTF-16 code units. Known-vector tests
  verify framing, session binding, and absent-versus-empty distinction.
- Malformed percent-encoded chat keys and corrupt records are isolated per key;
  healthy chats remain readable, and the hook reports a local recovery error.
- Distinct tab writers keep distinct pending versions. Reconciliation retains
  concurrent unseen text as alternatives; an older recovered write cannot
  replace a newer observed version.
- Legacy server document IDs (`device:session`) remain valid. New clients
  observe those IDs in their `seen` map when editing, so an unchanged legacy
  branch retires cleanly; an old tab's later write advances that same legacy
  branch beyond the observed version and remains a conflict candidate. Local
  checkpoints are optional fields on v1 per-chat records, so older v1 readers
  continue to validate and read the value/tombstone fields they understand.
- Offline editing does not require server identity or replication to succeed.
  Cached workspace identity permits local display/recovery only; existing
  verification still gates server synchronization.

## Observable verification

- Seed a legacy map with several chats and verify migration bytes preserve every
  value while writing independent per-chat records. Repeat migration and verify
  no additional record changes. Interrupt after one record, reload, and verify
  the remaining records migrate without loss.
- Force local storage failure, reload, and verify the latest successfully
  journaled value or clear recovers and sync retries. Confirm an incomplete
  local write reports the error and does not claim durability.
- Edit the same chat in two tabs and verify both pending versions and the
  conflict outcome survive reload. Verify workspace A records never appear in
  workspace B.
- After migration, change the aggregate map from an old tab following both a
  local edit and a local tombstone. Verify unchanged old values do not
  resurrect, changed values become independently versioned alternatives, and
  the alternative survives a failed sync and reload.
- Add a new chat only in an already-open old tab after the new client migrated;
  verify immediate same-page hook adoption, durable journal creation, and sync.
  Inject one journal write failure for an existing changed chat, then allow the
  bounded rescan to recover it without reload or another old-tab event.
- With a simulated 5 MiB localStorage quota, migrate 500 legacy chats of 2,000
  characters and verify every record imports. Exercise an edit near capacity;
  a failed local write must be visible and recover through the journal when
  space is restored. Legacy source data remains intact throughout.
- Seed a malformed encoded key alongside healthy records and verify startup
  keeps the healthy chats available while surfacing a recoverable error.
- Send immediately after typing and verify the send path sees the current text
  and retains its established outbox/request identity semantics.
- Measure paired baseline/current bytes and setter latency with equivalent
  fixtures. Report timings as local observations, not guarantees.

## Verification record

- The 5 MiB storage fixture migrated 500 × 2,000-character drafts using
  4,302,432 bytes. Adding one 2,000-character pending journal brought modeled
  use to 4,307,016 bytes. After a near-capacity failure with 256 bytes of
  headroom, the prior local edit, journal, and aggregate fallback remained
  readable; restoring space allowed a subsequent edit and journal write.
  Accounting sums actual key/value UTF-16 code-unit bytes, including checkpoint
  data embedded in per-chat values. This is a deterministic storage simulation,
  not Chromium's origin quota or unrelated-origin usage.
- `node --experimental-strip-types tests/draft-storage-contract.mjs` passed:
  interrupted migration, retries, idempotence, workspace scope, writer ID and
  `seen` compatibility, old-tab edits and removals, tombstones, and malformed
  percent-encoded keys.
- `TMPDIR=/dev/shm CHROME_BIN=/usr/bin/chromium node
tests/draft-storage-migration-browser.mjs` passed in headless Chromium. It
  covers offline reload, failed old-tab sync and journal recovery, tombstones,
  existing-device reload, cross-version old-tab writes, adoption of a newly
  created old-tab chat into the running hook, bounded retry after one injected
  legacy-journal storage failure without a second event/reload, exactly one
  persisted RxDB row with the expected legacy device/session/text, empty draft
  and removal handling, and healthy records beside malformed keys.
- `TMPDIR=/dev/shm CHROME_BIN=/usr/bin/chromium node
tests/draft-recovery-browser.mjs` and `... node
tests/message-retry-http-browser.mjs` passed. These are isolated browser
  fixtures; they do not send live messages.
- The strict message-delivery fixture passed on both the final production
  build (`/dev/shm/studio-message-delivery-XvC0I9`) and the pinned
  `2470fb5` production build (`/dev/shm/studio-message-delivery-Si3ayz`). It
  records all mutations and permits only known draft identities with the
  listed before/after fixture texts; all other writes remain prohibited.
- The fixture additionally requires each `assumedMasterState` and
  `newDocumentState` payload to share the same exact document ID and chat
  session before its transition may enter the draft-sync allowlist. Synthetic
  mismatched-ID and mismatched-session rows are explicitly rejected.
- The exact-baseline 500-draft browser pair passed two focused storage rounds.
  It measured 40,195,940 aggregate-map edit bytes vs 9,660 per-chat edit
  bytes; one-time migration used 2,208,842 bytes. Setter medians were
  23.2/22.4 ms baseline and 3.8/4.9 ms current. This focused hook fixture is
  not the production App responsiveness suite. The separate Node in-memory
  pair measured 40,197,820 vs 10,580 edit bytes and 2,212,780 migration bytes;
  medians were 0.003687/0.010508 ms. Node times are not browser latency.
- The combined paired runner then stopped at its pinned-baseline production UI
  probe: desktop/scrolled typing recorded 3 App renders against a limit of 0.
  An earlier exact-baseline run passed that probe and the candidate recorded
  2 App renders in mobile/scrolled typing. A standalone report once described
  as candidate evidence identifies source `2470fb5`, so it is baseline-only.
  An independent replay of the saved candidate artifact
  (`/dev/shm/studio-ui-responsiveness-PitPa7`, index SHA-256
  `a1dc56d5cd16c7b040c6842c69615ab26a252db0616e757f79b2947c713639e9`)
  reproduced 1 App render in mobile/scrolled typing against the 0 limit.
  The focused draft-hook pair passed, but no full candidate production UI
  responsiveness run passed.
  Candidate production UI responsiveness remains unresolved; the paired result
  is not a pass. No live runtime or server state was used.

## Final integration verification

The reviewed migration was integrated at `49b6faa`, with the deterministic repeated-storage-failure fixture at `6014b0a`, and merged with concurrent application changes at `4aa4604`. The final TypeScript/Vite build passed. Storage contracts, the migration browser fixture (at least two held journal failures followed by exact checkpoint and one RxDB row), draft recovery, and production message delivery checks passed in the isolated integration worktree. Native integration review found no concrete merge regressions.

The earlier checkpoint assertion used an async browser polling predicate and could finish before durable convergence; the final test uses awaited snapshots with a bounded deadline. Production draft code did not change for this fixture correction. This does not establish a full production responsiveness pass; that previously documented limitation remains. No live backend or user state was changed.
