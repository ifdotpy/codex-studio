# studio-runtime-store

## Change Contract

- **Responsibility:** Open an existing Studio state directory in place, exclude
  other runtime processes with `runtime.lock`, validate supported repository
  requirements, prepare the sync schema family, and provide typed reads plus
  asynchronous Canvas writes.
- **Public interface:** async `RuntimeStore::open` and `RuntimeStore::transaction*`,
  synchronous `RuntimeStore::read` and `RuntimeStore::read_transaction*`, and
  async writer checkpoint/diagnostic operations; `JsonRecord` and `Table<R>`
  for JSON-record tables; repositories for agents, tasks, queue events, and
  chat messages.
- **Boundaries:** Tokio and `tokio-rusqlite` own the Canvas writer boundary;
  each actor job runs one synchronous `rusqlite` closure. `MAX_PENDING_WRITES`
  in `src/sqlite_actor.rs` owns the queued-plus-running admission limit of 16.
  Reads still use the existing connection APIs: `CANVAS_READER_CONNECTION_CAPACITY`
  in `src/lib.rs` owns the total limit of four readers, including the reserved
  version reader; the ordinary pool derives its capacity from that owner. No
  Python, general schema migration, or state directory creation. Sync DDL is
  sourced only from `src/sync_schema.sql`.
  Python persisted models and the OpenAPI document own the schemas.
- **Invariants:** One exclusive runtime lock, one serialized Canvas writer
  using `BEGIN IMMEDIATE`, independent query-only read connections, WAL with
  `synchronous=NORMAL` and a periodic flush (`sqlite_durability`), and fail-closed checks for required tables, columns,
  triggers and migration-marker meaning. `Table<R>` validates records against
  their named schemas on reads and writes, checks declared key modes, and
  reports invalid records with one table/key error while preserving extension
  fields. Repositories declare whether each table is required or conditional;
  bootstrapped feature tables are required after `RuntimeStore::open`.
- **Configuration owner:** `codex_state.py` currently defines state path
  resolution; this crate mirrors its environment and platform rules. The
  committed `runtime-state-schema.json` describes SQLite catalog requirements;
  JSON-record schemas are committed source under `runtime-schema/src/schemas`.
- **Focused check:** run `CODEX_AGENTS_PYTHON=/nonexistent cargo test -p studio-runtime-store`.
  Tests create disposable state through Rust; historical record data is frozen in
  `tests/fixtures/record-round-trip.json` and decoded into typed records.

## Canvas writer actor

The Canvas connection has one async owner. Each submitted operation is a
synchronous transaction closure with owned `Send + 'static` inputs and output;
SQL transactions and borrowed rows do not cross an `await`. A bounded admission
permit remains with the queued or running callback if its caller cancels. A
caller cancelled before admission submits no SQL; after submission, the job may
still commit and its result is unknown to that caller. Receipt-backed retries
must use the original identity and content and are never automatic.

The actor holds the `RuntimeLock` lease through every queued or running
callback. After the last callback returns, dropping the sender leaves only
`tokio-rusqlite`'s library-managed native connection-close tail; application SQL
and callbacks do not run after the lock and identity leases end. The analytics
database remains under its separate writer owner, and the dedicated version
reader remains separate from the ordinary Canvas read pool. Raw read API
migration and removal of synchronous read leases are later work. See the
[SQLite actor migration preflight](../../../../docs/plans/rust-sqlite-actor.md)
for the full cutover and acceptance contracts. The production caller cutover
is not claimed complete by this package description.

`ObservedTable` callbacks are process-local invalidation hints. They fire after
a known successful commit or an ambiguous commit that may have landed, run
outside the transaction, and require the consumer to reread authoritative
state. They carry no receipt and must never trigger a replay. A lost response
after enqueue remains an unknown outcome; receipt-backed retry uses the
original identity and content.

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

Rust checks WAL before opening the writer, then sets `synchronous=NORMAL` on the
writer and analytics connections and refuses either without it; see
[the flush decision](../../../../docs/decisions/2026-10-24-sqlite-normal-with-periodic-flush.md). Reads are opened read-only and set
`query_only=ON`. Its standard busy timeout is 15 seconds. Event sync triggers
depend on Python's connection-local `sync_entity_payload` and
`sync_entity_hash` functions; the writer registers compatible event projections
and SHA-256 encoding before writes. The bundled SQLite is 3.53.2 (the version
in `libsqlite3-sys`'s bundled header); this host's Python SQLite is 3.53.4.
Historical Python database compatibility is covered by committed fixtures;
tests execute only the Rust writer.

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

The record fixture round-trip test checks that Rust preserves adversarial
binary64 event timestamps, extension fields, non-ASCII/control text, and nested
values. Its fixed historical records are committed as JSON; Rust seeds the
current schema and verifies writes and reopen behavior. The unit test
`high_precision_event_projection_keeps_the_stored_binary64_value` pins the
Rust output independently. No test launches the historical Python writer.

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
  `analytics_turns`, `analytics_usage`, `analytics_usage_roots`. Rust adds
  `analytics_cost_rows` and `analytics_cost_groups`, the incremental per-root
  session-cost aggregate (`repositories/cost_aggregate.rs`) that every usage
  write updates in its own transaction; `/api/session-cost` prices its groups.

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
Each repository submits a `SchemaRegistration` beside its contract;
`schema::verify` sorts these inventory entries by repository name before its
startup checks. Add an explicit alphabetical module line and a local schema
registration with each repository; directory and requirement parity tests
catch omissions. Keep public imports on `repositories::<family>` paths when
migrating consumers; the existing crate re-exports remain during migration.
Bootstrap parent tables and supported connection-local SQL functions are
inventory registrations too, each collected in stable name order.

- No build-script scanner: explicit declarations keep modules visible to Rust tooling.
- No `automod`: its upstream is archived, and rustfmt skips macro-declared modules.

The committed catalog snapshot is owned by this crate. Fresh-state tests create
both databases through `RuntimeStore::open`, compare their complete catalogs
across independent roots, and verify that reopening preserves them. The tests
require no Python interpreter or backend source files.

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
The event trigger SQL is generated from the Python projection contract. A
current Python-prepared database opens without replacing derived objects. The
committed older live-state fixture upgrades its three event triggers atomically
while preserving its source records and sync rows.

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

This crate owns the fresh-start snapshot at
`workspaces/runtime/packages/runtime-store/src/runtime-state-schema.json`.
Fresh-state bootstrap and the crate tests read that file; the opener does not
compare the entire catalog. See [the state-format inventory](../../../../docs/runtime-state-formats.md#feature-and-migration-schema-beyond-a-fresh-start)
for optional creator families and observed migration differences.

The payload migration marker is optional. No table or no
`payload-v1:tasks` row means `NotStarted`; `complete=0` means `InProgress`,
and `complete=1` with a coherent status means `Complete`. The store reads but
never runs this migration. Task records remain inline JSON in every phase;
completed retention may replace an old large `tail` with a preview and
`outputTruncated` / `outputPreviewOnly` flags. Rust preserves either record
shape. `analytics_meta.fileMigrationV1` is accepted only when its JSON phase is
`complete`; an absent marker is `NotStarted`.

## Scoped analytics cost notifications

`add_analytics_root_change_observer` reports the roots written to
`analytics_usage_roots` by a committed usage-generation change. Both analytics
transaction adapters collect bounded row hints and resolve roots after commit.
Rollback emits nothing; unknown scope, deleted roots, hint overflow, or uncertain
commit emits `ChangedRoots::Full`. The existing unscoped commit observers remain
available. Observers enqueue work and never read the database within callbacks.
