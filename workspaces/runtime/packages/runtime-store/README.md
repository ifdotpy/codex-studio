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
  `XDG_CACHE_HOME=/var/tmp/cs-rust-store/cache XDG_STATE_HOME=/var/tmp/cs-rust-store/state TMPDIR=/var/tmp/cs-rust-store/tests CARGO_TARGET_DIR=/var/tmp/cs-rust-store/target PYTHON="$(XDG_CACHE_HOME=/var/tmp/cs-rust-store/cache python3 workspaces/runtime/apps/server/src/codex_python.py)" cargo nextest run -p studio-runtime-store`.
  Python subprocesses create disposable state under the system temporary
  directory (the command sets it to `/var/tmp/cs-rust-store/tests`).

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
function JSON-decodes its raw argument and projects it at
`codex_sync_entities.py:437-449`. The trigger constructs that argument with
SQLite `json_object` (`codex_sync_entities.py:456-467`), not a direct
`CAST(REAL AS TEXT)` and not `json.dumps` over the original database value.
`json_object` formats a REAL using the SQLite library linked into that Python
interpreter. Python then parses the JSON text, validates `EventEntityDto`, and
serializes the projected object with `json.dumps` (`codex_sync_entities.py:151-239,
371-374`). The DTO's only numeric field is `created: float | None`
(`studio_api/sync/models.py:1021-1028`); integers accepted there become binary64
floats, while strings and booleans are not numeric substitutes. Non-finite
values are removed (`codex_sync_entities.py:141-148, 187-191`). Python emits a
sorted, compact, UTF-8 collection/id/value envelope with shortest float spelling
and signed, zero-padded exponents such as `-1e-07`; SHA-256 is over those exact
bytes.

SQLite changed its default REAL-to-text rounding from 15 to 17 significant
digits in 3.53.0. The 3.52.0 release was withdrawn and its changes were moved
forward; the [3.53.0 release notes](https://sqlite.org/releaselog/3_53_0.html)
and [SQLite floating-point documentation](https://sqlite.org/floatingpoint.html)
describe the change. Therefore Python's event projection and hash can vary with
the SQLite library linked to Python, before Python sees the number. The Python
host used for this crate reports SQLite 3.53.4; Rust's bundled SQLite is 3.53.2.
The cross-runtime test reports both versions and each side's raw
`SELECT json_object('x', 1791788985.5209818)` result on failure.

The Rust contract is to preserve the stored binary64 value and format its REAL
with the shortest round-trip decimal and Python `json.dumps` exponent spelling.
This matches Python when its SQLite produces the full-precision JSON number.
Against a Python runtime linked to SQLite older than 3.53.0, SQLite can round a
REAL to 15 significant digits before Python parses it. When Rust rewrites that
event, its payload/hash can consequently differ once; sync clients observe an
entity change, with no loss of the stored REAL value. This does not create a
hash loop: the event trigger updates a row only when its hash or deleted flag
changes (`codex_sync_entities.py:467`), and subsequent identical writes produce
the same Rust payload/hash. `put()` likewise returns unchanged when the
projected hash and deleted flag already match (`codex_sync_entities.py:485-499`).

The cross-runtime test
`python_fixture_round_trips_through_rust_and_python_write_rollback_gate`
compares exact payload text and hash only for fixed values expected to render
identically across SQLite versions (including `0.1`, `1e21`, `5.0`, an
integer converted by REAL affinity, an integer-valued REAL, negative values,
and exponent spelling). It separately
checks that Rust preserves the binary64 values for `1791788985.5209818`, a
17-significant-digit value, and the value rounded into REAL affinity above
2^53; Python's numeric result may be rounded to 15 significant digits. The
unit test `high_precision_event_projection_keeps_the_stored_binary64_value`
pins the Rust output independently. No assertion uses the current time.
Non-ASCII/control text and nested values are also covered.

Other record JSON has a semantic, not byte-level, compatibility contract.
Python `Runtime._put` stores `json.dumps(record)` (`codex_runtime.py:3333-3350`);
Rust repositories use `serde_json::to_string` (`records.rs:305-318`). The
cross-runtime test confirms nested values and booleans survive reads and
rewrites, but record bytes intentionally differ: Python inserts separator
spaces, escapes non-ASCII by default, and keeps dict insertion order; Rust
emits compact JSON with literal UTF-8, and flattened extra fields are ordered
by `BTreeMap`. Rust's typed task `created` float also writes `1e-7` where
Python's `json.dumps` writes `1e-07`. Consumers must parse these record
columns as JSON; raw record serialization is not a canonical byte format.
Event sync payload bytes remain a separate exact compatibility contract.

The consumer check searched the server runtime and Rust store for SQL selecting
raw `record` values followed by byte equality or hashing. Ordinary `_put`
change detection compares decoded Python objects (`codex_runtime.py:3333-3350`),
and query/index consumers use parsed JSON fields. One deliberate raw-text
comparison exists in the task payload migration: `codex_payload_migrate.py:139-142`
compares the current record bytes with the staged source bytes as an optimistic
concurrency guard. It does not hash or serve raw record serialization. A
concurrent Rust rewrite that changes only JSON formatting could make that guard
report a conflict and retry the migration; record readers and API consumers
still parse JSON.
SQLite functions are connection-local, so a connection that changes
`runtime_events` without registering them fails with `no such function:
sync_entity_payload` or `no such function: sync_entity_hash`.

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
