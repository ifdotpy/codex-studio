# UI synchronization

[`workspaces/runtime/apps/server/src/codex_sync.py`](../workspaces/runtime/apps/server/src/codex_sync.py) owns the production
`SyncStore`, constructed by [`workspaces/runtime/apps/server/src/studio_api/context.py`](../workspaces/runtime/apps/server/src/studio_api/context.py).
The HTTP integration is in [`workspaces/runtime/apps/server/src/studio_api/sync/router.py`](../workspaces/runtime/apps/server/src/studio_api/sync/router.py).
The client and server wire contract is in [`sync-protocol.md`](sync-protocol.md).

The store serves the legacy state projection, current entity-based workspace
projection, sparse transcript updates, and browser-owned draft branches. The
entity and transcript streams use durable monotonically increasing sequences
and bounded tombstone recovery. Draft writes use compare-and-swap conflict
handling. Request receipts and scheduler observations remain owned by their
runtime components.

## Invalidation and snapshots

The compatibility generation uses a persistent read-only connection's SQLite
`PRAGMA data_version`. It advances when another connection commits to the
database, including writes that do not alter the visible state projection.
Legacy state snapshots are cached against that clock with a short age bound,
and their content hash prevents unchanged projections from consuming another
document sequence. This broad invalidation is intentional for the production
compatibility stream.

The generation map exposes the same broad database clock for state and
transcripts, plus the draft checkpoint. Compact per-agent transcript revisions
let clients skip unchanged transcript projections despite the shared clock.
Status signatures supplied by the runtime add a durable invalidation hint for
external state. Entity streams use their own durable entity revisions and
tombstone floor rather than the legacy snapshot clock.

This implementation does not expose independent trigger-backed state and
transcript generation counters. A former sync-store prototype did; tests and measurements based on it did not describe production
behavior. See the [consolidation audit](verification/2026-10-05-syncstore-consolidation.md)
for the complete comparison and decisions about prototype-only contracts.

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
