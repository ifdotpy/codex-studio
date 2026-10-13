# studio-runtime-store

## Change Contract

- **Responsibility:** Open an existing Studio state directory in place, exclude
  other runtime processes with `runtime.lock`, validate supported repository
  requirements, prepare the sync schema family, and provide synchronous typed
  reads and writes.
- **Public interface:** `RuntimeStore::open`, `RuntimeStore::read`, and
  `RuntimeStore::transaction`; `JsonRecord` and `Table<R>`
  for JSON-record tables; repositories for agents, tasks, queue events, and
  chat messages.
- **Boundaries:** No Tokio, Python, general schema migration, or state
  directory creation. Sync DDL is sourced only from `src/sync_schema.sql`.
  Python persisted models and the OpenAPI document own the schemas.
- **Invariants:** One exclusive runtime lock, one serialized writer using
  `BEGIN IMMEDIATE`, independent query-only read connections, WAL and FULL
  synchronous writes, and fail-closed checks for required tables, columns,
  triggers and migration-marker meaning. `Table<R>` validates records against
  their named schemas on reads and writes, checks declared key modes, and
  reports invalid records with one table/key error while preserving extension
  fields. Repositories declare whether each table is required or conditional;
  bootstrapped feature tables are required after `RuntimeStore::open`.
- **Configuration owner:** `codex_state.py` currently defines state path
  resolution; this crate mirrors its environment and platform rules. The
  committed `runtime-state-schema.json` describes SQLite catalog requirements;
  JSON-record schemas are generated under `runtime-schema/src/schemas`.
- **Focused check:** with the isolated Python environment prepared, run
  `XDG_CACHE_HOME=/var/tmp/cs-rust-store/cache XDG_STATE_HOME=/var/tmp/cs-rust-store/state TMPDIR=/var/tmp/cs-rust-store/tests CARGO_TARGET_DIR=/var/tmp/cs-rust-store/target PYTHON="$(XDG_CACHE_HOME=/var/tmp/cs-rust-store/cache python3 workspaces/runtime/apps/server/src/codex_python.py)" cargo nextest run -p studio-runtime-store`.
  Python subprocesses create disposable state under the system temporary
  directory (the command sets it to `/var/tmp/cs-rust-store/tests`).

## JSON-record table scope

- Covers only records stored as one `id` key and one JSON `record` value.
- Validates named schemas and row identity through `Table<R>`; repository
  contracts declare required and conditional tables and external-key checks.
- Preserves raw JSON text only for typed readers that depend on original
  property order.
- Offers ID access, fixed agent filters and declared deterministic orderings.
- Keeps composite-cursor paging and response shaping in their repositories and routes.
- Keeps normalized tables, joins, projections, counts, FTS and sync SQL specialized.

Python connection behavior reproduced in this slice:

| Behavior                                                                                                                         | Python source                                                                  |
| -------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------ |
| Main DB connection timeout of 15 seconds (`busy_timeout=15000`)                                                                  | `codex_runtime.py:2017`                                                        |
| Read-only `read_db()` connection and `PRAGMA query_only=ON`                                                                      | `codex_runtime.py:2661-2665`                                                   |
| Analytics connection timeout and `PRAGMA journal_mode=WAL`                                                                       | `codex_runtime.py:2686-2690`                                                   |
| Analytics read `PRAGMA query_only=ON`                                                                                            | `codex_runtime.py:2699-2702`                                                   |
| Temporary connection `PRAGMA busy_timeout` changes                                                                               | `codex_runtime.py:2184-2185, 2878-2881`                                        |
| Startup WAL setup for the primary DB                                                                                             | `codex_runtime.py:1597`                                                        |
| `synchronous=FULL` observed on both DBs by the fresh-start snapshot; Python does not explicitly set it in these connection paths | `codex_runtime.py:2017, 2686`; `codex_analytics.py` has no SQLite PRAGMA calls |

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

State-directory table inventory, grouped by family (exact names from the
committed snapshot; it includes both typed-record and specialized SQL tables):

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
Each repository is a small type that uses `Table<R>` for JSON-record families
and maps normalized SQL row families explicitly. Repository-specific methods
retain projections and joins that do not fit the single-record API. Each
implementation lives in
`src/repositories/<family>.rs` and exports `REQUIREMENTS: SchemaRequirements`
beside its SQL methods. The declaration lists every table and column it uses,
required triggers and connection-local SQL functions its writes need, plus conditional
requirements such as transcript triggers when the transcript table exists.
`schema::verify` aggregates these declarations via the single
`repositories::REQUIREMENTS` list. Add a module, declare its contract next to
its SQL, and add one line to that list; do not centralize family-specific
schema knowledge in `schema.rs`.

The fixture schema used by the Python drift guard is captured from a new
`Runtime` before `Canvas` is initialized. It therefore omits
`graph_agents`, `graph_edges`, `groups`, `messages`, and `messages_room`, which
`Canvas.__init__` creates on every real server start. The Python guard is blind
to changes in those Canvas-owned objects. Recommendation for a future guard
change: capture the canonical snapshot after full server startup, including
`Canvas`; keep a separately named Runtime-only fixture only if focused tests
need it. This slice does not change the Python guard or snapshot.

`RuntimeStore::open` verifies the Python-owned core schema, then prepares the
sync schema family in one immediate transaction, and finally verifies the
repository requirements that bootstrap provides. The Rust store owns this
family from first open onward (one-way move). Callers can rely on a successful
`RuntimeStore::open`; no separate preparation call or prepared-state gate is
needed. Its only DDL source is
`src/sync_schema.sql`; bootstrap builds a scratch catalog from that file.
Existing sync tables are compared structurally using `table_xinfo` and
`table_list`, plus the automatic `UNIQUE` and `PRIMARY KEY` indexes reported by
`index_list` and `index_xinfo`; a mismatch identifies the table and differing
column or constraint. Explicit indexes and triggers whose stored SQL differs
from the scratch catalog are replaced inside the bootstrap transaction. No
owned sync table currently has a `CHECK`, foreign key, or generated column;
`table_xinfo` reports generated columns, and a test guards against adding
`CHECK` or `REFERENCES` clauses before extending structural comparison. The
committed live-state fixture has the expected table structures. Existing sync
rows are preserved; absent identity, generation, and tombstone-count seed rows
are inserted only when missing. Only
`runtime_items` is optional: `runtime_events` and `runtime_agents` are checked
by the core schema before bootstrap. Transcript item triggers are created when
`runtime_items` exists. Event triggers use the registered
`sync_entity_payload` and `sync_entity_hash` connection functions. Legacy
triggers with the literal, case-sensitive `sync_watch_` prefix are removed.
The DDL text matches the current Python producer's stored index and trigger
SQL; first opens of both a Python-prepared database and the committed live-state
fixture replace zero derived objects.

### What the table check does and does not establish

The table signature supports sync tables created by the product's historical
Python writers or by this Rust bootstrap. It detects a different column layout
and missing or extra primary-key and unique constraints; it is not a proof of
full SQLite table equivalence. Equivalent declarations such as `VARCHAR` in
place of `TEXT`, `DEFAULT '0'`, `DEFAULT +0`, or `DEFAULT ((0))` in place of
`DEFAULT 0`, and reordered columns are refused. These forms can arise from a
manual rebuild; no historical product writer creates them, and this code does
not normalize type or default spellings.

The signature does not detect a removed `AUTOINCREMENT`, `ON CONFLICT REPLACE`,
a non-key column collation, or added `CHECK`/foreign-key clauses. No product
version creates those alternatives, and they are outside this deliberately
limited check. The source guard prevents adding `CHECK` or `REFERENCES` to the
owned DDL until comparison is extended. A database refused by the structural
check is intended to be reported by the pre-cutover state check, which can call
the read-only `bootstrap::inspect_sync_tables` function to get all findings;
the pre-cutover command is outside this change. The function builds the
comparison from `sync_schema.sql` and does not write to its connection.

Opening can write this schema on its first run. The store has no read-only open
path today; if a future read-only state checker is added, it must report an
unprepared sync family without invoking bootstrap.

This bootstrap owns only sync tables and related indexes/triggers. Other
lazily created families (including Canvas graph tables, analytics projections,
execution state, and search indexes) remain owned by their existing creators;
the feature and migration inventory is in
[the state-format inventory](../../../../docs/runtime-state-formats.md#feature-and-migration-schema-beyond-a-fresh-start).
The one-time cutover upgrade is not part of this crate method.

The Python schema-drift guard owns the fresh-start snapshot at
`workspaces/runtime/apps/server/tests/fixtures/runtime-state-schema.json`.
Runtime-store's crate tests reference that same file; the opener does not
compare the entire catalog. See [the state-format inventory](../../../../docs/runtime-state-formats.md#feature-and-migration-schema-beyond-a-fresh-start)
for optional creator families and observed migration differences. When the
Python runtime is removed, move the snapshot under this crate and update the
guard in that cutover; do not move it in this slice.

The payload migration marker is optional. No table or no
`payload-v1:tasks` row means `NotStarted`; `complete=0` means `InProgress`,
and `complete=1` with a coherent status means `Complete`. The store reads but
never runs this migration. Task records remain inline JSON in every phase;
completed retention may replace an old large `tail` with a preview and
`outputTruncated` / `outputPreviewOnly` flags. Rust preserves either record
shape. `analytics_meta.fileMigrationV1` is accepted only when its JSON phase is
`complete`; an absent marker is `NotStarted`.
