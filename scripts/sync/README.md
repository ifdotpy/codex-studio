# Change Contract

The sync component tests and benchmark exercise the production
[`codex_sync.SyncStore`](../codex_sync.py), which is also constructed by
[`studio_api/context.py`](../studio_api/context.py). Test and benchmark databases
are temporary files. They must not open Studio's live state or start native
sessions. The HTTP handlers and browser protocol contracts remain in the root
integration suites.

# UI synchronization

The normative client and server behavior is documented in
[`docs/sync.md`](../../docs/sync.md) and
[`docs/sync-protocol.md`](../../docs/sync-protocol.md). The removed
`scripts/sync/sync_store.py` was a divergent prototype; it is not an alternate
implementation or source of contract.

The browser receives typed protocol-3 resource notifications over
`/api/sync/stream` and pulls affected projections through `/api/sync/pull`.
Each connection sends an initial resource baseline, then only changed references.
Reconnect and explicit resume reconcile active subscriptions. The server has no
generation polling route or legacy unversioned/transcript stream. These resource
events are transient invalidations, not durable projection cursors.

## Checks

```sh
python3 -B -m unittest discover -s scripts/sync/tests -v
```

The snapshot benchmark was retired when its callers moved to entity-scoped
pulls. Historical measurements remain in the dated verification records.
