# Text clipping detector

Import `detectTextClipping` from `text-clipping.mjs`.
Call it with `page.evaluate(detectTextClipping, { scene, profile })` after the page settles.

The report contains the scene, profile, selector, text, rectangle, and reason.
It checks text ranges against hidden or clipped ancestors, dimensions, ellipsis
without full text, and covered button or label text. A title, an accessible
label, or a referenced tooltip must contain the full text. A line clamp with
full text has `intentional: true`.

The detector excludes hidden elements, closed details, text outside scroll
viewports, screen-reader text, and controls below a modal. A scroll container
permits text outside its viewport. Vertical scroll overflow has an intentional
report entry. Open menus and model lists can intentionally cover controls. A sticky modal
header can cover controls in the same scroll container. These cases have a
justification in the report. Native select options, input values, SVG icons,
and terminal canvas cells require separate checks.

Run the detector regression test:

```sh
cd web
npx playwright test --config playwright.config.ts --project=client text-clipping.spec.mjs
```

The scene runner is `/tmp/ui-compare/clip-audit/run.mjs`. See its README for the
isolated fixture, five profiles, screenshots, and JSON report.

## Running the client browser suite

From the repository root, run `npm run test:client:browser`. Playwright builds
the app, then selects workers from the CPUs available to this process and the
memory currently available to it. `PLAYWRIGHT_WORKERS=<n>` overrides the
automatic selection for manual runs.

The runner probes temporary filesystems with an `fsync` write sized to the
selected worker count times the measured per-worker fixture scratch, capped at
256 MiB. It then chooses the memory-backed candidate with the most verified
headroom. A failed quota or space probe excludes that candidate; `/tmp` is
tested like every other candidate. Set `PLAYWRIGHT_SCRATCH_ROOT` to probe and
select a specific root. The probe is implemented in
[`scratch-root.mjs`](../scratch-root.mjs).
Spawned fixture `TMPDIR`, `HOME`, `XDG_STATE_HOME`, `XDG_CACHE_HOME`, and test
state directories stay under this scratch root.

The CPU limit is the smaller of `os.availableParallelism()` and the cgroup CPU
quota. The runner samples `/proc/stat` eleven times across five seconds,
subtracts its own process from each `procs_running` sample, and subtracts the
median competing count from the CPU limit. The CPU plan uses a 0.54-core mean
demand per worker measured as process-tree CPU seconds divided by wall time and
worker count in a non-saturated two-worker run. It doubles that measurement for
startup and workload variance, then budgets at most half of currently available
CPU capacity for the browser suite. This leaves headroom for fixture startup
and short CPU bursts; the run's spawn-to-port median is the startup health
signal.
The memory budget is the smaller of 70% of available memory and available
memory minus a 4 GiB reserve.
Each worker is sized by its measured p90 process tree PSS, which already
includes Chromium and fixture children. Until at least five worker samples
exist, the selector uses a conservative 2.1 GiB tree-PSS default. If headroom
cannot fit one worker while retaining the reserve, the automatic runner refuses
to start. The automatic worker count is the smaller CPU or memory limit. The
selector and its current measured inputs live in
[`playwright-workers.mjs`](../../playwright-workers.mjs). Use
`PLAYWRIGHT_WORKERS=<n>` to lower or otherwise manually choose concurrency;
the override still must fit the measured memory ceiling and reserve.
