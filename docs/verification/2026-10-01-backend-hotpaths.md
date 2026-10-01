# Backend hot path measurements

Measured on 2026-10-01. The source database opened with SQLite URI mode `ro`.
The benchmark copied the relevant rows to a temporary SQLite database.
It copied all 1,487 agent rows, 71,667 event rows, and 34,066 monitor rows.
One agent had 12,100 delivered events and 1,894 metadata rows.
Its team had 59 agents. Other event text was omitted because these paths do not read it.
The live database received no writes.

Each value is the median of seven runs. Times are milliseconds.

| Path          |  Before | After | Measurement                                                                              |
| ------------- | ------: | ----: | ---------------------------------------------------------------------------------------- |
| Event window  | 352.009 | 0.029 | Two ordered event scans, then the unchanged window marker path                           |
| Agent put     |   0.055 | 0.002 | Root record lookup and decode, then the full view builder without that lookup            |
| Known context |  79.178 | 0.017 | Delivered event join and JSON sort, then the persisted manifest lookup                   |
| Team status   | 936.715 | 4.311 | Global record decode and monitor filter, then team selection and indexed capacity counts |

The event pull uses a created and ID index. It checks the event entity sequence
through a collection and sequence index. It sorts the 200 row window only after
an event entity changes.

The context lookup uses a persisted manifest keyed by agent and epoch. A cold
cache returns unknown without reading event history. The next confirmed delivery
stores the manifest. Exact worktree reminder versions also use a bounded lookup.
Context repair uses an agent and time index over delivered repair candidates,
plus an index over user event turns.

Checks passed:

- `tests/sync-entities-contract.py`
- `tests/token-efficiency-contract.py`
- `tests/worker-lifecycle-contract.py`
- `tests/context-repair-contract.py`
- `tests/context-role-dedup-contract.py`
- `tests/runtime-contract.py`
- `tests/agent-management-contract.py`
