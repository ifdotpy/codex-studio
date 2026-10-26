# Startup session costs and HTTP admission

Initial base: `ade3a475a7b8dc25ff7dd9c02b205859b51d91a2`.
Implementation base: `4ce9d06255e384f29f977c7fb8d4c94623f30d98`.
The implementation base contains the other operator's receipt-scope batching.
This change preserves that batching and changes HTTP startup admission only.
Task: L (`work-1792979604027580928-2`).

## Cause

`runtime-server/src/main.rs` binds the listener before it initializes the store.
It awaits `HttpState::initialize_bounded_entities` before `router` and
`axum::serve`. A connection can enter the listener backlog during this interval,
but HTTP service does not start.

`bounded_entities::start_with_pricing_fetch` awaits `publish_session_roots` for
all lead chats. That function awaits one blocking closure that computes every
session estimate. One slow estimate delays every HTTP route before service starts.

`costs::session_cost_inner` opens an independent read-only analytics connection
through `AnalyticsRepository::open_read_only`. It reads `analytics_cost_groups`,
then reconciles eligible local Claude journal receipts. `cost_groups` calls
`scope_rows` for the affected agents. In the initial base, its turn/response
predicates use the scope index prefix `(root, agent)`. Each turn can scan the
agent's complete history. Commit `4ce9d0625` batches these scans per agent.
An agent with model-less rows requires the complete agent scope instead.

## Ownership during computation

| Owner                      | State during the session cost closure                                                                                                                    |
| -------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Canvas reader pool         | Capacity 15, plus one separate version reader. Root and account reads return their permits before cost computation.                                      |
| Canvas writer              | One serialized SQLite actor, at most 16 admitted jobs. The estimate does not own a writer transaction. Entity writes start after the closure returns.    |
| Analytics writer           | Separate connection behind `RuntimeStore::analytics`, a standard mutex. The estimate opens another read-only connection and does not acquire this mutex. |
| `COST_PUBLICATION`         | A process-wide asynchronous mutex for account cost snapshots and scan admission. `publish_session_roots` does not acquire it.                            |
| Entity maintenance mutex   | `lock_entity_projection_maintenance` serializes the cold legacy entity pull path. It does not enclose session cost computation.                          |
| Bounded publication worker | One ordered receiver. Startup awaits costs before the receiver starts. Later session work also delays other work in this receiver.                       |
| Server-sent events (SSE)   | The stream reads saved entity scopes through Canvas readers. It does not calculate session costs. At startup it cannot run before HTTP service starts.   |

The global HTTP outage comes from startup ordering. Reader capacity and a writer
lock do not explain this interval in the inspected revision.

## Read-only clone measurement

Measured on 2026-10-26 UTC. No live database connection or backend change.
`cp -c` cloned `analytics.sqlite3` and its existing WAL and shared-memory files
into a private temporary folder on APFS. Python opened only the clone with
`mode=ro` and `PRAGMA query_only=ON`. The separately cloned files do not establish
an atomic snapshot of the live writer. These measurements describe the clone.

| Query                                                                      | Result                                                        | Duration                               |
| -------------------------------------------------------------------------- | ------------------------------------------------------------- | -------------------------------------- |
| Cost row count                                                             | 1,803,836                                                     | 6.3142 seconds                         |
| Aggregate group count                                                      | 1,051,580                                                     | 2.6234 seconds                         |
| Largest `(root, agent)` history                                            | 69,925 rows                                                   | 1.4619 seconds for the top-three query |
| One modeled turn through the `scope_rows` predicate, `count(*)` projection | 171 rows, about 1,348,000 SQLite virtual-machine instructions | 7.5834 seconds                         |
| Aggregate groups for that root, covering-index `count(*)`                  | 35,204 groups, about 105,000 instructions                     | 0.2597 seconds                         |

`EXPLAIN QUERY PLAN` for the sampled scope uses
`analytics_cost_rows_scope (root=? AND agent=?)` for both the outer query and its
`affected` subquery. It does not select the exact turn through the index.
The count projection isolates predicate work and omits cost row decoding.
These are individual clone measurements, not a reproduction of the reported
22-minute live interval or a complete session estimate benchmark.

## Regression

`bounded_entities::tests::startup_serves_http_while_large_session_cost_publication_is_pending`
creates 32,768 synthetic cost rows and their aggregate in isolated temporary state.
A test-only channel gate pauses the actual session cost worker before its
analytics read. Each gate belongs to one temporary state folder.
The test uses the production startup order and the real authenticated HTTP router.
It requires `GET /api/desktop` to respond before the analytics gate opens.
It then releases the worker and requires the saved session estimate to include all
32,768 samples with the expected price. The gate makes the result independent
of CPU speed. This is an isolated correctness witness, not a contention load run.

## Change

Startup reads saved session estimates from Canvas. A new root receives a
`loading` entity with an unknown total. A removed root receives a tombstone.
Startup queues the initial full cost refresh through the existing ordered
publication worker and returns. The HTTP server can then start while estimates
use the analytics connection. Completed estimates update the saved entities
and reach the client through the existing SSE path.

The existing cost algorithm and the batching in `4ce9d0625` remain unchanged.
Later work still uses the existing ordered publication worker. A slow session
estimate can delay other background summaries in that worker, while HTTP reads
continue to use saved entities.

## Validation

Before the startup change, the regression fails on `4ce9d0625` at:
`HTTP must serve before analytics cost publication finishes: Elapsed(())`.
The cost worker reaches the gate first. The HTTP deadline expires after two
seconds. Releasing the worker lets the request complete.
Monitor: `88fef06e-8ef9-5739-879d-fde90642c99b` (exit 101).

The final regression also requires an initial SSE event with pending costs
before the gate opens, followed by an SSE event with all priced samples.
A separate seed test checks saved estimates, missing roots, and removed roots.
The focused run passes all 23 `bounded_entities::tests` cases after the change.
Command: `CARGO_INCREMENTAL=0 cargo test -j 4 --locked -p studio-runtime-http --lib bounded_entities::tests:: -- --nocapture`.
Monitor: `c0e544b4-2cf5-511c-a215-549280fdde56` (exit 0).
The test process takes 5.76 seconds, excluding compilation.

`CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=4 just test-fast` fails after 499.166 seconds.
The workspace build, generated contracts, and all 23 bounded publication cases pass.
Rust: 1,150 passed, four failed. Web: 1,063 passed, three failed, one skipped.
The integration owner identifies these seven failures as inherited from
`4ce9d0625` and owns their correction on the integrated head:

| Case                                    | Failure                                      |
| --------------------------------------- | -------------------------------------------- |
| `platformNeutralCopy`                   | Removed `IMAGE` mode still expected          |
| `Usage.eligibility`                     | Mock lacks `useServerSync`                   |
| `ExecutionSettings.receipt`             | Expected POST does not occur                 |
| VM old-thread catalog                   | Native thread expectation differs            |
| `mac_sync` handoff replay               | HTTP 400, expected 200                       |
| `studio_errors` dismissal               | Zero tombstones, expected two                |
| `neutral_commits_enqueue_no_cost_roots` | Fixture writes `analytics_items`, now a view |

Gate logs: `~/.cache/codex-studio/checks/run-1792981259197-15z5VP`.
Monitor: `738e84e1-e6da-51f9-8841-ed79a650dd78` (exit 1).

Three Python helper steps initially select Python 3.9 from the host tools.
The follow-up helper runs use `/opt/homebrew/bin/python3` through `PATH`.
The guest run also sets `STUDIO_GUEST_PYTHON` to that executable.
The retained helper and recovery steps pass all three witnesses.
Monitor: `c114850c-e1cc-5133-9b6d-4805ba0102f0` (exit 0).
The guest suite reports 165 passed, zero failed, and nine skipped.
Monitor: `8cdc1507-4688-5d67-9294-a5a0d5da074d`.
Python: 3.15.0. No additional source changes form part of these reruns.
No live backend, live provider request, package installation, or deployment
forms part of this evidence.
