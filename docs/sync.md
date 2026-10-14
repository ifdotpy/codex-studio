# UI synchronization

[`workspaces/runtime/apps/server/src/codex_sync.py`](../workspaces/runtime/apps/server/src/codex_sync.py) owns the production
`SyncStore`, constructed by [`workspaces/runtime/apps/server/src/studio_api/context.py`](../workspaces/runtime/apps/server/src/studio_api/context.py).
The HTTP integration is in [`workspaces/runtime/apps/server/src/studio_api/sync/router.py`](../workspaces/runtime/apps/server/src/studio_api/sync/router.py).
The client and server wire contract is in [`sync-protocol.md`](sync-protocol.md).

The store serves the entity-based workspace projection, sparse transcript
updates, and browser-owned draft branches. The
entity and transcript streams use durable monotonically increasing sequences
and bounded tombstone recovery. Draft writes use compare-and-swap conflict
handling. Request receipts and scheduler observations remain owned by their
runtime components.

## Invalidation and snapshots

Protocol-3 resource notifications invalidate subscribed projections after
their source writes commit. These transient notification revisions do not
replace the durable entity and transcript pull checkpoints. Compact per-agent
transcript revisions avoid rebuilding a page after an unrelated commit.

## Transaction ownership

Entity pulls use a committed SQLite read snapshot with `query_only` enabled.
They neither claim the writer nor repair projections, retire windows, or
schedule cleanup. Initialization upgrades and repairs existing views before
the server accepts requests. Runtime attachment prepares projections and
revision triggers that need runtime tables.

Source writers own their projected changes. Event triggers call the existing
bounded-window selector in the same transaction as the source event update,
including displacement tombstones and rollback. Runtime writers already own
agent, task, and monitor projections. The runtime maintenance tick schedules
bounded tombstone pruning; a GET does not become a maintenance operation.

Transcript pulls compare their hash-only ledger in a read snapshot first.
Already projected full pages, sparse deltas, deletions, and old-cursor recovery
do not claim the writer. A changed page or excess retained tombstones closes
that snapshot, claims the writer, and compares again before allocating durable
sequences. Sparse item deltas and the existing tombstone floor remain intact.
New transcript projections and browser draft writes still require a durable
write; saturated physical storage can delay those commits.

Runtime tool receipts and notification effects share writer-first admission in
[`Runtime.write_db`](../workspaces/runtime/apps/server/src/codex_runtime.py).
Writer contention waits outside the runtime state lock. After admission, state
changes and their commit retain the lock, and a failed body or uncertain commit
is never retried by the admission helper. Historical usage capture also uses
writer-first admission and saves one bounded history page per transaction,
then advances the separate analytics checkpoint. Application and commit retain
the runtime lock so a whole-record lifecycle update cannot erase budget fields.
Exact response IDs make replay
safe after a lost response or failed analytics checkpoint. Runtime WAL and
`synchronous=FULL` remain unchanged.

## Draft identity

Each browser tab owns its draft branch. A draft key can be `device:session` or
`device:writer:session`, where `writer` is one nonempty tab identifier without
colons. Identity fields in the payload must match that key. Conditional writes
return the current master row on conflict, preserving other tabs' drafts.

## Checks

The [`sync component guide`](../workspaces/runtime/apps/server/src/sync/README.md) runs the production
store's component tests and isolated benchmark. Root-level HTTP, entity, read,
write, critical-flow, and browser contracts verify integration through their
actual callers.

The entity read-lock and read-latency contracts hold an unrelated SQLite writer
open after committed source changes and assert successful reads with matching
checkpoints. Transcript contracts cover the same contention for unchanged full
and sparse pages, deletion, and tombstone-floor recovery. History and tool
receipt contracts verify batching, rollback, lock ordering, and exact retry
identities. These use disposable databases and controlled barriers rather
than synthetic disk load.
