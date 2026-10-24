# Compact sync protocol v4

Owner: lead of the compact-sync effort. Integration branch: `feat/compact-sync`.
The owner lifted the bug-fix freeze for this work and allows a breaking wire change.
Old clients receive `426 Upgrade Required` from the version check; no dual format.

## Goals

- Minimal bytes on the wire; usable on slow and lossy links.
- Point updates: only changed records travel.
- Simple, testable code; no speculative machinery. No new allocations or
  copies on hot paths compared with v3; measure before and after.
- The client applies every change of one SSE event in one batch (one store
  publication), never record by record.

## Wire contract

`SYNC_PROTOCOL_VERSION` becomes `4` (owner: `runtime-schema/src/sync_protocol.rs`).
OpenAPI stays the owner of every DTO; Rust and TypeScript types stay generated.

### Entity change (replaces `SyncEntity` everywhere: stream, pull, `_syncEntities`)

```json
{"op":"upsert","collection":"agent","id":"<id>","seq":42,"value":{...}}
{"op":"delete","collection":"agent","id":"<id>","seq":43}
```

- `value` is the structured typed DTO of that collection, never a JSON string.
- `collection` and `id` appear once; the client derives `entity:<collection>:<id>`.
- Within one batch the server sends at most one change per key (the latest).
- A list leaving membership is not a delete; `delete` means the entity is gone.

### Stream size

One limit, `SYNC_STREAM_EVENT_MAX_BYTES` (owner: `runtime-schema/src/shared_values.rs`),
bounds the complete serialized SSE event in UTF-8 bytes including `event:`, `id:`,
`data:` lines. It replaces `SYNC_ENTITY_STREAM_MAX_BYTES`,
`SYNC_TRANSCRIPT_STREAM_MAX_BYTES`, and `SYNC_DRAFT_STREAM_MAX_BYTES`. A larger
event carries revisions only, and the client pulls the interval over HTTP
(Brotli JSON). The HTTP compression threshold is a separate contract.
The limit applies to `resources` events only. `token-rates`, heartbeat,
session, acknowledgement, and `api-schema` events are exempt; `token-rates` stays
bounded by its schema `maxItems`.

### Stream compression

`/api/sync/stream` is Brotli-encoded when the request accepts `br`. The encoder
flushes after every event, so each event reaches the client at once, and keeps
its dictionary for the lifetime of the stream. Without `br` the stream is plain.

### Token rates

The `token-rates` event carries a full snapshot only as the first frame of a
stream; later frames carry changed records and removed agent ids.

### Live subscription changes

One SSE stream per client. The first event names a `sessionId`. The client changes
its resource set with `POST /api/sync/subscriptions {sessionId, gen, add, remove}`;
the stream answers with an acknowledgement for `gen`, followed by initial data of
added resources. The client drops data of removed resources received before that
acknowledgement. An unknown or expired session answers so that the client reopens
the stream with its full set and cursors, the v3 recovery path. Set changes are
coalesced on the client. If the resource set changes before a session is announced,
the client also reopens the stream with its full set and cursors.

## Ordering invariants (unchanged from v3)

Startup moves legacy workspace rate-limit snapshots to per-account entities and
removes the legacy workspace fields in
[`migrate_workspace_limits`](../../workspaces/runtime/packages/runtime-store/src/repositories/account_state.rs).
These committed changes use the shared entity/transcript sequence allocator.
Frozen v3 transcript fixtures therefore account for the startup allocation when
checking newly assigned cursors; existing transcript cursors remain unchanged.

- A notification never advances the applied cursor; only a fully applied interval does.
- A stale HTTP response cannot overwrite a newer change or revive a deleted entity.
- Unsubscribing cancels reads and rejects late results by subscription generation.
- Server restart, lost tombstones, and queue overflow force explicit recovery.
