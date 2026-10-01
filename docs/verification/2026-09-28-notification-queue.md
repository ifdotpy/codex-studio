# Notification queue, Phase 3

Source baseline: `eddd9f1d319b6e9794bfb14a81594d0364b7ca2f`.
The fixture writes only to a temporary directory. The live log and database were read only.

## 1. Measure

The live log window was 2026-09-27 21:23:45 to 2026-09-28 05:23:45 UTC.
It contained 27,747 `callbackLatency` records. The log omits fast callbacks when queue delay is below one second.

| Measure                         |                                Value |
| ------------------------------- | -----------------------------------: |
| Callback duration p50, p95, max |          201.8, 2,409.7, 21,685.6 ms |
| Queue delay p50, p95, max       |    21,409.4, 402,819.0, 618,655.9 ms |
| Queue delays above 60 seconds   |                                9,563 |
| Rate limit updates              | 2,784 records, 171.6 ms duration p50 |
| Token usage updates             | 2,803 records, 203.4 ms duration p50 |
| Diff updates                    | 1,598 records, 207.7 ms duration p50 |

The isolated fixture used 24 agents, 320 notifications, 12 snapshots, 12 tool reservations, and scheduler calls.
It measured each outer `Runtime.lock` acquisition. The baseline lock held for 2,599.1 ms and waited for 919.3 ms in total.
The lock holder totals identify the code paths. Concurrent wait totals can overlap.

| Lock holder                             | Calls | Hold ms | Wait ms |
| --------------------------------------- | ----: | ------: | ------: |
| `Runtime.notification`                  |   320 | 2,438.6 |   213.5 |
| `Runtime.snapshot` (HTTP snapshot path) |    12 |    48.1 |   138.6 |
| `RequestMixin.reserve_tool_request`     |    12 |    41.4 |   134.3 |
| `Runtime.dispatch`                      |     4 |    32.5 |    31.6 |
| `CapacityRetryMixin.capacity_tick`      |     4 |    17.8 |    92.9 |
| `RulesMixin.rules_tick`                 |     4 |     5.1 |    30.5 |
| `UsageResumeMixin.usage_resume_tick`    |     4 |     4.8 |    26.4 |

`Runtime.notification` held 93.8% of the measured global lock time in this fixture.
The fixture cannot measure the live process lock without changing the running backend.

## 2. Latest values

`AppServer.enqueue` keeps one queued rate limit update per account server, one token usage update per thread, and one diff update per thread and turn.
A request, receipt, item event, or other notification closes the latest value slots.
A tool call still enters its separate request queue. It closes the slots before the bypass.
The reader contract verifies values, keys, item and request boundaries, and tool bypass.

## 3. Account owner

Rate limit validation, telemetry, and cache writes use a lazy account cache lock.
The database transaction commits before the cache value changes.
The automatic usage resume path still uses `Runtime.lock` when a scheduled resume exists.
An indexed read skips that lock when no scheduled resume exists.
The runtime contract holds `Runtime.lock` in another thread and verifies that a rate limit update completes.

## 4. Callback cost

The dispatcher thread reuses one SQLite connection. Each `Runtime.db` context still commits or rolls back its own transaction.
A nested database context uses a separate connection. The thread local connection closes when its dispatcher thread ends.
The notification path skips unrelated task lookups and root record decoding.
The token usage path uses the budget result already captured by analytics, with a fallback after an analytics rollback.

| Fixture result              |   Baseline | Changed source |
| --------------------------- | ---------: | -------------: |
| Input notifications         |        320 |            320 |
| Executed callbacks          |        320 |            288 |
| Queue delay p95             | 2,592.1 ms |       632.6 ms |
| Callback duration p50       |   2.429 ms |       0.713 ms |
| Callback duration p95       |  35.906 ms |       9.761 ms |
| Total global lock hold      | 2,599.1 ms |       546.7 ms |
| Total global lock wait      |   919.3 ms |       807.0 ms |
| `Runtime.notification` hold | 2,438.6 ms |       395.1 ms |

The fixture uses one saved baseline run. Host load affects elapsed times. The changed source has no live installation evidence.
Run `python3 -B tests/notification-load-contract.py` to print the saved baseline and a new isolated result.
After the rebase onto `be2a1e4`, a new fixture run measured 258.9 ms queue delay p95 and 613.9 ms total lock hold.

## 5. Per agent locks

The fixture queue delay p95 fell by 75.6%. No per agent lock was added.
The live effect remains unmeasured until a separate approved installation.

## Checks

The rebased source passed all required checks: protocol reader (19), tool request (18), runtime (58), critical steer (13), team delivery (10), queue order (12), and native release (18).
The affected usage resume (21), analytics (27), and budget (22) contracts also passed.
The fixture completed after the rebase with one reused dispatcher connection.

## Function and state inventory

| Module                                | Class              | Changed or added functions                                                                                                                                  |
| ------------------------------------- | ------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `scripts/codex_runtime.py`            | `AppServer`        | `close_latest_slots` (new), `coalesce_latest` (new), `release_slot`, `enqueue`                                                                              |
| `scripts/codex_runtime.py`            | `Runtime`          | `db`, `record_task`, `notification`, `set_rate_limits`, `store_rate_limits` (new)                                                                           |
| `scripts/codex_analytics.py`          | `AnalyticsMixin`   | `analytics_event`                                                                                                                                           |
| `scripts/codex_usage_resume.py`       | `UsageResumeMixin` | `usage_resume_limits_changed`                                                                                                                               |
| `tests/protocol-reader-contract.py`   | `ReaderContract`   | `test_latest_values_keep_keys_and_item_request_boundaries` (new), `test_tool_bypass_closes_latest_slot_without_delaying_tool` (new)                         |
| `tests/runtime-contract.py`           | `RuntimeContract`  | `test_rate_limit_notification_does_not_wait_for_unrelated_runtime_lock` (new), `test_dispatcher_database_reuses_connection_and_commits_each_callback` (new) |
| `tests/notification-load-contract.py` | `QuietRuntime`     | `schedule` (new)                                                                                                                                            |
| `tests/notification-load-contract.py` | `TracedLock`       | `__init__`, `acquire`, `release`, `__enter__`, `__exit__` (new)                                                                                             |
| `tests/notification-load-contract.py` | module             | `percentile`, `measure`, `main` (new)                                                                                                                       |

No existing module gained an import or module level name.
The new fixture imports `argparse`, `defaultdict`, `io`, `json`, `Path`, `queue`, `sys`, `tempfile`, `threading`, `time`, `uuid`, `AppServer`, and `Runtime`.
The fixture adds module names `QuietRuntime`, `TracedLock`, `percentile`, `measure`, and `main`.

Existing `AppServer` objects create `_latest_slots` on first queue use.
Existing `Runtime` objects create `_rate_cache_lock` on first rate cache update.
Existing `Runtime` objects create `_callback_db` on first database or dispatched notification call.
The dispatcher sets `_callback_db.reuse` on its first notification and creates `_callback_db.connection` on its first database call.
Each reused database context sets `_callback_db.depth` during the transaction and clears it after completion.
