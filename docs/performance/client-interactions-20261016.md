# Client interaction measurements — 16 October 2026

## Method

The baseline is commit `9aec4b99b8b2edb430d898baa260a87354a97a8d`.
The manual `just benchmark-client-interactions` command builds cumulative variants
without changing checked-out source or the normal production `dist`. Only the
five named modules in the build script are substituted; other source, dependencies,
WASM, CSS, fixture data, and browser settings are shared. The manifest records
source hashes for each variant.

Each variant receives three fresh browser contexts at 1200 × 800, one Chromium
worker, with service workers blocked. The fixture has three conversations, two
with 360 messages; transcript pages contain 120 messages. The scenario measures
startup, four chat switches, six Studio settings tabs, chat settings, Messages,
seven workspace sections, an older transcript page, and twelve scroll steps.
Workspace screens use a small fixture with empty resource collections. No real
account, backend, user profile, or model is contacted. No CPU throttling or
synthetic load is applied. Builds and other verification do not run during the
measurements. Results include content readiness, two-frame paint readiness,
frame intervals, mounted DOM counts, and synchronous storage calls.

Timings are descriptive measurements, not a performance threshold. Three samples
cannot establish statistical significance. The final run rotates variant order between sample rounds to reduce warm-up
bias; earlier exploratory runs used a fixed baseline-first order. Fixture timing does not establish native startup
or remote-server latency. Hidden native WebViews suspend animation frames and
were rejected as a renderer timing method.

## Changes and correctness

- UI saves skip an identical serialized value, while still observing intervening
  changes and preserving owner isolation. The native adapter also skips unchanged
  values without losing a pending exact receipt after an uncertain reply.
- Completed Markdown reuses its hydrated tree, avoiding a second cache decode and
  render on mount. Cold completed messages start parsing immediately; streaming
  updates retain coalescing and cancellation.
- History scrolling omits a provisional null checkpoint before the actual anchor.
  In-memory anchoring and both existing capture points remain synchronous with
  their original lifecycle. A more aggressive deferred-anchor version was rejected
  because the existing long-history browser test found a follow-to-latest failure.
- Workspace loads on demand with explicit component readiness, keeping its module
  out of the initial bundle. A static import eliminated the first-open delay but
  increased measured startup time, so it was rejected. The explicit loader avoids
  the first-open Suspense fallback delay and preserves import-error handling.

The native adapter microbenchmark uses an in-memory journal and command receiver:
twelve identical saves dropped from twelve native commands and 24 journal writes
to one command and two journal writes. Twelve changed values still generate twelve
commands and 24 journal writes. This establishes eliminated work, not disk latency.

## Measurement artifacts

Raw results and rejected experiments are retained outside the checkout under the
session recovery directory, in `performance-20261016`. `cumulative-comparison.json`
contains the first complete four-variant comparison. The build script and scenario
are maintained in `workspaces/client/apps/web/tests/performance/`.

## Cumulative comparison

Median milliseconds to content plus two animation frames; three samples per stage.

| Action                | Baseline | Save deduplication | Completed Markdown | Scroll checkpoint | Workspace loader |
| --------------------- | -------: | -----------------: | -----------------: | ----------------: | ---------------: |
| chat-short            |    152.3 |              151.6 |              125.7 |             140.5 |            151.1 |
| chat-long             |     32.8 |               32.8 |               36.3 |                41 |             39.3 |
| chat-short            |     32.2 |               32.2 |               29.1 |              23.9 |             23.1 |
| chat-long             |     34.6 |               32.9 |               31.6 |              32.4 |             37.7 |
| settings-Accounts     |     36.8 |               48.5 |               36.5 |              40.8 |             40.8 |
| settings-Appearance   |     29.3 |               30.6 |               26.8 |              24.1 |             30.7 |
| settings-Federation   |       33 |               33.6 |               33.3 |              32.9 |             34.1 |
| settings-Servers      |     31.7 |               31.7 |               30.5 |              30.9 |             31.7 |
| settings-Linux VM     |     32.3 |               31.9 |               18.9 |              33.2 |             31.8 |
| settings-Hotkeys      |     31.5 |               30.7 |                 31 |              33.2 |             30.6 |
| chat-settings         |     38.5 |               36.9 |               38.4 |              38.1 |               51 |
| team-messages         |      334 |              337.7 |              336.6 |             336.3 |             35.6 |
| workspace-Changes     |     89.4 |               89.7 |                 82 |              77.6 |             85.6 |
| workspace-Plan        |     38.1 |               47.4 |               45.4 |              31.7 |             37.8 |
| workspace-Checkpoints |     47.9 |               33.4 |               48.8 |              49.9 |             48.4 |
| workspace-Tools       |     48.5 |                 49 |               49.9 |              46.6 |             47.9 |
| workspace-Profiles    |     47.9 |               48.7 |               48.5 |              49.5 |             49.4 |
| workspace-Rules       |     47.1 |               49.8 |               48.5 |              48.2 |             46.5 |
| workspace-Search      |     32.8 |                 33 |               30.9 |              31.4 |             31.8 |
| older-page            |     80.9 |               79.5 |               81.7 |              78.5 |             79.4 |
| scroll-history        |    433.3 |              432.9 |              434.5 |             433.2 |            432.4 |

The durable-write reductions are stable even where wall-clock timings are not:

- Warm long-chat switches: 12 storage writes to 2–3 after save deduplication.
- Older-page expansion: median 10 writes at baseline, 4 in the final variant.
- Twelve scroll steps: 45 writes at baseline, 24 in the final variant.
- First-open Messages: 334 ms at baseline, 35.6 ms with explicit module readiness.

The scroll scenario intentionally waits two frames per step, so its roughly 433 ms
total is dominated by the prescribed cadence. It does not establish faster
scrolling. Most settings/workspace screen timings and older-page latency show no
material improvement. Startup needs the larger alternating comparison below.

## Final alternating comparison

Seven fresh-context samples per variant, alternating baseline and final order,
confirmed that the larger startup numbers in the three-sample run were not stable.
These are medians from `paired-final.json`, in milliseconds:

| Action                                  | Baseline | Final |
| --------------------------------------- | -------: | ----: |
| Startup: composer ready                 |    199.9 | 194.2 |
| Startup: formatted chat plus two frames |    352.8 | 337.9 |
| First short-chat navigation             |    118.3 | 105.5 |
| First warm long-chat navigation         |     35.9 |  33.1 |
| First Messages opening                  |    353.4 |  34.5 |
| Older formatted page                    |     77.8 |  82.6 |
| Twelve paced scroll steps               |    432.5 | 431.9 |

The startup and chat differences are small; Messages is the clear latency win.
There is no demonstrated older-page or frame-rate improvement. Its measured page
latency was 4.8 ms higher; ranges overlap (baseline 76.5–83.6 ms, final 77.3–84.2 ms).
Reducing storage writes preserves CPU and persistence work, rather than proving
higher scrolling throughput. Native process launch and real remote screen data
remain outside this fixture's measurements.

## Validation

- TypeScript passed with the final on-demand workspace loader.
- Web unit suite: 984 passed, one skipped with the final loader.
- The existing long-history browser scenario passed: bounded mounted history,
  anchors across prepend/resize/reload, focused/selected content, attachment preview,
  and follow-to-latest. Its isolated Vite fixture now explicitly optimizes Dexie.
- Every interaction in all cumulative and final paired runs completed without a
  browser page error. Changed source passed Oxlint; package checks are recorded
  with the delivered artifact.
- Author review checked owner isolation, unchanged-value handling after lost replies,
  streaming cancellation, late Markdown results, and on-demand module unmount/errors.
  No branch merge or push is performed by this work.
