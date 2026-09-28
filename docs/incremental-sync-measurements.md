# Incremental sync measurements

Measured 2026-09-28 in isolated fixtures. No live database or backend was queried. The reproducible synthetic run is `python3 -B tests/sync-entity-measure.py`.

| Measure | Before | After |
| --- | ---: | ---: |
| Single agent change, raw | 6,600,000 B snapshot | 1,005 B entity document |
| Single agent change, gzip | Not separately measured in task baseline | 266 B entity document |
| Initial load, raw | 6,600,000 B chat snapshot | 40,350 B for 40 synthetic agents |
| Initial entity load, gzip | Not separately measured in task baseline | 10,614 B |
| Pulls at 100 changes/min, 2 windows | 200 full-snapshot pulls/min | 200 incremental pulls/min |
| Snapshot serialize/hash or entity write/pull CPU per 100 changes | 1,363.791 ms | 41.488 ms |
| `sync_entities` WAL writes at 100 changes/min | No entity table | 824,032 B/min |

The live baseline figures, 6.6 MB for `/api/state?view=chat` and 26.7 MB for `/api/state`, were supplied by the task. The synthetic before CPU runs the old full-payload deserialize/serialize/SHA-256 path against a 6.6 MB payload 100 times. The after CPU includes 100 entity writes, 200 checkpoint pulls, and client-envelope gzip measurement. These are local process CPU measurements, not a live-server profile.

For the 40-agent fixture, initial raw transfer fell by about 164×, and one agent change fell from 6.6 MB to 1,005 B. Two open windows each pull once for each of 100 visible changes, so the count remains 200 pulls/min; the bytes and server work per pull are bounded by changed entities. The WAL measurement is total WAL growth while writing/upserting entity versions; SQLite reported 819,200 B in committed WAL frames at checkpoint. This synthetic measurement isolates the new table's cost and does not include unrelated application writes.

The production mobile startup fixture also passed: 7,938 ms cold composer, 330 ms warm draft, and a 4,372,203 B gzip full-state fixture versus a 17,886 B gzip compact snapshot. Those HTTP snapshot measurements validate the fixture, while the raw entity seed measurement above validates the new pull wire format.
