# Transcript storage

## Change Contract

This component owns complete Studio transcript bodies and the derived SQLite
full-text search index. `runtime_items` continues to own compact display records,
timestamps, ordering, metadata, and item identity. `runtime_item_bodies` owns the
complete current body. Search rows are disposable derived data. The exact schema
and version behavior live in [`storage.py`](storage.py); the caller continues to
enforce agent, source, thread, and receipt checks. Run the component tests and
the production-code benchmark check below.

## How it works

Think of the body table as the saved page and FTS as its fast index. Every item
update saves the whole page and a pending-index marker in the same SQLite
transaction. Streaming updates replace that marker but retain the first
checkpoint deadline, so a long stream is indexed periodically instead of
postponing work forever. A completed native item writes its full text over the
streamed prefix under the same item ID and refreshes FTS synchronously.

The scheduler drains due markers, maps legacy FTS row addresses, repairs legacy
items that have no search row, and classifies truncated rows in the same bounded
item-migration pass, including rows that already have an FTS index. Startup only
creates tables and cursors; it does not scan old conversations. Each item batch
is capped at 128 rows and 2 MiB of excerpt text, with one oversized item allowed
to make progress. A truncated legacy excerpt without a recoverable full body is
marked partial and full-body readers report it unavailable. Even before
migration reaches a row, readers reject FTS text that only repeats its excerpt.
Scoped work search also reports an explicit unavailable-text error if one of
its visible legacy rows is partial. A complete replacement clears the partial
marker. A failed index write leaves
the saved body and marker intact. Restart resumes the same queues. Old items
still read their full text from the legacy FTS row when present, then fall back
to the item excerpt. While either migration cursor remains, search advances a
bounded batch and reports the same retriable indexing-pending error rather than
returning a possibly incomplete snapshot. Legacy rows still return complete
text from FTS when available; UI item metadata retains its saved excerpt.

Search force-drains one bounded batch before searching, including streaming
markers whose checkpoint deadline has not arrived. If visible work remains, it
returns the retriable error “Transcript search is indexing. Retry shortly.” It
never reports a successful but incomplete transcript result. Body reads always
return the latest committed full text and do not wait for FTS.

## Checks and benchmark

Run from the repository root:

```sh
PYTHONPATH=workspaces/runtime/apps/server/src python3 -m unittest discover -s workspaces/runtime/apps/server/src/transcript_storage/tests
PYTHONPATH=workspaces/runtime/apps/server/src python3 workspaces/runtime/apps/server/src/transcript_storage/benchmarks/benchmark.py --check
PYTHONPATH=workspaces/runtime/apps/server/src python3 workspaces/runtime/apps/server/src/transcript_storage/benchmarks/benchmark.py
```

The benchmark uses synthetic text and production persistence/index/migration
functions. It reports p50, p95, p99, and index-write counts for a burst microcase,
a continuous stream with scheduler ticks every 1 second, matching
`Runtime.schedule`. The latter checks
that FTS catches up within the 2 second checkpoint delay plus one scheduler
interval. A separate case times a migration page with long legacy excerpts.
Timing uses in-memory SQLite: it describes neither durable filesystem
cost nor end-to-end application latency, and it is not a live user's database.
