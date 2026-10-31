# Projections in the page

The page keeps the projection cache of every server in memory. The UI reads only
memory. This replaces the native SQLite projection store and the RxDB and
Dexie caches. See "Frontend cache" in the
[target architecture](../decisions/2026-10-28-target-client-server-architecture.md).

The [sync protocol](../sync-protocol.md) is unchanged. Stream events still carry
the data. The client does not poll.

## Who keeps what

| Frontend                                                             | Projection cache                                                     | Unsent input               |
| -------------------------------------------------------------------- | -------------------------------------------------------------------- | -------------------------- |
| On the same machine as its server (desktop shell, loopback host)     | None. Memory only.                                                   | IndexedDB, written at once |
| Remote from its server (phone, UI-only computer, other server views) | Memory, plus one IndexedDB record per subscription (data and cursor) | IndexedDB, written at once |

"On the same machine" is decided by `frontendIsLocal` in
[`localFrontend.ts`](../../workspaces/client/apps/web/src/sync/localFrontend.ts):
the `local` owner, in the desktop shell or on a loopback or isolated view host.

## Cache records

[`thinCache.ts`](../../workspaces/client/apps/web/src/sync/thinCache.ts) uses the
plain IndexedDB API (database `studio-thin-cache`).

- One record holds all entity rows with the subscription cursors. One record
  holds one transcript with its items and its cursor.
- A record is written after the UI shows the data, at most every 5 seconds and
  when the page becomes hidden.
- A record loads when first used. A loaded row never replaces a row with a higher
  server sequence. Cached data shows as stale until a live frame confirms it.
- A lost write only makes the next reconnect ask for a larger delta.
- A new API schema hash uses new record keys. Records of older hashes are deleted
  after a matching schema response.

## Unsent input

The outbox and the drafts that the server has not confirmed are written to the
`unsent` store before the write is acknowledged. An outbox row leaves the store
when the message is accepted or cancelled. A draft leaves it when the server
accepts the push. The server owns drafts.

## Old stores

At the first start the page copies unsent rows from the old RxDB databases
(`rxdb-dexie-*`) to the `unsent` store, then deletes those databases
([`legacyStores.ts`](../../workspaces/client/apps/web/src/sync/legacyStores.ts)).
Nothing is deleted if a row could not be copied.

## Native core

The core no longer has the `projection.getRows`, `rangeRows`, `putRows` and
`pruneRows` commands (removed 2026-10-30, #630).
