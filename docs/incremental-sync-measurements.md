# Incremental sync measurements

Measured 2026-09-28 in isolated fixtures. No live database or backend was queried. The reproducible synthetic run is `python3 -B tests/sync-entity-measure.py`.

| Measure                                                                      |                                   Before |                            After |
| ---------------------------------------------------------------------------- | ---------------------------------------: | -------------------------------: |
| Single agent change, raw                                                     |                     6,600,000 B snapshot |          1,005 B entity document |
| Single agent change, gzip                                                    | Not separately measured in task baseline |            266 B entity document |
| Initial load, raw                                                            |                6,600,000 B chat snapshot | 40,350 B for 40 synthetic agents |
| Initial entity load, gzip                                                    | Not separately measured in task baseline |                         10,614 B |
| Pulls at 100 changes/min, 2 windows                                          |              200 full-snapshot pulls/min |        200 incremental pulls/min |
| Snapshot serialize/hash or entity write/pull CPU per 100 changes             |                             1,582.509 ms |                        65.084 ms |
| `sync_entities` WAL writes at 100 changes/min                                |                          No entity table |                    824,032 B/min |
| Transcript `sync_entities` WAL writes at 54 stream updates/minute-equivalent |         Repeated full-payload projection |                    659,247 B/min |

The live baseline figures, 6.6 MB for `/api/state?view=chat` and 26.7 MB for `/api/state`, were supplied by the task. The synthetic before CPU runs the old full-payload deserialize/serialize/SHA-256 path against a 6.6 MB payload 100 times. In the final rebased run it measured 1,582.509 ms; after measured 65.084 ms for 100 entity writes, 200 checkpoint pulls, and client-envelope gzip measurement. The transcript WAL value is from a separate 40 KB / 54-update fixture at 0.75-second cadence, normalized to one minute; it is not additive to the agent-change case. These are local fixture/process measurements, not a live-server profile.

For the 40-agent fixture, initial raw transfer fell by about 164×, and one agent change fell from 6.6 MB to 1,005 B. Two open windows each pull once for each of 100 visible changes, so the count remains 200 pulls/min; the bytes and server work per pull are bounded by changed entities. Agent WAL growth while writing/upserting entity versions was 824,032 B/min; SQLite reported 819,200 B in committed WAL frames at checkpoint. The separate transcript fixture measured 659,247 WAL B/min while tracking per-item, order, and metadata hashes with NULL payloads. These are independent synthetic loads and exclude unrelated application writes.

The production mobile startup fixture also passed: 9,448 ms cold composer, 370 ms warm draft, and a 4,372,195 B gzip full-state fixture versus a 17,780 B gzip compact snapshot. Those HTTP snapshot measurements validate the fixture, while the raw entity seed measurement above validates the new pull wire format.
