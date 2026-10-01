# Runtime load benchmark

## Change Contract

This harness measures Studio's real Runtime, SQLite, HTTP, sync, RxDB, and browser application under a bounded synthetic load. Its Python fixture owns the synthetic active-turn identities, event offer queue, and report; production modules continue to own storage, notifications, sync, HTTP routes, and the app UI. It never opens a native provider session or reads a user's state. Do not change the production scheduler's worker cap to represent this workload. Run the quick check before a full load, keep JSON evidence outside the checkout, and inspect all event counts and cleanup results.

## What it runs

The default case creates eight synthetic leads and 32 synthetic worker identities per lead: 264 active-turn equivalents. A synthetic active turn has a persisted `running` state and a unique turn identifier. No Codex or Claude process is started, no paid model call is made, and no provider session is opened.

Each worker gets a temporary state and profile. The harness offers four bounded phases (ramp, steady, burst, and drain). Each phase sends turn lifecycle events, assistant deltas and final messages, tool start/output events, plus durable worker-to-worker and worker-to-lead messages through production `Runtime.chat_message`. Production `Runtime.notification` stores transcript and analytics events. The actual `make_server` HTTP implementation serves eight headless Chromium pages running the built Studio App and RxDB sync client.

The eight App pages are the only browser tabs counted. They use the real local server origin, state API, sync stream, pull API, transcript store, and browser rendering. Feed reads after the workload go through the production `/api/agent-chat` endpoint. The JSON report includes browser API latency samples, page errors, long tasks, mutation counts, visible worker rows, and per-team message coverage, as well as Runtime event totals, transcript roles, queue depth and drain, dispatch delay, CPU, and peak RSS.

The one ordered queue in `server.py` belongs to this benchmark. It measures accepted-to-dispatched wait and Runtime dispatch time. It is not the native `AppServer.callbacks` queue. That production queue cannot be started under the no-native-factory rule because constructing `AppServer` launches a native CLI process. Consequently native callback queue depth and native-model/tool response time are omitted and must not be inferred from these results. Tool events are protocol-shaped synthetic notifications, not real tool execution.

## Run

From the repository root, first build the production browser application:

```sh
npm --prefix web ci
npm --prefix web run build
```

The quick check uses one team, two synthetic workers, and one browser page to confirm the end-to-end wiring. Run it before the full eight-tab, 256-worker case:

```sh
node scripts/benchmarks/runtime_load/run.mjs --check
node scripts/benchmarks/runtime_load/run.mjs --rounds 1
```

Chromium is headless. Playwright uses its installed Chromium by default; set `CHROME_BIN` to another executable when needed. The Node runner stores reports under `$XDG_STATE_HOME/evidence/latency-components` (or `~/.local/state/evidence/latency-components` when `XDG_STATE_HOME` is unset), has a hard case deadline, terminates a stuck fixture, closes every browser page, and removes its unique temporary state only after child processes stop. A failure exits nonzero and prints the fixture diagnostic.

To run the harness against another checkout's production app/server and dependencies, pass that root explicitly:

```sh
node scripts/benchmarks/runtime_load/run.mjs --check --source-root /path/to/checkout --output /outside/checkout/runtime-load.json
```

The report records the source revision and source root. Build that checkout first. The harness scripts themselves come from the current checkout.

## Read the result

`offered` and `dispatched` count each event class. A passing run requires every queued item to finish once, no backlog at drain, the expected active identities, saved assistant transcript items, readable lead feeds, and zero browser page errors. Compare reports only when source revision, host, browser, workload, and rounds match.

`latencyMs.harnessQueueWait` measures the synthetic offer queue; `latencyMs.runtimeDispatch` measures calls into production Runtime methods. Browser resource entries report latency by HTTP route. The browser timing starts and ends at the client and is not server-only time. Queue depth is the maximum harness queue depth, not native AppServer depth. Process CPU and peak RSS belong to the Python fixture process and exclude Chromium and the OS. The browser report separately provides page long tasks and worker-list rendering evidence.

The report is a local benchmark, not a performance target or proof of production provider capacity. It identifies the amount and type of synthetic work the measured code handled and exposes missing event delivery or browser errors.
