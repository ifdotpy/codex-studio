# Incremental sync live patch inventory

The compatibility window is closed: the pull request that completed the move to
sync entities (round 3), `#TBD-ROUND3`, removed `/api/state` and the legacy
`state` / `state:chat` pull scopes. The install sequence and dry-run notes below
record the earlier rollout plan; they are not current deployment instructions.

The guarded server patch artifact is [`scripts/codex_sync_live_patch.py`](../scripts/codex_sync_live_patch.py). It is built for the live server method layout at `a1ede55` and the rebased renderer/server source. Do not start another backend or point this patch at a live database during fixture verification.

## Install order

1. Apply the guarded server patch and install `codex_sync_entities.py` in the running server's `scripts` directory. This updates the server while preserving its existing `Runtime`, `Canvas`, `SyncStore`, and HTTP handler instances.
2. After the server patch is active, install the web assets.
3. At the time of this rollout plan, new clients used `state:entities:v1` on reload; old clients used `state` and `state:chat` until reloading. That compatibility window is now closed.

## Lazy initialization and schema inventory

`make_server` creates the `SyncStore` only on the first sync identity, pull, or stream request. Its constructor ensures the entity and legacy scope schema. The first pull or generation request lazily creates `sync_versions` and opens its read-only `data_version` connection. The first `state:entities:v1` pull seeds the entity set and writes the one-time `sync_entity_meta` seed marker.

Objects created by this change:

| Object                                                                    | Created when                        | Persistent write cost                                                                                                                                                                                                                                        |
| ------------------------------------------------------------------------- | ----------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `sync_entities` table                                                     | First lazy `SyncStore` construction | One row per `(collection,id)`. Initial pull seeds current renderer DTOs; later writes upsert only when projected DTO hash or deletion state changes. Transcript rows store `payload=NULL`, and transcript tombstones are retained within the 512-item bound. |
| `sqlite_autoindex_sync_entities_1` primary-key index on `(collection,id)` | Created with `sync_entities`        | Updated for each inserted entity key; an upsert of an existing key keeps the same key entry. Included in the measured combined entity-table write cost below.                                                                                                |
| `sync_entities_seq` index on `seq`                                        | First lazy `SyncStore` construction | Updated on each changed-entity upsert; supports ordered incremental pulls. Included in the combined write measurement below.                                                                                                                                 |
| `sync_entity_meta` table                                                  | First lazy `SyncStore` construction | One row, `seeded=1`, on the first entity pull; no recurring writes.                                                                                                                                                                                          |
| `sqlite_autoindex_sync_entity_meta_1` primary-key index on `key`          | Created with `sync_entity_meta`     | One index insertion with the one-time seed marker; no recurring writes.                                                                                                                                                                                      |

The synthetic write measurement for `sync_entities` and its indexes is **824,032 WAL bytes per minute** for 100 changed entities in a minute. SQLite reported 819,200 committed WAL-frame bytes at checkpoint. This includes the table pages, primary-key index and sequence index; their byte costs were not isolated from one another. Internal-only runtime writes and unchanged projected DTOs add no entity-table writes. `sync_entity_meta` and its primary-key index add one-time seed-marker cost, not recurring per-minute writes.

The following compatibility objects are also ensured if missing. They predate this change and are not part of its incremental write cost:

| Existing object                                                                      | Purpose and write behavior                                                                                                                        |
| ------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------- |
| `sync_identity` table and its primary-key autoindex                                  | Existing workspace identity; one row is inserted only for a database without an identity.                                                         |
| `sync_generation` table                                                              | Existing legacy coarse change generation; initialized to one row if absent. Entity writes do not bump it.                                         |
| `sync_documents` table, its `UNIQUE(scope,id)` autoindex, and `sync_scope_seq` index | Existing drafts and legacy scope storage; writes remain limited to those old routes. New entity state is not copied here.                         |
| `sync_versions` table and its `UNIQUE(scope)` autoindex                              | Existing old-scope hash checkpoints; first-use DDL is lazy. Old `state` / `state:chat` pulls write a checkpoint when their snapshot hash changes. |

No other new indexes are created. Three triggers, `sync_entity_event_INSERT`, `sync_entity_event_UPDATE`, and `sync_entity_event_DELETE`, cover direct `runtime_events` writes that bypass `Runtime.put`; they do not create a journal table. They upsert the current event entity only when its renderer hash changes. Old `sync_watch_%` row triggers are removed when the version reader initializes.

## Runtime and handler closure patch points

Patch these methods together so DTO changes and cache invalidation stay aligned:

- `Runtime.put`, `Runtime.records`, `Runtime.db`, `Runtime.invalidate_agent_records`, and `Runtime.mark_agent_records_changed`.
- `Canvas.register_agent`, `Canvas.connect_chat`, `Canvas.create_chat`, `Canvas.post`, and `make_server` for graph/chat mutations, action response entities, and SSE.
- `SyncStore.pull`, `SyncStore._put`, `SyncStore._ensure_versions`, `SyncStore.entity_sequence`, `SyncStore.transcript_pull`, `SyncStore.shared_snapshot`, and the transcript order/tombstone constants.
- `codex_sync_entities.project`, `put`, `seed`, `next_sequence`, `max_seq`, `ensure_tables`, and `install_bypass_triggers`.

Every method on the closure-created `Handler` class is listed here:

- `log_message`: suppresses request logging.
- `send`: attaches changed non-transcript entity DTOs to successful action responses; retains compression, ETag, and response headers.
- `trusted`: enforces local-origin and write-token checks.
- `stream_transcript`: streams transcript item deltas.
- `stream_sync`: emits numeric entity max sequence for `state:entities:v1`; emits legacy `"RESYNC"` for old scopes.
- `do_GET`: serves entity and legacy pulls, streams, snapshots, and other GET routes.
- `do_POST`: applies writes, captures the pre-action entity sequence, and returns changed entity documents with successful actions.
- `service_actions`: periodically prunes voice audio.
- `server_close`: closes owned cost and terminal readers.

`make_server` also closes over `terminals`, `snapshot`, and `sync`; the existing server exposes `server.canvas`, `server.sync_store`, `server.snapshot_state`, and `server.RequestHandlerClass` for in-place patching. The handler methods that require changed implementations for this feature are `send`, `stream_sync`, `do_GET`, and `do_POST`; preserve `stream_transcript`, `trusted`, and lifecycle methods from the same installed class revision.

## Client scope transition order

1. Historical rollout step: install server support while keeping `/api/sync/pull?scope=state`, `state:chat`, and the legacy stream behavior available. Existing clients then continued receiving their old full snapshots and `"RESYNC"` events.
2. Install the renderer bundle. On reload, new clients map the local `state` projection to remote scope `state:entities:v1`, pull and persist entity documents by global sequence, and use numeric entity SSE as a pull hint. Initial pulls page until they reach the server's returned `maxSeq`; successful actions can apply their returned entity documents immediately.
3. Old clients switch only when they reload into the new renderer. They then begin using the entity scope. Until that happens, their requests remain on the legacy scopes; the server keeps both routes active, so the cutover is per client and requires no forced migration.
4. Historical rollout step: keep legacy `state` and `state:chat` behavior during the compatibility window. The round 3 sync-entities pull request listed above closed that window and removed the legacy routes and scopes.

Transcript and drafts retain their independent scopes. Transcript item revisions use `sync_entities` hashes and tombstones only; pull responses derive current payloads from the transcript and do not persist transcript text in `sync_documents` or `sync_entities`.

## Evidence boundary

The guarded patch dry run used a clean archive of `a1ede55` as the old server and the rebased source tree as the new source. In one process it created the old `SyncStore` through `GET /api/sync/pull?scope=state`, applied the patch twice (`applied`, then `already_applied`), paged the entity scope through `maxSeq`, fetched legacy `state` and `state:chat`, observed a numeric entity SSE hint after a `Runtime.put`, pulled a transcript from a lagging cursor while verifying transcript table payloads remain NULL, and saved a budget through the prepatch reused SQLite connection. The same patched process then passed `sync-entities-contract`, `critical-sync-contract` (7), `mobile-state-contract` (9), and `runtime-contract` (59).

Measurements are in [incremental-sync-measurements.md](incremental-sync-measurements.md). Contract and volume checks use isolated SQLite fixtures. This worktree did not connect to or modify a live database, backend, or `/Applications` bundle.
