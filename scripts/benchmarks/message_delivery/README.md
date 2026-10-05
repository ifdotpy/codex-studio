# Message delivery benchmark

## Change Contract

Keep the measured notification, database, and HTTP code in production modules.
This scenario owns only synthetic inputs, local clients, and measurement. Never
use an existing state directory or connect to a native provider. Check every
final message and fail visibly on missing or corrupt data. Workload defaults and
limits live in [benchmark.py](benchmark.py); run the local tests below after changes.

This benchmark answers a narrow question: after Studio receives a synthetic
message notification, how long until a local HTTP client can read that final
message from the transcript?

Think of it as timing a letter through the real server room: `Runtime.notification`
writes the production SQLite transcript, then the production HTTP handler serves
it to a local client. The synthetic agents never start a model or native
app-server. A fail-fast factory prevents native transport. Every case uses a
fresh temporary state directory and Codex profile.

## Run it

From the repository root:

```sh
python3 scripts/benchmarks/message_delivery/benchmark.py --check
python3 scripts/benchmarks/message_delivery/benchmark.py --agents 1 8 32 --messages-per-agent 8 --repetitions 2 --analytics both --transport both --output /tmp/message-delivery.json
python3 -m unittest discover -s scripts/benchmarks/message_delivery -p 'test_*.py'
```

The first command is a quick protocol-3 stream and sync-pull smoke run. The full command produces
separate cases for 1, 8, and 32 subscribed agents, with and without the real
analytics history importer, through both transport paths. Its synthetic offered
rate defaults to 80 messages per second across the whole case. `--rate` changes
that fixed-rate arrival schedule (finite and at most 10,000 messages per
second); repetitions use fresh runtimes, capped at 100 per setting and 24 total
matrix cases.
Each agent receives eight distinct final messages by default. The cap of 100
messages per agent keeps every transcript inside the production 120-item page.
At the same total rate, more subscribers receive fewer messages per second each
and the case lasts longer. Compare matching configurations, not an apparent
improvement from adding subscribers to a fixed-rate workload.

Both transport cases use one protocol-3 `/api/sync/stream` connection with
typed transcript subscriptions. `--transport sync` measures concurrent
`transcript:<id>` projection pulls from `/api/sync/pull`; `--transport
transcript` measures concurrent current transcript reads from `/api/transcript`.
The reports label the pull paths separately.

The 1/8/32 counts stress increasing numbers of subscribed chats. They do not
mean a normal foreground view opens 32 streams: the application shares one
protocol-3 invalidation stream across projection subscribers. This benchmark measures
server and local HTTP delivery only. It does not measure the browser, RxDB,
rendering, a physical network, a native callback queue, or model response time.
The synthetic dispatch queue belongs to this fixture and is reported as such;
its peak depth is not a measurement of the native app-server callback queue.
Each case runs in a child process with a parent-enforced 60-second deadline.
The worker also uses bounded thread cleanup. Parent supervision can terminate a
case if a production lock or runtime close leaves a non-daemon thread stuck.
HTTP sockets use a 20-second read timeout, above the server's 15-second
protocol-3 heartbeat interval. The 60-second case
deadline remains the overall bound.

## Report fields

The JSON report contains the command configuration and separate rows for every
agent-count, importer, transport, and repetition combination.

- `latencyMs.scheduledToEnqueue` records producer lateness from the fixed
  absolute offer schedule until the fixture accepts the input. This keeps late
  producer work visible instead of shifting the arrival clock forward.
- `latencyMs.enqueueToDispatch` starts when the fixture queue accepts an input
  and ends immediately before `Runtime.notification`.
- `latencyMs.dispatchToClientReceipt` starts immediately before that production
  notification call and ends when the client sees the exact final item in a
  transcript response. For sync transport, this includes invalidation cadence
  and the subsequent pull. SSE frames are not counted as messages.
- `latencyMs.scheduledToClientReceipt` starts at the fixed-rate due time;
  `enqueueToClientReceipt` covers acceptance through final receipt. The report
  also gives exact offered total and per-agent rates. Only 1, 8, or 32 subscribed
  agents are supported; `benchmark.py` owns the workload and matrix limits.
- Percentiles use
  nearest rank over delivered messages; `samples`, `expected`, and `received`
  show the population. A missing, timed-out, or corrupt message fails the run.
- `elapsedMs` covers offered workload through the last client receipt.
  `cpuProcessSeconds` includes fixture setup, input generation, and HTTP client
  work in the case process; it is not backend-only CPU time.
- `peakRss` is the process high-water RSS from the standard library. The value
  is the high-water mark of the isolated case process, not a per-phase delta;
  the report states the platform unit and source. It may be unavailable.
- `queuePeak` and `queueDrained` describe only the benchmark's bounded synthetic
  input queue. There are no automatic retries.
- With analytics enabled, the benchmark starts the actual
  `Runtime.analytics_history_step` importer on a generated 1,024-record
  synthetic journal. The first dispatch releases the import start gate; importer
  readiness is recorded after its first history step runs and its written-row
  count is observed. The benchmark
  timestamps observed analytics row writes and requires at least one to fall
  inside either a notification execution interval (`notification_execution`)
  or the complete fixed offer-to-final-client-receipt window
  (`fixed_offer_to_final_client_receipt`). `analyticsProgress` reports which
  window matched, its interval bounds, and the timestamps used. The import is
  one finite batch, not continuous background load throughout every case.
  The generated records are synthetic
  token usage responses that exercise the production analytics collector; the
  run fails unless it writes one analytics usage row for every response.

There is no pass/fail performance target because no baseline has been approved.
Use the report to compare runs made on the same host and configuration. Keep
reports and all temporary databases outside the checkout. The benchmark refuses
native transport if `CODEX_BENCH_NATIVE_TRANSPORT` is set.
