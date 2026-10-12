# studio-runtime-store

## Change Contract

- **Responsibility:** Open an existing Studio state directory in place, exclude
  other runtime processes with `runtime.lock`, validate the committed SQLite
  schema snapshot, and provide synchronous typed reads and writes.
- **Public interface:** `RuntimeStore::open`, `RuntimeStore::read`, and
  `RuntimeStore::transaction`; repositories for agents, tasks, queue events,
  and chat messages.
- **Boundaries:** No Tokio, Python, schema creation, schema migration, or state
  directory creation. The root OpenAPI document owns HTTP DTOs; the Python
  `codex_records.py` declarations are the current source for persisted record
  fields until storage DTO generation is introduced.
- **Invariants:** One exclusive runtime lock, one serialized writer using
  `BEGIN IMMEDIATE`, independent query-only read connections, WAL and FULL
  synchronous writes, exact SQLite schema and migration-marker agreement.
- **Configuration owner:** `codex_state.py` currently defines state path
  resolution; this crate mirrors its environment and platform rules. The
  committed `runtime-state-schema.json` is the only schema snapshot.
- **Focused check:** `cargo nextest run -p studio-runtime-store`.

The remaining table families are `runtime_access_*`, `runtime_agent_moves`,
`runtime_agent_record_generation`, `runtime_annotations`, `runtime_assets`,
`runtime_budget*`, `runtime_capacity_retries`, `runtime_checkpoints`,
`runtime_compactions`, `runtime_complaints`, `runtime_completed_turns`,
`runtime_context_*`, `runtime_event_meta`, `runtime_execution_*`,
`runtime_federation_*`, `runtime_item_fulltext`, `runtime_items`,
`runtime_lead_requests`, `runtime_monitors`, `runtime_native_sweeps`,
`runtime_operation_receipts`, `runtime_payload_refs`, `runtime_plans`,
`runtime_profiles`, `runtime_projects`, `runtime_requests`, `runtime_rooms`,
`runtime_rules`, `runtime_scheduler_*`, `runtime_search_*`,
`runtime_server_*`, `runtime_sidebar_order`, `runtime_supervisor_cursor`,
`runtime_tool_*`, `runtime_turn_item_link_state_v1`,
`runtime_turn_item_links_v1`, `runtime_usage_resumes`,
`runtime_work`, `runtime_workspace_operations`, and the `sync_*` tables.

Stored record fields are currently derived by hand from `codex_records.py`.
When a stored record is identical to an HTTP API model, generate it from the
canonical OpenAPI document; otherwise keep the persistence shape local to the
store. Unknown JSON fields are retained in a flattened map on read and rewrite.

The initial state directory schema is created by the Python runtime today. A
fresh state directory without Python does not get provisioned by this crate;
the schema snapshot's SQL is the candidate source for a separately owned
bootstrap step. This crate deliberately never executes that SQL.
