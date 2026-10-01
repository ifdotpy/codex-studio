# Runtime latency components, 2026-10-01

This report is for contributors checking the notification, transcript storage,
and UI synchronization changes together. Component READMEs own their contracts,
tests, and benchmark commands:

- [Native notifications](../../scripts/native_notifications/README.md).
- [Transcript storage](../../scripts/transcript_storage/README.md).
- [UI synchronization](../../scripts/sync/README.md).

## What the measurement means

The starting production logs showed a callback lasting about 73 seconds and
events waiting about 84 seconds before dispatch. Callback duration includes time
waiting for the shared runtime lock. These figures do not identify the function
that held the lock, and this work does not claim to reproduce that particular
incident.

Think of the shared lock as a single service desk. Reducing unnecessary work at
the desk helps other callers proceed. Faster lookup or less indexing alone does
not prove that the entire application is faster under every workload.

## Before-change delivery measurement

The baseline uses revision `d12ff29bc3bf4f924b37aeaaf5ebfcff8bff086a` and the
existing [message delivery scenario](../../scripts/benchmarks/message_delivery/README.md).
The run started at `2026-10-01T01:43:12Z`. Each configuration ran once, with eight
final messages per subscribed agent and a fixed total offer rate of 80 messages
per second. All expected messages arrived with the correct text.

The table records milliseconds from each message's scheduled offer to the local
HTTP client's receipt. The percentiles describe the samples in that configuration;
the one-agent cases contain only eight samples. They are not statistical proof
of a performance improvement.

| Subscribers | Concurrent history import | Received / expected | p50 ms | p95 ms  | p99 ms  |
| ----------- | ------------------------- | ------------------- | ------ | ------- | ------- |
| 1           | No                        | 8 / 8               | 949.45 | 999.45  | 999.45  |
| 1           | Yes                       | 8 / 8               | 939.64 | 989.64  | 989.64  |
| 8           | No                        | 64 / 64             | 579.81 | 950.79  | 979.81  |
| 8           | Yes                       | 64 / 64             | 557.95 | 928.21  | 957.95  |
| 32          | No                        | 256 / 256           | 747.62 | 1156.53 | 1215.27 |
| 32          | Yes                       | 256 / 256           | 725.00 | 1091.77 | 1178.71 |

The primary sync route includes its invalidation cadence. The fixture uses real
notification, database, and HTTP code with synthetic agents and messages. It
does not measure a live model, the native callback queue, browser rendering, or
a remote network. The importer-enabled cases verified actual analytics writes
inside the measured delivery window.

## Reproduce the integration workload

From the repository root, choose a result directory outside the checkout:

```sh
bench_dir=$(mktemp -d "${TMPDIR:-/tmp}/studio-latency.XXXXXX")
python3 -B scripts/benchmarks/message_delivery/benchmark.py \
  --agents 1 8 32 --messages-per-agent 8 --repetitions 1 \
  --analytics both --transport sync --output "$bench_dir/delivery.json"
```

The scenario owns its deadline and failure reporting. A timeout, corrupt body,
or missing message fails the case. Compare identical workloads on the same host
with similar background load. The raw local baseline is retained outside the
repository as `evidence/latency-components/baseline-delivery.json` beneath the
Studio state directory.

## Integration status

The small baseline is recorded. The requested stress scenario additionally
requires 256 active workers in eight browser tabs, with 32 workers in each team.
The [runtime load scenario](../../scripts/benchmarks/runtime_load/README.md) owns
its event mix, rate, duration, correctness checks, and reproduction commands.

Worker activity is emulated without model requests. This must be distinguished
from starting 256 native model turns through the production scheduler, whose
admission limit is owned by
[Runtime.dispatch](../../scripts/codex_runtime.py). The benchmark does not change
the live application's concurrency setting or use existing user sessions.

The three components are integrated in the isolated checkout at
`f2e4ffe11fcd5aabfa7b84542a3eeafbcc06cad4`. Fifteen component and integration
suites passed together, including native notifications, runtime, history,
portable export, workspace, request recovery, HTTP sync, mobile state, and source
inventory. The frontend build passed; its source revision is `cf070bb` because
the later commits only change transcript storage and its callers.

The continuous-stream microbenchmark uses the actual one-second scheduler
cadence. For 200 fragments it made seven FTS writes (six checkpoints and the
final write), compared with 200 per-fragment writes. Its maximum simulated dirty
age was 2.9 seconds within the three-second fixture bound. This is in-memory
SQLite evidence, not a filesystem or application latency result.

The item migration now combines missing-index repair and partial-text
classification in one resumable pass. Its default step is limited to 128 rows
and 2 MiB of excerpt text, allowing one oversized row to progress. The byte
budget limits excerpt indexing, not all legacy full-body reads or wall-clock
lock duration. A separate historical FTS address pass remains bounded by rows.

Independent combined review found a missing automatic retry after a transient
snapshot 503. The correction was independently reviewed and integrated as
`be8c43a`; its Chromium test holds the generation unchanged while recovery
clears the error.

The original eight-tab baseline cannot finish initialization: the sixth document
request remained queued in Chromium until an earlier benchmark-owned tab closed.
The diagnostic captured browser requests and server arrival counts, with about
20 GiB available memory. This is a confirmed multi-tab connection bottleneck,
not a successful load measurement.

At the user-requested Pivot1 checkpoint, a shared cross-tab notification channel
was proposed. The user authorized this direction and local-main integration at
11:19 UTC. TanStack Query remains a separate experiment. The new transport must
preserve exact message identity and offline/reconnect behavior, avoid per-tab
permanent streams, and pass the full 256-worker/eight-tab scenario. Its
implementation is integrated as `d5b1618`; final load acceptance remains pending.
No live backend update is claimed.

## Local-main integration checks

The integration incorporates the existing local target `3920a87`, without the
TanStack Query experiment. Merge `03702fe` preserves provider-version warnings
in the pinned snapshot and its volatile invalidation signature. Both snapshot
locks are acquired nonblocking to avoid inversion with native send/startup.
Correction `9a287dd` gives team tools a separate committed roster read;
`4a50afa` makes snapshot-observation tests tolerate only the documented transient
read deferral. Independent review found no remaining issue in those changes.
The 16 mobile-state checks, HTTP contract, frontend build, and 52 runtime
contracts passed before the budget-schema addition.

A subsequent small delivery run failed with SQLite's `database schema has
changed` during analytics import. It did not reproduce in two later six-case
runs. `32e3119` initializes budget tables before runtime workers and adds a real
history-import regression that checks unchanged schema version and one budget
charge. This removes a plausible late-DDL race; the original failure's exact
cause remains unproven. Future benchmark failures include importer tracebacks.

The six-case follow-up delivered and drained every expected message: 8, 64,
and 256 messages for 1, 8, and 32 agents, with analytics both off and on. At 32
agents, enqueue-to-client p95 was 1397.71 ms without analytics and 1471.34 ms
with it, versus 1156.40 and 1091.73 ms in the original baseline. These are single
runs with a one-second legacy SSE cadence and additional target changes; they
do not establish an application speedup. Evidence:
`message-delivery-analytics-fixed.json` in the external latency evidence folder.
The required 256-worker/eight-tab workload remains the acceptance gate for the
shared-stream change.

## Sustained workload and WAL lifetime findings

The shared-stream implementation passed the exact-commit eight-tab Chromium
check: one active SSE, scoped transcript invalidation, hidden-owner handoff,
and owner-close recovery in 3531 ms. A browser Web Lock owns the stream;
BroadcastChannel relays generations and unavailable coordination uses polling.
Managed transcript views use scoped pulls instead of a per-tab transcript SSE.

The first sustained 256-worker/eight-tab run on `d5b1618` initialized all tabs
in 524–688 ms, then exceeded its 240-second workload deadline. It recorded
264 unrecovered snapshot-deferral 503 responses. A subsequent diagnostic run
also failed. Early instrumentation added database reads to every callback and
unbounded per-thread metric categories; those results must not be treated as
unmodified backend capacity. Later harness revisions remove those probes and
separate scheduled intents, actual notification offers, and completed durable
writes plus receipts. The acceptance target remains 160 configured turns/s,
with at least 90% achieved offers and completions, exact event accounting,
and the documented browser latency and drain limits.

Read-only analysis and isolated reproduction also identified excessive trigger
DDL on unrelated schema changes. The correction through `5d007c5` preserves
matching triggers and atomically repairs changed triggers with scope-generation
increments, including populated sources discovered at restart. Nineteen
SyncStore tests and the HTTP contract passed after integration. This does not
establish that DDL caused the sustained snapshot deferrals.

A fixed-count runtime-only comparison used the same harness `66d6b35`, FULL
SQLite durability, ext4, two transports, serial producer, and 256 workers.
The baseline was `b0d9c3c`; the candidate was `049d88e`, which retains one
WAL-registered idle connection while keeping existing per-operation commits.
The harness's own keeper was disabled in both runs. Both completed all 1280
turns (256 warmup, 512 steady, 256 burst, 256 drain), 2560 durable chat writes
and their receipts, 23040 notification sample identities, and 3840 runtime
event acknowledgements, with no pending work or errors.

Elapsed time fell from 104.2 to 48.4 seconds. Steady completed-turn rate rose
from 18.91/s to 35.02/s; this still fails the 144/s minimum capacity gate.
Sampled callback connection-close mean fell from 1.566 to 0.088 ms, and p99
from 36.73 to 0.254 ms. Commit/context-exit mean fell from 2.538 to 1.008 ms,
and p99 from 45.53 to 3.87 ms. These are one paired runtime-only measurement,
not browser acceptance or live model throughput. Evidence is saved externally
at `runtime-load-fixedcounts-keeper-049d88e/comparison.json` with the raw logs.

The final keeper candidate `737314a`, integrated as `6248ecf`, adds reviewed
startup-failure cleanup to the measured implementation. Its connection is
created, primed, and closed on a dedicated owner thread, with no retained read
transaction. It closes after Runtime-managed writers drain and before lease
release. Existing fail-closed partial shutdown and daemon HTTP-handler lifecycle
limitations are unchanged. Production synchronous settings are unchanged;
NORMAL-mode measurements were isolated diagnostics and are not acceptance.

No live backend restart, user-database modification, or native model workload
was performed. TanStack Query remains outside these changes. Further work is
required to meet the full 256-worker/eight-tab acceptance target.
