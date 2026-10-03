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
A native completion must identify the root turn. Unowned completions cannot
change the agent. Late background events update their original node.

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
