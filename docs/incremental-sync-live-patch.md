# Incremental sync live patch points

The patch changes server source only. Do not copy or rebuild renderer assets as part of a server-only patch. The lead owns applying it to a running installation.

## Existing server instance

`make_server` exposes `server.canvas`, `server.sync_store` (a lazy accessor), and `server.snapshot_state`. Its `RequestHandlerClass` is the patch point for `send`, `do_GET`, `do_POST`, and `stream_sync`. A patch updates those methods on the existing class; it does not create another `LocalServer` or backend. The handler keeps old `/api/sync/pull?scope=state` and `state:chat` routes available for clients that have not reloaded. New clients use `state:entities:v1` and its numeric sequence stream.

## Runtime methods and lazy fields

Patch these `Runtime` methods together: `put`, `records`, `db`, `invalidate_agent_records`, and `mark_agent_records_changed`. Patch `Canvas.register_agent`, `connect_chat`, `create_chat`, `post`, and `make_server` for graph/chat writes, action responses, and streaming. `SyncStore.pull`, `_put`, `_ensure_versions`, `entity_sequence`, and `shared_snapshot` own the sync routes and sequence spaces. `codex_sync_entities.project`, `put`, `seed`, `next_sequence`, `max_seq`, `ensure_tables`, and `install_bypass_triggers` implement bounded projections and tracking.

An existing `SyncStore` initializes `_version_lock`, `_versions_ready`, `_version_reader`, and the SQLite entity schema on its first versioned pull. Runtime agent cache state is lazily held in `_agent_records_cache`, `_agent_records_cache_lock`, and `_agent_record_revision`. The only row triggers added are `sync_entity_event_INSERT`, `sync_entity_event_UPDATE`, and `sync_entity_event_DELETE` on `runtime_events`, for direct writes that bypass `Runtime.put`; entity hashes suppress unchanged DTO updates.

## Tables and compatibility

The patch adds `sync_entities` (one row per `(collection,id)`), `sync_entity_meta`, its sequence index, and uses existing `sync_documents`, `sync_versions`, and `sync_identity` for their established scopes and shared sequence allocation. The transcript agent stores null entity payloads for its transcript IDs; transcript pulls derive payloads from current transcript rows. No full transcript content is added to `sync_entities`.

After server patching, new actions attach changed entity documents to successful JSON responses. New renderer code applies these documents locally and fetches subsequent versions from the entity pull route. Existing renderer assets retain their prior snapshot routes until reload. Verify on an isolated fixture before patching production; this worktree did not connect to or modify a live DB or backend.
