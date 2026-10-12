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
- **Focused check:** with the isolated Python environment prepared, run
  `XDG_CACHE_HOME=/var/tmp/cs-rust-store/cache CARGO_TARGET_DIR=/var/tmp/cs-rust-store/target PYTHON="$(XDG_CACHE_HOME=/var/tmp/cs-rust-store/cache python3 workspaces/runtime/apps/server/src/codex_python.py)" cargo nextest run -p studio-runtime-store`.
  Python subprocesses create disposable state under `/var/tmp/cs-rust-store/tests`.

Python connection behavior reproduced in this slice:

| Behavior                                                                                                             | Python source                                                                  |
| -------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------ |
| Main DB connection timeout of 15 seconds (`busy_timeout=15000`)                                                      | `codex_runtime.py:2017`                                                        |
| Read-only `read_db()` connection and `PRAGMA query_only=ON`                                                          | `codex_runtime.py:2661-2665`                                                   |
| Analytics connection timeout and `PRAGMA journal_mode=WAL`                                                           | `codex_runtime.py:2686-2690`                                                   |
| Analytics read `PRAGMA query_only=ON`                                                                                | `codex_runtime.py:2699-2702`                                                   |
| Temporary connection `PRAGMA busy_timeout` changes                                                                   | `codex_runtime.py:2184-2185, 2878-2881`                                        |
| Startup WAL setup for the primary DB                                                                                 | `codex_runtime.py:1597`                                                        |
| `synchronous=FULL` observed on both DBs by the snapshot; Python does not explicitly set it in these connection paths | `codex_runtime.py:2017, 2686`; `codex_analytics.py` has no SQLite PRAGMA calls |

Rust checks the expected WAL/FULL values before opening the writer, then sets
`synchronous=FULL` on that connection. Reads are opened read-only and set
`query_only=ON`. Its standard busy timeout is 15 seconds. Event sync triggers
depend on Python's connection-local `sync_entity_payload` and
`sync_entity_hash` functions; the writer registers compatible event projections
and SHA-256 encoding before writes. The bundled SQLite is 3.53.2 (the version
in `libsqlite3-sys`'s bundled header); this host's Python SQLite is 3.53.4.
Cross-runtime tests exercise Python opening and writing databases after Rust
writes, but the two SQLite patch versions remain a compatibility surface to
recheck when either runtime upgrades.

The event triggers depend on Python's connection-local `sync_entity_payload`
and `sync_entity_hash` functions (`codex_sync_entities.py:437-449`). The payload
function JSON-decodes the event projection, keeps `EventEntityDto` fields from
`codex_sync_entities.py:151-271`, and encodes the collection/id/value envelope
as sorted compact UTF-8 JSON in `codex_sync_entities.py:371-374`; the hash
function is lowercase SHA-256 over those UTF-8 bytes. Rust registers equivalents
on its sole writer connection. SQLite functions are connection-local, so a
connection that changes `runtime_events` without registering them fails with
`no such function: sync_entity_payload` or `no such function: sync_entity_hash`.
The integration test
`python_fixture_round_trips_through_rust_and_python_write_rollback_gate`
compares the full sync row (sequence, payload, hash, deleted flag) after the
same event status update through Rust and Python. The projection normalizes its
floating `created` value through `f64` to match Python's `json.loads`/`json.dumps`
round trip, despite the Rust and Python SQLite patch versions differing.

CI runs that integration test in **Fast tier: remaining checks** because that
job's `fast-tier-setup` action builds the native module and prepares the server
Python dependencies before `test-fast-rest` runs workspace nextest. The **Rust
checks** job runs fmt, clippy, and doctests only; it does not run nextest or this
Python-dependent integration test. The test is never silently skipped: Python
runtime or native module setup failures produce an explicit failing import
probe with setup instructions.

`Cargo.lock` pins `rusqlite` 0.40.2 and its `libsqlite3-sys` 0.38.2 bundled
SQLite source (3.53.2). Bump the bundled SQLite deliberately by updating the
rusqlite/libsqlite3-sys lock selection, checking the compiled `SQLITE_VERSION`,
and rerunning cross-runtime trigger/hash and rollback tests against the host
Python SQLite before accepting the lockfile update.

New crates, checked against the registry on 2026-10-12:

| Crate       | Version | License           | Purpose and removal                                                                                                                  |
| ----------- | ------- | ----------------- | ------------------------------------------------------------------------------------------------------------------------------------ |
| `rusqlite`  | 0.40.2  | MIT               | Synchronous SQLite access and bundled SQLite replace Python DB-API access; remove it with this crate when the Rust store is retired. |
| `thiserror` | 2.0.21  | MIT OR Apache-2.0 | Typed store errors; remove it when the crate's public error interface is retired.                                                    |
| `sha2`      | 0.11.0  | MIT OR Apache-2.0 | Reproduces Python `hashlib.sha256` for stored sync-event hashes; remove it with event trigger compatibility.                         |

`rusqlite` and `thiserror` are actively maintained; the selected versions are
their current stable releases. `sha2` is the actively maintained RustCrypto
SHA-2 implementation. Cargo's lockfile is the source of exact resolved
transitives (the crate dependency tree contains 36 distinct packages,
including workspace dependencies). Serde and serde_json are existing workspace
dependencies; Serde's derive feature is enabled for this crate's records.

Remaining repository work, grouped by table family (exact names from the
committed snapshot):

- Access: `runtime_access_alias_requests`, `runtime_access_audit`,
  `runtime_access_clients`, `runtime_access_invites`,
  `runtime_access_local_alias`, `runtime_access_name_requests`,
  `runtime_access_name_targets`, `runtime_access_nonces`,
  `runtime_access_outbound`, `runtime_access_receipts`.
- Accounts and agents: `runtime_account_transfers`, `runtime_agent_moves`,
  `runtime_agent_record_generation`, `runtime_annotations`, `runtime_assets`,
  `runtime_budget`, `runtime_budget_usage`, `runtime_capacity_retries`,
  `runtime_checkpoints`, `runtime_compactions`, `runtime_complaints`,
  `runtime_completed_turns`, `runtime_context_manifests`,
  `runtime_context_reminders`.
- Events and execution: `runtime_event_meta`, `runtime_execution_attempts`,
  `runtime_execution_effects`, `runtime_execution_inputs`,
  `runtime_execution_nodes`, `runtime_execution_runs`.
- Federation: `runtime_federation_identity`, `runtime_federation_inbox`,
  `runtime_federation_invites`, `runtime_federation_nonces`,
  `runtime_federation_outbox`, `runtime_federation_peers`,
  `runtime_federation_rooms`, `runtime_federation_settings`.
- Items and operations: `runtime_item_fulltext`, `runtime_items`,
  `runtime_lead_requests`, `runtime_monitors`, `runtime_native_sweeps`,
  `runtime_operation_receipts`, `runtime_payload_refs`, `runtime_plans`,
  `runtime_profiles`, `runtime_projects`, `runtime_requests`, `runtime_rooms`,
  `runtime_rules`.
- Scheduler: `runtime_scheduler_agent_change_state`,
  `runtime_scheduler_agent_changes`.
- Search: `runtime_search`, `runtime_search_config`,
  `runtime_search_content`, `runtime_search_data`, `runtime_search_docsize`,
  `runtime_search_idx`, `runtime_search_indexed`, `runtime_search_next_meta`,
  `runtime_search_rollout`, `runtime_search_rows`,
  `runtime_search_rows_rollout`.
- Server: `runtime_server_fetch`, `runtime_server_inbox`,
  `runtime_server_links`, `runtime_server_outbox`,
  `runtime_server_output_tools`, `runtime_server_retired`,
  `runtime_server_sequence`, `runtime_sidebar_order`,
  `runtime_supervisor_cursor`.
- Tools and turns: `runtime_tool_request_aliases`, `runtime_tool_requests`,
  `runtime_tool_results`, `runtime_turn_item_link_state_v1`,
  `runtime_turn_item_links_v1`, `runtime_usage_resumes`, `runtime_work`,
  `runtime_workspace_operations`.
- Sync: `sync_documents`, `sync_entities`, `sync_entity_meta`, `sync_versions`.
- Analytics database: `analytics_agents`, `analytics_cost_changes_v1`,
  `analytics_cost_projection_v1`, `analytics_cost_roots_v1`,
  `analytics_cost_state_v1`, `analytics_history`, `analytics_items`,
  `analytics_limits`, `analytics_meta`, `analytics_notifications`,
  `analytics_turns`, `analytics_usage`, `analytics_usage_roots`.

Stored record fields are currently derived by hand from `codex_records.py`.
When a stored record is identical to an HTTP API model, generate it from the
canonical OpenAPI document; otherwise keep the persistence shape local to the
store. Unknown JSON fields are retained in a flattened map on read and rewrite.
Each repository is a small type with `get(connection, id)` and
`put(transaction, record)` methods; JSON-backed families share the private
`get_json_record`/`put_json_record` helpers, while SQL row families map their
columns explicitly. This is the pattern for adding the remaining families.

The initial state directory schema is created by the Python runtime today. A
fresh state directory without Python does not get provisioned by this crate;
the schema snapshot's SQL is the candidate source for a separately owned
bootstrap step. This crate deliberately never executes that SQL.

The schema is included directly from
`workspaces/runtime/apps/server/tests/fixtures/runtime-state-schema.json`;
do not copy or edit it here. When the Python runtime and its tests are removed,
move the snapshot under this crate and keep it as the same single authority.
