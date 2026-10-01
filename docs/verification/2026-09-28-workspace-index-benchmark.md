# Workspace index sizing and build sample

The workspace snapshot uses root and agent filters in SQL. Those reads remain
correct without new indexes, so Runtime initialization does not build them.
The optional definitions are in `scripts/maintenance/workspace-indexes.sql`.
Run that file only in an explicitly scheduled maintenance window: SQLite index
creation takes the writer lock.

## Live row counts

Counts and average record lengths were read on 2026-09-28 from the live SQLite
database with `mode=ro` and `PRAGMA query_only=ON`. No index was created there.

| Candidate index                                  | Table rows | Fixture index size | Fixture build time |
| ------------------------------------------------ | ---------: | -----------------: | -----------------: |
| `runtime_agent_workspace_root`                   |        745 |             36 KiB |           0.0006 s |
| `runtime_request_workspace_agent_status`         |        180 |             16 KiB |           0.0004 s |
| `runtime_complaint_workspace_lead`               |        151 |             12 KiB |           0.0004 s |
| `runtime_monitor_workspace_agent_status_created` |     30,330 |           1.73 MiB |           0.0220 s |
| `runtime_work_workspace_root_status`             |      1,347 |             76 KiB |           0.0010 s |
| `runtime_task_workspace_agent_status_created`    |    523,094 |          29.95 MiB |           0.4631 s |
| `runtime_annotation_workspace_agent`             |          0 |              4 KiB |           0.0003 s |
| `runtime_checkpoint_workspace_agent`             |      6,143 |            268 KiB |           0.0026 s |
| `runtime_rule_workspace_agent`                   |         51 |              4 KiB |           0.0003 s |

The fixture had the exact live row count for each table, 36-character agent/root
keys, matching JSON fields and value types, and SQLite `dbstat` measured each
index's allocated pages. It timed each `CREATE INDEX` separately. The records
were deliberately compact, so these are useful index-size estimates, not a
promise of live build time. Table page layout, WAL pressure, storage speed, and
live writes can increase build duration. The task table is the material index:
estimate about 30 MiB and 0.46 s on the scaled fixture.

## Existing analytics indexes

No live analytics index was added. Read-only `EXPLAIN QUERY PLAN` on the live
database showed the exact-response lookup scanning `analytics_usage` globally
through `analytics_usage_thread` and building a temporary DISTINCT table. With
the selected agent predicate, SQLite searches that same existing index by
`agent`; fixture timing for 815,059 usage rows was 1.405 s for the former global
lookup and 0.031 s for the scoped lookup. On the same fixture, `Runtime.analytics`
profiled at 0.92 s for one agent with 220,000 items and 43,210 usage rows; the
full usage table held 815,059 rows. The endpoint target is met at the runtime
method boundary in that profile, although wall time varied with fixture
background-thread contention. The query still includes all rows for the selected
agent so date filters do not change exact/provisional attribution.

Other analytics reads already use `analytics_usage_scope` /
`analytics_usage_team` and `analytics_items_scope` for the scope and time
predicates. These existing indexes avoid a new write-heavy index build on the
36 GB database.

## Snapshot fixture timing

`Runtime.workspace_snapshot` took 0.054 s on a fixture with the exact live row
counts for agents (745), requests (180), complaints (151), monitors (30,330),
work (1,347), tasks (523,094), checkpoints (6,143), and rules (51). It returned
the selected root's 101 recent/active tasks, 100 bounded monitors, and 9
checkpoints. The optional workspace indexes above were absent during this run.
The fixture used compact synthetic JSON records and did not reproduce the live
record text sizes; timing validates the root-scoped query shape and response
assembly, not the slowest live storage device or concurrent load.
