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
implementation and final load results remain pending. No live backend update
is claimed.
