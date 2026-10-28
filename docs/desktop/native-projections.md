# Native projections

In the desktop app, the native core stores the projection cache of the local
server. The page does not open IndexedDB, RxDB, Web Locks or a BroadcastChannel
for it. This removes the slow storage path that caused "Could not prepare or
apply local sync data. Reconnecting...": WebKit storage took seconds for a
startup snapshot, and the page waited for it before it applied or resumed the
stream. The [native live stream](native-live-stream.md) is step 1. This is step 2.

The [sync protocol](../sync-protocol.md) is unchanged. Stream events still carry
the data. The core stores it. The client does not poll.

## What the core owns

| State                                                                     | Owner                                                                                                                                                                    |
| ------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Entity rows, tombstones, ready and complete markers, per-resource cursors | Core SQLite store, rows `projection.getRows`, `rangeRows`, `putRows`, `pruneRows` in [`projections.rs`](../../workspaces/client/packages/client-core/src/projections.rs) |
| Transcript rows and baselines, terminal and room view cursors             | Same rows                                                                                                                                                                |
| Cache format (API schema hash in the row prefix), removal of older caches | Core `pruneRows`; the page names the prefix                                                                                                                              |
| Staging of large entity intervals                                         | Page memory, until the last page proves the interval; the result goes to the core                                                                                        |
| Order of writes                                                           | One queue in the page (`withEntityProjectionWrite`); the page is the only writer                                                                                         |

The page reads rows through [`nativeProjections.ts`](../../workspaces/client/apps/web/src/sync/nativeProjections.ts).
It offers the part of the RxDB collection API that the sync client uses:
`find`, `findOne`, `storageInstance.findDocumentsById`, `storageInstance.bulkWrite`
and change streams. A write whose base row changed gets a conflict, as in RxDB.
A cursor is written only after the rows it covers.

The core stores each row as an opaque document. Its sequence is the server
sequence. A reset may lower it, so the store has no ordering rule of its own.
The ordering rules (which sequence wins, tombstones, eviction) stay in the
page code that already owned them.

Native commands that return rows stay under the native message limit. A range
reply stops at a row limit or a byte limit and says `more`. The page continues
after the last id.

## Page without browser storage

The sync client takes an optional `projectionStore`. The local server in the
desktop app and the default client pass the native store. Then:

- `syncDatabase()` returns the workspace identity and the native projections. It opens no database.
- `outboxDatabase()` opens the RxDB database of the outbox on demand (see below).
- RxDB and Dexie load on first use. The native path does not load them for projections.
- `createResourceEvents` runs with `independent: true`: one page owns its stream, with no Web Locks owner election and no BroadcastChannel.
- Large entity transfers stage in memory (`MemoryTransferStorage`).

A remote server in the desktop app and a plain browser keep the RxDB path.

## Failure cases

- The page restarts: it reads the cursors and rows from the core. A covered reconnect needs no HTTP read.
- A write fails in the core: the frame fails like a failed IndexedDB write did, the cursor stays, and the stream resumes from it.
- A late write after a reset: the same sequence rules apply as before. The core never reorders.
- The API schema changes: the page uses a new prefix, starts empty, and the core removes the old rows.

## Uploads and recordings

Step 4 moves the file uploads and dictation recordings of the local server into
the core. The page sends commands and renders the state.

- Uploads: `files.begin` and `files.append` stage a file in parts of at most
  1 MiB (`PART_LIMIT` in [`uploads.rs`](../../workspaces/client/packages/client-core/src/uploads.rs),
  `UPLOAD_PART_BYTES` in `native/uploadParts.ts`). A 20 MiB file never crosses
  the native bridge in one message. Parts are not request receipts, so their
  bytes are stored once. `uploads.enqueue` queues the staged files.
- Retries: `files.begin`, `uploads.queue` and `dictation.create` and
  `dictation.begin` replay their first reply for the same request identity. An
  append repeats at the same offset. A lost reply therefore does not create a
  second upload, recording or transcription attempt.
- Cleanup: a failed queue sends `files.discard` for its unqueued handles. On
  core start, a staged file older than `STAGE_MAX_AGE_MS` (24 hours, in
  milliseconds) that no upload row matches is removed. A queued file has no
  stage left, so discard and cleanup never touch an upload.
- Cancel: `uploads.cancel` leaves a tombstone. A late `uploads.deliver` reply
  never creates an attachment.
- Dictation: `dictation.*` stores recordings and PCM chunks. The core assigns
  recording and attempt identities. Trash clears the attempt, so a late
  transcript is refused. The page joins WAV slices from `dictation.read`.
- Old browser rows (`codex-studio-uploads`, `codex-studio-dictation`) move once
  on the first page load (`native/legacyUploads.ts`, `native/legacyRecordings.ts`).
  A file imports under its old row ID. A finished recording gets a core marker,
  so a deleted recording does not return. A recording that cannot be read
  becomes an error row, and an upload that cannot be read stops the list with
  its name. The old rows stay in browser storage.
- Remote servers and the plain browser keep the IndexedDB path until the
  transport step.

## What stays on IndexedDB

- The composer attachment list and the form drafts of the local server. They use `localDraft` (localStorage with IndexedDB locks). Steps 5 and 6 move them.
- The outbox and drafts of the local server (`send.ts`). Step 3 moves the outbox to `outbox.*`. The page opens that database on demand, so a slow database no longer blocks the stream or the projections.
- Old IndexedDB projection databases from earlier builds stay on disk until the user clears them. The native path never reads them.
- Remote servers and the plain browser path (RxDB, Dexie, Web Locks, BroadcastChannel).

## What moves later

The logic that decides how a stream event changes the cache (scoped eviction,
epoch resets, transcript window merge, terminal merge) and the snapshot builder
(`entityProjection.ts`) are still TypeScript. They run over the core's rows.
The core already has `projection.applyEntities` and `sync.rs`, but its snapshot
lacks fields the page needs (models, receipts, UI preferences, project
sequences). Moving the apply logic is the next step.
