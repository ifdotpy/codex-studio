# Change Contract

The sync component tests and benchmark exercise the production
[`codex_sync.SyncStore`](../codex_sync.py), which is also constructed by
[`studio_api/context.py`](../studio_api/context.py). Test and benchmark databases
are temporary files. They must not open Studio's live state or start native
sessions. The HTTP handlers and browser protocol contracts remain in the root
integration suites.

# UI synchronization

The normative client and server behavior is documented in
[`docs/sync.md`](../../../../../../docs/sync.md) and
[`docs/sync-protocol.md`](../../../../../../docs/sync-protocol.md). The removed
The former sync-store prototype was divergent; it is not an alternate implementation or source of contract.

The browser receives typed protocol-3 resource notifications over
`/api/sync/stream` and pulls affected projections through `/api/sync/pull`.
Each connection sends an initial resource baseline, then only changed references.
Reconnect and explicit resume reconcile active subscriptions. The server has no
generation polling route or legacy unversioned/transcript stream. These resource
events are transient invalidations, not durable projection cursors.

## Checks

The [message delivery scenario](benchmarks/message_delivery/README.md) measures
synthetic agent messages reaching an HTTP client through sync invalidation and
pulls or the direct transcript read path.

```sh
python3 -B -m unittest discover -s workspaces/runtime/apps/server/src/sync/tests -v
python3 -B workspaces/runtime/apps/server/src/sync/benchmarks/benchmark.py --check
python3 -B workspaces/runtime/apps/server/src/sync/benchmarks/benchmark.py --iterations 300
```

The isolated benchmark reports p50/p95/p99 pull latency, serialized payload
bytes, and snapshot build count for analytics-only churn and real state writes.
It verifies that analytics-only commits emit no state documents and each UI
write emits one changed state document. Production invalidation uses SQLite
`data_version`, so the measured rebuild count includes the effects of unrelated
commits; it is not a scoped-invalidation benchmark. Results do not represent a
live database, native session, or production latency guarantee.
