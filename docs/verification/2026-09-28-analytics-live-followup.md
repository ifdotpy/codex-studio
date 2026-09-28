# Analytics payload and Workspace task-feed follow-up

Measurements on 2026-09-28. Live requests were GET-only. Live SQLite inspection used `mode=ro` and `PRAGMA query_only=ON`. The 5.08 GB fixture was populated from a read-only live connection by copying the relevant source rows into a temporary SQLite database; benchmark reads also used `mode=ro`. No live indexes were created.

## Analytics payload and timing

`GET /api/analytics?agent=<lead>` returned 100,369,758 decoded bytes live before this change. The response contained 91,844 rate-limit snapshots (86,206,615 bytes) and 2,953 turns (2,414,729 bytes). The UI rendered the latest 100 of each. The change sends the newest 100 by default, gives SQL `COUNT(*)` totals, and fetches older records in 100-row pages on request. `export=1` still returns every row. Other analytics summary calculations are unchanged.

The copied fixture held 895,350 `analytics_limits` rows (819,883,649 bytes of record JSON), 2,954 selected-agent turns, 200,654 selected-agent items and 43,642 usage samples. It reproduced a 100,500,148-byte decoded baseline response. Warm measurements, including analytics method, JSON serialization and gzip, were:

| Phase | Before | After |
|---|---:|---:|
| Analytics read/decode | 7,650 ms | 3,349 ms |
| Analytics response build | 2,969 ms | 1,564 ms |
| JSON serialization | 386 ms | 18.5 ms |
| gzip | 120 ms | 11.5 ms |
| End-to-end method + JSON + gzip | 11.416 s | 5.125 s (second warm run: 5.541 s) |
| Decoded response | 100,500,148 B | 4,633,580 B |
| Gzip response | 3,603,953 B | 531,194 B |

These are fixture timings, not a live post-install measurement; network transfer is excluded. The response shrank about 95.4% decoded and 85.3% compressed, but the 1 s target is not met. Profiling the changed fixture path found about 249k JSON decodes and 2.23 s cumulative in `json.loads`; `analytics_items` still requires the selected agent's 200k rows to preserve the existing summaries. The current `analytics_limits` count and page selection also scan the table. Thus the copied fixture does not reproduce all of the live 36 GB file's cache and locking conditions, but the remaining multi-second work is measurable locally.

`GET /api/analytics?...&timing=1` now returns `Server-Timing`: `analytics-read`, `analytics-build`, `analytics-total`, `response-json`, and `response-gzip`. Timing is opt-in and uses `perf_counter` around existing work; no lock is held longer to measure.

An optional index was measured only on the temporary copy:

```sql
CREATE INDEX analytics_limits_account_at ON analytics_limits(account,at DESC,id DESC);
```

Its table has 895,350 copied rows and 819,883,649 bytes of record JSON. The index occupied 37,396,480 bytes in `dbstat`; fixture build time was 2.017–2.306 s. It changes count and page plans from full scans to covering index searches, but the measured endpoint remained roughly 4.6 s or more. It does not explain the remaining multi-second build, so this index alone does not reach the target and is not proposed as the analytics fix.

## Workspace task-feed query plan and index estimate

The live `runtime_tasks` table had 525,557 rows and no `runtime_task_workspace_agent_status_created` index. The installed task-feed query plan showed `SCAN t`, a scan of `runtime_agents`, and `USE TEMP B-TREE FOR ORDER BY`. This explains the repeated 1.9–2.3 s idle polls (223-byte response) and the 2.185 s initial request (72,631 bytes).

The handler now resolves active IDs for the authorized root first and binds them as parameters in the task query. Without the candidate index, the task plan remains a full `runtime_tasks` scan plus temporary sorting. With the candidate index, the copied-fixture plan becomes `SEARCH t USING INDEX runtime_task_workspace_agent_status_created (<expr>=?)`, with a temporary b-tree retained for ordering. On the 526,013-row copied task table (2,658,137,202 bytes of record JSON), the index build took 4.599 s and occupied 34,070,528 bytes. An idle cursor poll measured 0.127–0.129 s warm after a 1.801 s cold first call. Without the index, the plan still scans the copied full task table; live installed idle polls measured 1.9–2.3 s. Therefore the index is required for this query rewrite to reduce task-feed polling to about 0.13 s on the measured warm fixture. These values are not a live indexed measurement.

Exact maintenance statement (do not run at startup or lazy initialization):

```sql
CREATE INDEX runtime_task_workspace_agent_status_created
ON runtime_tasks(
  json_extract(record,'$.agent'),
  json_extract(record,'$.status'),
  json_extract(record,'$.created') DESC
);
```

Best live build estimate: about 5–15 s, based on a 4.599 s build over an exact 2.66 GB copied task-record payload; physical layout, storage throughput, and concurrent reads/writes can change this. A live build would take the SQLite writer lock and needs an explicit maintenance decision. Do not create it during application startup.

## Checks

Analytics UI pagination/export and usage account UI passed. Workspace task-feed UI passed. Analytics contract passed (29 tests), analytics history and transaction contracts passed (25 and 4 tests), usage resume contract passed (21 tests), and `workspace-contract.py` passed all 44 tests including the incremental task-feed test. These passed under Python 3.14.7. The analytics UI was rebuilt and rerun after adding the final-page regression assertion; it passed. The shell's default `/usr/bin/python3` is Python 3.9.6; its earlier contract runs were stopped after confirming it cannot import the repository's required `tomllib` and produced simulated native-start timeouts.
