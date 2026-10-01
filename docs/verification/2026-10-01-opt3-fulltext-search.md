# Opt3 contentless search measurement

Measured 2026-10-01 with Python 3.14 / SQLite 3.53.4 on a disposable SQLite copy. The application Python reports SQLite 3.54.0, which supports FTS5 `contentless_delete=1` (minimum 3.43). The live database was opened read-only; no backend was started against it and no live state was written.

## Sample and index size

The fixture copied the first 172,149 legacy search rows in rowid order until it held 2,147,484,567 bytes of body text (2.00 GiB). The resulting fixture was removed after measurement. It contained 19,376 truncated items (11.3% of rows), whose complete bodies occupied 1,472,499,284 bytes (68.6% of sampled body bytes). This is an ordered prefix, so the truncated-item ratio is a sample estimate, not a live count.

| Allocation in fixture | Bytes |
| --- | ---: |
| Existing FTS content and index | 2,930,544,640 |
| Contentless FTS index and result map | 583,053,312 |
| Full bodies retained for truncated items | 1,472,499,284 |

Using the live `runtime_search_content` size of 7.621 GiB as the scale target gives a factor of 3.81. This projects about 2.22 GiB for the contentless index and map, plus about 5.61 GiB for the full-text rows required by the sampled truncated items. Compared with the lead's 7.621 GiB content plus 2.482 GiB FTS data, that projects a roughly 2.27 GiB (22%) reduction. The full-text table is necessary to preserve truncated bodies; the sample indicates it will retain much of the old content allocation.

## Latency and migration timings

Search query latency was measured over nine warm runs on the 2.00 GiB fixture. Each query used the existing FTS `MATCH`, `ORDER BY rank`, runtime item join and `afterRestore` filter, with a 1,000-row result limit. Times are milliseconds, p50 / p95:

| Query | Existing FTS | Contentless FTS |
| --- | ---: | ---: |
| `the` | 567.7 / 596.4 | 476.3 / 531.0 |
| `function` | 353.5 / 404.3 | 231.8 / 287.7 |
| `sqlite` | 168.7 / 258.8 | 41.3 / 140.0 |

The initial 100-row / 8 MiB build batches measured p50 126.9 ms, p95 346.3 ms, max 899.9 ms, so the rollout now uses 5 rows / 64 KiB per transaction. On a 67,259,120-byte actual-text sample, this final configuration built 1,221 batches in 6.18 s: p50 2.7 ms, p95 15.7 ms, max 135.3 ms. The maximum came from one body larger than the byte cap, which must be indexed atomically as one document. Scaling the build time by the 7.621 GiB / 67,259,120-byte ratio projects about 12.5 minutes.

Old-index cleanup was measured on 500 real sample documents: 100 transactions of 5 rows, p50 1.94 ms, p95 12.89 ms, max 23.93 ms. At 783,073 live rows this is about 156,615 transactions and 304 s of transaction time; including the worker's 1 ms yield per batch gives about 7.7 minutes. Combined build and cleanup estimate is about 20 minutes, excluding startup, verification, and unusually large documents. These are sample extrapolations; monitor the live phase and cursor.

## Rollout

1. Complete the Opt4 analytics move first and do not run the two data migrations concurrently. Recheck free space afterward. The FTS worker checks before index creation and before each index-growing batch; it waits in `waiting_for_space` below its configured reserve.
2. Before deploying, confirm the live free-space check passes the worker's initial reserve (`max(16 GiB, half the current database file size)`). With the measured 42.65 GB database and about 30 GiB free from the lead's baseline, the initial reserve is about 21.3 GiB. Confirm SQLite is at least 3.43.
3. Deploy and monitor `runtime_search_rollout.phase`, `cursor`, `search_migration_error`, and filesystem free space. The legacy index remains the query path during build. New writes update both indexes until the atomic switch.
4. Before old-index cleanup, the worker checks that the full-text row count equals the truncated-item count and compares SHA-256 hashes of a deterministic sample of up to 20 old FTS bodies against `runtime_item_fulltext`. Any mismatch prevents the switch. Search queries then use the contentless index while legacy rows are deleted in small resumable batches.
5. Do not expect the SQLite file to shrink when the old table is dropped: `auto_vacuum=0`, so pages become reusable. Run `VACUUM` only after both Opt4 and Opt3 migrations finish and a fresh free-space check shows roughly one remaining database-file size available. At the 42.65 GB baseline and about 30 GiB free, defer it until sufficient space is available. The FTS migration itself does not require `VACUUM`.
