# UI synchronization

`sync.sync_store.SyncStore` owns SQLite-backed pull checkpoints for the browser's
RxDB projections. The canvas HTTP handler wires its pull, draft push, identity,
and shared SSE endpoints to this production component. It does not own message
receipts or scheduler observations.

## In simple terms

Studio keeps a local copy of chats so they still open offline. The server sends
a small “something changed” signal, and the browser asks for only the changed
part. Chat lists, transcripts, and drafts each have a counter. An analytics
search index update changes none of those counters, so it does not make every
open chat reload its large chat list. An actual chat or message change advances
the counter for the data it can affect.

A browser reconnect starts by receiving all current counters. It compares the
counter names with its active subscriptions, then pulls those parts. A browser
using the older stream format still receives a broad “check again” signal, so
an older cached interface remains safe during rollout. Offline RxDB data,
pull checkpoints, transcript tombstones, browser-owned draft branches, draft
conflict handling, and immutable message request IDs remain unchanged.

## Generation ownership

- `sync_generation` is the historical broad SQLite write counter read by the
  runtime scheduler through `write_generation`. Its triggers remain broad.
- `sync_scope_generation.state` advances for the explicit UI state tables,
  including `analytics_limits` because those records supply visible rate limits.
  Legacy app-server thread status-file signatures and launcher-process liveness,
  plus volatile connection state, rate-limit maps, and native connection IDs,
  are sampled and advance this UI counter directly; they do not advance the
  scheduler clock. Lock samples are nonblocking, so a busy native startup defers
  observation to the next SSE/poll tick. `runtime_items` advances state only
  when its first or last row for an agent changes the snapshot's empty-lead
  eligibility; this checks for a sibling through the agent index.
- A pull whose runtime snapshot cannot capture connection permissions because
  native startup holds `start_lock` returns a retryable HTTP 503. It does not
  cache a partial snapshot; retrying after startup returns the current view.
  Snapshot lock acquisition never waits while holding the other runtime lock.
  Team roster tools and chat-membership validation read committed database rows
  directly and remain available during native startup without capturing UI
  connection permissions. The full snapshot reuses the same public agent
  projection inside its pinned database view.
- `sync_scope_generation.transcripts` advances for records used to build
  transcripts and their pending user receipts, including durable
  `runtime_item_bodies` once that table is installed. The derived,
  body-free search queue and its `runtime_search_partial`,
  `runtime_search_item_cursor`, `runtime_search_indexed`, `runtime_search_rows`,
  `runtime_search_address_cursor`, and `runtime_search_deletions` index/migration
  tables are excluded.
- `sync_scope_generation.drafts` advances only when a draft document changes.
- Analytics usage/history, all `runtime_search_*` derived/index metadata tables,
  and unrelated FTS writes own no UI generation. Adding a new UI projection source
  requires adding it to the trigger map and its migration test.

Startup upgrades existing databases idempotently: it creates missing counters
and reconciles the broad and scoped triggers against their current definitions.
Correct triggers stay in place; missing triggers are added, and changed or
obsolete sync-owned definitions are replaced. A schema change in an unrelated
table therefore does not rebuild the scoped trigger set. When it discovers a
new source table, it also invalidates the affected scope if rows were committed
before its triggers could be installed. Startup does the same when it repairs a
missing or changed scoped trigger for a populated source, covering rows added
while Studio was stopped.

The browser receives typed protocol-3 resource notifications over
`/api/sync/stream` and pulls affected projections through `/api/sync/pull`.
Each connection sends an initial resource baseline, then only changed references.
Reconnect and explicit resume reconcile active subscriptions. The server has no
generation polling route or legacy unversioned/transcript stream. These resource
events are transient invalidations, not durable projection cursors.

## Checks

```sh
python3 -B -m unittest discover -s scripts/sync/tests -v
python3 -B scripts/sync/benchmarks/benchmark.py --check
python3 -B scripts/sync/benchmarks/benchmark.py --iterations 300 --output /tmp/sync-benchmark.json
```

The component checks use temporary synthetic SQLite databases. The benchmark
calls the production `SyncStore` and reports p50/p95/p99 pull latency, payload
bytes, snapshot build count, and scoped invalidation count under analytics churn
and actual UI-state writes. It does not connect to a live state directory or
native session. HTTP/SSE and RxDB caller integration belongs to the root-level
integration contracts.
