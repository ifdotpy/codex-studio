# Execution identities

Studio stores one execution record per native model turn. A pending input run can
have several start attempts before the provider accepts it. Busy input adds an
attempt to the containing run. If the provider returns a new turn, Studio keeps
the original run and moves that attempt to the new run.

The records supplement receipts. They do not authorize retries or continuation.
An unsent attempt, an unknown submission, an observed turn, and an accepted input
remain distinct. Only the native response or exact input receipt confirms input
acceptance. An observed native start confirms activity only.

## Records and writes

- `runtime_execution_runs`: agent, account, epoch, native thread and root turn,
  first, root, and latest attempt, native terminal status, result, and recovery markers.
- `runtime_execution_attempts`: the original start attempt, run, preparation
  identity, input event identifiers, native operation fields, and submission state.
- `runtime_execution_inputs`: links from runs to the existing input queue.
- `runtime_execution_effects`: tool receipts, monitor receipts, worker creation,
  native requests, and task submissions. Each effect retains its original run.
- `runtime_execution_nodes`: managed workers and provider child or background
  turns. Child terminal events cannot end the root run.

`Runtime.put`, native event handlers, and task submission write these records in
their existing transactions. Text deltas do not write execution records.
Disconnect and backend restart do not establish a native terminal outcome.
A native completion record must identify the exact root turn, account, and thread.
Child and background events update nodes only. The observer preserves the existing
agent, item, task, and failure notice handlers. Record failures log the function
and exception type. They do not discard native events.

The existing hourly HTTP server maintenance removes at most 200 finished or
rejected runs older than 30 days per pass. It removes their dependent records
first. Active and unknown runs remain available for recovery. Existing receipts
and input events remain outside this retention policy.

## Reads

`/api/diagnostics` includes `executions`. The response contains the latest 25 runs,
with up to 100 attempts, nodes, effects, and input links per run. It includes the
limits in the response. The message information popover shows the run identifier
and the root start attempt identifier for the exact saved native thread and turn.
An ambiguous identity produces no identifiers.

Startup creates empty tables and indexes. It reconciles existing agent state when
that state changes. It does not rebuild old history or rewrite existing receipts.
Earlier analytics messages can therefore have no execution identifier.

## Migration measurement

The October 3, 2026 measurement reads only live database metadata. The live runtime
file contains 17,733,206,016 bytes. The fixture uses each table's maximum row
identifier as its row count. Deleted rows can make this count an upper bound.
It uses synthetic payloads of 9,780 bytes per row and contains 18,046,525,440 bytes.
The fixture contains these rows:

| Table            |    Rows |
| ---------------- | ------: |
| Agents           |   1,766 |
| Input events     |  80,098 |
| Transcript items | 855,009 |
| Tool requests    | 110,909 |
| Native tasks     | 676,814 |
| Monitors         |  36,874 |

The execution schema took 7.768 ms to create. Ten existing schema checks had a
median of 0.017 ms and a maximum of 0.767 ms. SQLite used write ahead logging and
FULL synchronization. The schema migration read no old table rows. This measures
the new schema step, not total backend startup or a live provider request.

Run the fixture again:

```sh
python3 -B tests/execution-migration-benchmark.py --payload-bytes 9780
```

## Extra SQL statements

The measurement uses the actual `Runtime.put` caller and temporary state. It
counts only SQL executed by the additional record hook. Existing source writes,
sync projections, and transaction commits remain outside these counts.

| Put case                                                   | Reads | Writes | Total |
| ---------------------------------------------------------- | ----: | -----: | ----: |
| Agent text delta or no execution field change              |     0 |      0 |     0 |
| Agent accepted attempt with one input, changed preparation |     3 |      3 |     6 |
| Agent first unsent attempt with one input                  |     2 |      3 |     5 |
| New tool request linked to an active run                   |     4 |      1 |     5 |
| Unchanged tool request                                     |     3 |      0 |     3 |
| Tool request terminal status                               |     3 |      1 |     4 |

The count varies with the state. Each additional input adds one link insert.
A terminal run update adds one attempt status update. A worker status change
adds one node read and, when its node exists, one node write. Child creation also
links the parent run and creates a worker node. The record hook adds no commits.

```sh
python3 -B tests/execution-write-cost.py
```

## Notification replay measurement

The October 3, 2026 replay uses `tests/notification-load-contract.py` on
`origin/main` revision `a4c2691` and the execution branch. Three trials alternate
between the two checkouts. Each trial uses temporary state, 320 input frames,
288 callbacks after transport coalescence, and one dispatcher connection.

| Metric, milliseconds  | Before median | After median |       Before range |        After range |
| --------------------- | ------------: | -----------: | -----------------: | -----------------: |
| Queue delay p95       |       521.111 |      741.068 | 460.172 to 730.337 | 575.444 to 773.008 |
| Callback duration p95 |         5.487 |        7.766 |     4.482 to 6.946 |     6.586 to 9.530 |
| Total lock wait       |       404.061 |      527.042 | 324.109 to 579.726 | 438.859 to 538.608 |
| Total lock hold       |       512.023 |      741.107 | 450.025 to 722.759 | 577.693 to 770.003 |

The after trials are slower. These elapsed times include host load and concurrent
fixture activity. They do not isolate the cost of the new SQL statements. A
separate trace measured 308 agent record calls and 24 receipt record calls,
including setup, in 9.493 ms total. The agent calls executed 120 SQL statements,
all during setup. The receipt calls executed 96 SQL statements. This trace does
not measure SQLite commit cost or establish live performance.

Run this command in each checkout. Compare its `after` object; the command's
`before` object is the older checked-in baseline.

```sh
python3 -B tests/notification-load-contract.py
```

The 18,046,525,440-byte migration fixture was removed after its measurement.
The deletion check finds no files under
`/var/folders/29/8pytxrvn6qlcm4384bmy9n6m0000gn/T/studio-execution-migration-*/fixture.sqlite3`.
The temporary directory's random suffix was not retained. Only measured numbers
and the generator command remain.
