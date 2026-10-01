# Payload storage and retention measurement

## Scope and sample

Measured on 2026-10-01 with `scripts/codex_payload_measure.py` against the live database using SQLite read-only mode. The database file measured 42,653,687,808 bytes and its filesystem had 48.33 GB free at measurement time. The sampler probes 500 evenly spaced rowids per table, reads only those records, parses the JSON, and reports UTF-8 serialized byte sizes. This is a sample, not a full scan. Approximate row counts below use each table's maximum rowid.

The externalization cutoff is 65,536 bytes. The sample shows that checkpoint `items` is commonly large (p50 174 KB) and 69.9% of sampled values exceed the cutoff. In contrast, only about 3% of sampled tool payloads exceed it. A 64 KiB cutoff captures most checkpoint bytes while avoiding a filesystem object for the many small tool results. `runtime_items.text` is capped at about 24.8 KB in this sample and task `tail` at about 14 KB; both remain inline.

| Table and field | Samples | p50 | p95 | Max | >4 KiB | >64 KiB | Estimated DB bytes removed |
|---|---:|---:|---:|---:|---:|---:|---:|
| `runtime_checkpoints.items` | 495 | 174,434 B | 1,131,340 B | 1,723,525 B | 95.2% | 69.9% | 2.132 GB |
| `runtime_tool_requests.result` | 500 | 818 B | 63,340 B | 810,507 B | 25.4% | 3.2% | 675.2 MB |
| `runtime_tool_results.contentItems` | 500 | 626 B | 51,160 B | 1,257,779 B | 23.4% | 2.8% | 854.3 MB |
| `runtime_tasks.tail` (retention) | 423 | 1,706 B | 12,620 B | 23,879 B | 39.2% | 0% | 1.527 GB |
| `runtime_items.text` (kept inline) | 500 | 2,166 B | 20,478 B | 22,357 B | 34.0% | 0% | 0 B |

Projections multiply average sampled eligible bytes by maximum rowid: checkpoints 6,781; tasks 623,867; tool requests 102,667; tool results 102,544; items 782,738. Task projection applies the seven-day/newest-100 policy and assumes a 512-byte trailing preview. Gross estimated JSON reduction is about 5.19 GB before JSON reference overhead, SQLite page fragmentation, and VACUUM. It does not estimate filesystem allocation or deduplication savings. The task sample is an 800-row age/status-aware sample; other listed distributions use 500 probes.

Other table-level record size distributions (p50 / p95 / max) were: `runtime_checkpoints` 175,714 / 1,136,111 / 1,744,862 B; `runtime_tasks` 2,244 / 13,442 / 24,610 B; `runtime_tool_requests` 1,876 / 64,324 / 811,100 B; `runtime_tool_results.result` 668 / 51,796 / 1,257,983 B; `runtime_items` 2,616 / 20,890 / 26,202 B.

## Hot-path field reads

- Snapshot calls `recent_tasks`; snapshot and workspace task feed remove `tail`, `arguments`, and `error` in SQL before decoding. Task detail returns full task output. Retention preserves a short suffix for older task rows.
- Transcript page reads selected `runtime_items` records and, for `inputs` entries, embedded/event text. Item text remains inline and transcript history is not subject to retention.
- Entity sync reads task records; retention updates use the normal task sync write path when entity sync metadata is present.
- Search indexing reads runtime item `text`, `title`, and `kind`, plus runtime event text or embedded transcript input text. This change does not move item text or alter `runtime_item_fulltext`; the agreed batch search accessor continues to see the same values.
- Checkpoint summary paths omit `items`; restore/history paths resolve them. Tool request/result list paths omit full results; receipt, get, output, and recovery paths resolve them.

## Storage and crash safety

Payloads above the cutoff are stored under `state/blobs/aa/bb/<sha256>`. JSON rows retain a content reference, byte size, and preview. Writes check free space with a 64 MiB reserve, write to a temporary file in the destination filesystem, fsync the file, atomically link it into its content-addressed name, then fsync the containing directory. A per-state shared/exclusive flock coordinates DB reference updates with collection: writers keep a shared lock through the SQLite transaction; GC holds the exclusive lock. Triggers maintain the indexed `runtime_payload_refs` set. GC deletes only indexed-unreferenced blobs older than seven days. A crash before the row commit can leave an orphan, but cannot expose a dangling committed reference; the grace period protects concurrent and in-flight writers.

The migration prewrites blob files before a short `BEGIN IMMEDIATE`, then compares the source row to its captured bytes before replacing it and advancing a durable table cursor in the same transaction. It resumes from that cursor after restart. A rerun is idempotent. Default batch size is eight rows with a 2 MiB target payload limit; task retention is deliberately one task per transaction because those rows also have renderer sync indexes. The migration writer waits at most 40 ms to acquire SQLite's write lock, then yields and retries outside a transaction. Free space is checked before every batch and each blob write. Disk space is not returned to the filesystem merely by deleting JSON from SQLite: SQLite pages become reusable, and reducing the database file requires a separately scheduled `VACUUM` with adequate free space. No live migration or VACUUM was performed for this task.

## Partial-copy benchmark

`tests/payload-storage-benchmark.py` made a temporary WAL-mode partial database copy (1,000 items, 5,000 tasks, and sampled checkpoint/request/result rows), exercised the migration and actual transcript-page, task-feed, and snapshot readers, then removed the copy. It moved 166,197,814 bytes in 6.015 s (27.6 MB/s). Across migration batches, lock duration was p50 0 ms, p95 0.109 ms, max 31.248 ms; per-table maxima were checkpoints 22.082 ms, tasks 31.248 ms, tool requests 4.100 ms, tool results 4.281 ms. This run stayed below the 50 ms target. The maximum remains host and filesystem dependent.

Warm read latency in milliseconds, before to after migration (p50 / p95): transcript page 3.228 / 4.826 to 2.887 / 9.459; task feed 1.854 / 6.757 to 1.687 / 3.749; snapshot 125.711 / 231.867 to 148.265 / 317.621. These are small local fixture measurements with noisy tail latency, not a production load test. Snapshot and task feed omit externalized fields by design.

## Rollout

1. Deploy the code and start the normal server on its existing state directory. Schema setup adds the reference table and triggers without rewriting payload rows.
2. Check available space on the state filesystem. The migration refuses to grow payload storage when the 64 MiB reserve would be crossed; allow room for projected blob bytes plus continued database growth and filesystem overhead.
3. Run `python3 scripts/codex_payload_migrate.py --state-dir <state-dir> --table all` while the server remains online. The default batches contain eight rows for blob-backed payloads and one row for task retention. Monitor emitted moved-byte and lock-time metrics; the migration yields on write contention and resumes safely.
4. Run `python3 scripts/codex_payload_migrate.py --db <state-db> --state-dir <state-dir> --table tasks --restart-tasks` on a schedule to apply retention as rows age. Run with `--gc` after the seven-day orphan grace period to collect unreferenced blobs.
5. After backup and operational review, schedule SQLite `VACUUM` separately if reclaiming the database file is required. Do not run it as part of the online migration; it needs additional disk and may cause substantial I/O.

Expected logical SQLite payload reduction is approximately 5.19 GB from this sample extrapolation. Actual file shrink occurs only after VACUUM and may differ due to JSON metadata, page reuse, field distribution, and duplicate content.
