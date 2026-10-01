# Runtime load benchmark

## Change Contract

The harness owns synthetic active-turn identities, event timing, and benchmark evidence. Production code owns Runtime storage and APIs, AppServer queue and dispatch, browser sync, UI behavior, and the scheduler's worker limit. The harness must not launch provider sessions, change production concurrency limits, or use a user's state. Exact offered event identities and original Runtime inbox IDs must be accounted for; overload or incomplete drain must fail visibly. Workload options are owned by [`run.mjs`](run.mjs). The targeted check is the real App/RxDB path in `--check`, followed by the full multi-tab run.

## What it measures

The default case creates eight synthetic lead teams with 32 workers each: 256 worker identities and 264 active-turn equivalents total. These records represent already-active native turns. The benchmark does not launch agent workers, change the production scheduler limit in [`codex_runtime.py`](../../codex_runtime.py), open a provider session, or make a paid model call.

The isolated fixture uses the production Runtime, SQLite, analytics, transcript, chat-message, HTTP, sync, and `AppServer` implementations. A benchmark-only in-memory JSON-lines peer replaces `subprocess.Popen` using the same seam as `tests/protocol-reader-contract.py`. The real `AppServer` reader, bounded FIFO callback queue, ordered dispatcher, and callbacks remain active. Synthetic account keys route records through separate fake transport queues, which share one Runtime lock; they do not represent saved accounts or open provider sessions. The UI's provider-dependent model, cost, and limit requests for these synthetic keys are reported as expected unavailable API responses. Worker-to-peer and worker-to-lead chat rows use `Runtime.chat_message`. Their exact runtime-event IDs are consumed by synthetic `userMessage` receipts through `Runtime.notification` and the AppServer queue. Event IDs and delivery counts are reported separately.

Four phases offer work to every worker: warmup, steady, burst, and drain. Each worker/round offers a turn start and finish, tool start and completed output, assistant delta and final, and two durable chat messages with receipts. Assistant text carries one unique witness marker per team. Tool output is synthetic protocol data, not execution. The report separates API-server request lifetime, AppServer receive-to-callback delay, callback duration, browser route timings, and the unique final-marker offer-to-render delay.

Eight headless Chromium tabs run the built production App and RxDB client without activating a desktop window or sending OS input. Each represents one team and opens a representative worker transcript; the same team’s lead feed is read through `/api/agent-chat`. The report records each page's visibility state because headless background behavior can differ from a desktop session. It checks RxDB sync initialization, marker rendering, pull coverage, queue drain, unique callback IDs, pending/consumed Runtime event counts, long tasks, UI interval staleness, API responses, browser process use, and errors.

Hook start and completion lifecycle notifications use the production callback and analytics path, but do not run hook commands. Provider/model execution and native tool execution are omitted. Tool output is synthetic protocol data. These results describe local application and transport behavior, not model throughput.

## Build and run

Build the application for the source checkout you want to measure:

```sh
npm --prefix web ci
npm --prefix web run build
node scripts/benchmarks/runtime_load/run.mjs --check
node scripts/benchmarks/runtime_load/run.mjs --rounds 1
```

The quick check uses a reduced fixture and one browser tab. It must pass before the full run. The runner owns option bounds and defaults. `--rounds` increases the sample count, `--offered-rate` sets steady-phase offered worker turns per second, and `--transports` selects the number of fake AppServer queues. Phase rates and queue counts are defined in [`server.py`](server.py) and [`run.mjs`](run.mjs). For example:

```sh
node scripts/benchmarks/runtime_load/run.mjs --rounds 2 --offered-rate 80 --transports 2
```

To compare source checkouts with a pinned frontend artifact, select the backend checkout, built dist directory, and frontend source revision independently:

```sh
node scripts/benchmarks/runtime_load/run.mjs \
  --source-root /path/to/backend-checkout \
  --frontend-dist /path/to/web/dist \
  --frontend-source-revision <revision> \
  --output /outside/checkout/report.json
```

The report records the backend source revision and digest, frontend source revision and artifact SHA-256. The harness uses a unique state, profile, and Chromium temporary profile under the evidence directory. This avoids occupied application state and shared `/tmp` quotas. It has a hard deadline; failures retain JSON evidence and a browser log outside the checkout. It closes only its own pages, fake transports, server, and temporary state.

Reports default to `$XDG_STATE_HOME/evidence/latency-components` or `~/.local/state/evidence/latency-components`. Never point the fixture at an existing state directory.

Each representative tab tracks a phase-specific witness that appears only in the completed assistant item, not its streaming deltas. `finalOfferToFirstDOMAppearanceMs` measures elapsed time from offering that final item until the browser first observes the marker under `#messages`.

If a tab stalls before the workload starts, run `--diagnose-origin-pool` with the same source and frontend options. This diagnostic opens the real app in six tabs, captures Chromium Network-domain requests and active server routes, closes only its first benchmark-owned page, and records whether the pending sixth navigation then completes. It exits before offering synthetic turns and writes diagnostic-only evidence, not a load result.

The baseline UI reproduced a per-origin connection-pool stall: after five tabs opened, the fixture had five active `/api/sync/stream` and five active `/api/transcript/stream` responses. Chromium had issued tab 6's document request, but the fixture had received no sixth `/` request. Closing only benchmark tab 1 freed a slot; the fixture then received and returned the sixth document request. The browser peaked at 11 processes and about 1.43 GiB RSS while the host reported about 20 GiB available. No `ERR_INSUFFICIENT_RESOURCES` or refused localhost request was observed. See the external diagnostic JSON for the captured CDP timeline and route counts. Eight real tabs cannot complete initialization with this stream pattern until the product connection contract changes; do not raise browser connection limits or suppress streams to make the benchmark pass.

## Reading results

`offered` and `dispatched` describe callback identities admitted and consumed by production `AppServer.callbacks`. Eight assistant fragments may merge into a single callback; their identities are preserved in the production fragment sample list and counted individually. A passing run has equal unique identities, a drained callback queue, every chat and child-result event acknowledged by its original ID, no pending synthetic Runtime events, and one rendered witness per team tab.

`latencyMs.receiveToCallback` starts when the production pipe reader receives a notification and ends when its ordered dispatcher begins the callback. `latencyMs.callbackDuration` measures time inside the Runtime callback. `latencyMs.runtimeLock` separates wait-to-acquire from time holding the shared Runtime lock, grouped by calling thread. This is harness-only instrumentation around the production lock. `httpServerLatencyMs` is server-side request duration by route; streaming requests measure connection lifetime. Browser route timings include browser-to-server work. `renderedByCategory.expectedAssistantMarkers` measures from the final notification offer to the marker becoming visible in the selected worker transcript.

Queue capacity and peak depth are reported per real `AppServer.callbacks` queue. Queue depths are sampled when the fixture offers an event, so the peak is a sampled lower bound. The Python CPU and peak RSS belong to the fixture process. Browser process CPU/RSS are sampled independently. The report includes host process, descriptor, memory, and cgroup limits to make resource failures traceable.

This is a synthetic local benchmark, not a production capacity guarantee. Compare runs only when host, workload, backend revision, frontend artifact, and browser match. A failed check or incomplete drain is evidence of a broken or overloaded run; do not interpret it as a successful lower-rate run.
