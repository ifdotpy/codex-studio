# Testing

## Change contract

Client tests own renderer behavior; server tests own Python runtime and provider
contracts. Keep a module's unit tests beside its source in its feature folder.
Cross-component browser scenarios belong in `tests/client/<feature>/`. A browser
test for one component can live beside that component as `*.spec.mjs`.

Tests must use disposable state and must not attach to an existing Studio backend,
send paid model requests, or interrupt user agents. Desktop checks use hidden
windows. A simulated provider proves only the supplied protocol scenarios.

The root [package scripts](../package.json) are the entry points. Client discovery
and execution belong to the configurations in `web/`; the server inventory belongs
to [the server runner](../tests/server/run.py). Do not add a second handwritten list
of the same tests to this document.

## Choose a suite

Run commands from the repository root after `npm ci` and `npm --prefix web ci`.
Server and provider replay checks also require `npm --prefix scripts/claude_bridge ci`
for the pinned Claude bridge dependencies, including fixture-backed provider runs.

```sh
npm run test:client:unit
npm run test:client:browser
npm run test:server:list
npm run test:server
npm run test:mutation
```

`test:client` runs the client unit and browser suites. Unit tests run without a
production build. Browser checks build the renderer before exercising it.
`test:server:list` displays the available server suites without executing them;
use the server runner's `--help` for filtering and optional categories.
`test:server` also runs the colocated JavaScript bridge tests. To filter Python
contracts, use `npm run test:server:python -- --filter <name>`.
The Python server runner samples runnable processes for five seconds and sizes
its CPU allowance as the larger of one quarter of allowed CPUs and allowed CPUs
minus the higher of the maximum competing runnable count in the first two
seconds and the five-second median. This catches short bursts without allowing
a stale one-minute load average to suppress a later run. It then limits that count by the
runnable suite count and memory slots. Memory slots divide the smaller of half
`MemAvailable` and `MemAvailable` minus a 4 GiB reserve by the sum of measured
peak suite RSS and measured per-suite scratch bytes. Scratch bytes count against
the memory bound because a selected tmpfs stores them in RAM. The runner probes
quota by writing and syncing one file per candidate mount, capped at 256 MiB; it
requires statvfs headroom for measured scratch bytes times selected workers.
Measured suite durations order the longest suites first. `--jobs` and
`CODEX_SERVER_TEST_JOBS` override the automatic selection. `--show-jobs` prints
which resource bound selected the count; use `--load-sample-seconds 0` or
`CODEX_SERVER_TEST_LOAD_SAMPLE_SECONDS=0` for a fast one-shot plan query.
The checked-in [timing seed](../tests/server/timing-baseline.json) comes from
the exact base run; by default, successful runs update a repository-keyed local
profile under the short test cache root. Complete runs refresh aggregate RSS
and scratch peaks and decay prior peaks by 10% per run; filtered runs do not
decay them. On Linux, the runner uses a writable tmpfs scratch
mount when its free capacity covers the measured per-suite footprint for the
selected worker count, then falls back to the short cache root. It checks the
deepest known fixture Unix-socket paths before launching each suite and reports
an actionable error if an explicitly configured root leaves insufficient path
space. Scratch roots record their owning runner PID; a later run removes only
marked roots whose owner process has exited. Set
`CODEX_SERVER_TEST_TMP_ROOT` to choose a scratch root explicitly. See the [runner](../tests/server/run.py)
for the resource and scratch-root formulas. When the selected root is disk-backed,
the runner measures the median of seven 4 KiB write+fsync probes and scales the
CPU-derived worker count by `5.5 ms / (5.5 ms + measured fsync latency)`; the
5.5 ms reference is calibrated from the measured disk-vs-tmpfs runtime-contract
CPU and wall times. Probe results varied substantially across runs, so this is
a coarse guard against slow disk-backed scratch, not a precise runtime estimate.
This disk-only I/O bound is reported as `limitingBound=io`.
Before starting suites, each runner atomically claims its selected CPU and
measured memory slots in a shared live-runner registry under the short test
cache root. Concurrent invocations reduce their plans against active claims;
claims from exited processes are discarded. If an active claim would leave a
runner with less than half its planned workers, it waits up to three minutes,
then replans; after the bound it continues with available slots. `--show-jobs`
is an advisory plan and does not reserve slots.
Use `--show-jobs` to inspect the plan. Set `--jobs <count>` or
`CODEX_SERVER_TEST_JOBS` to override it manually.
Each suite gets separate short temporary, home, XDG, Codex, Claude, and workspace
directories under the selected scratch root, so parallel suites do not share
mutable test state. `--audit-home` enables a Python audit hook in each suite and
its Python children; it fails if they open or create anything under the real
user's `.codex`, `.claude`, or `.local/state/codex-agents` directories. Use this
to verify isolation without reading or logging file contents. The hook does not
inspect non-Python child processes.

For a focused client check, forward a file filter to Vitest or Playwright:

```sh
npm --prefix web run test:unit -- tokenRate
npm --prefix web run test:browser -- message-delivery-ui
npm --prefix web run test:browser:list
```

Install Playwright's managed Chromium with the locally installed CLI when it is
not available. `CHROME_BIN` can select an existing compatible browser. Browser
checks run headlessly. Set `BROWSER=webkit` to select Playwright WebKit after
installing that browser. Live or installed-environment checks and performance
measurements are explicitly selected separately from the default browser suite.
Use `npm --prefix web run test:performance` for the browser performance group.
To inspect the optional installed-environment group, enable
`PLAYWRIGHT_INCLUDE_SPECIAL=1` and select the `installed-only` project explicitly.

## Add or move a test

- Put a pure TypeScript module and its `*.test.mjs` or `*.test.ts` in the same
  feature folder. Import the real module and use Vitest's named tests.
- Put a component's browser test beside that component as `*.spec.mjs`. Put a
  multi-component workflow in `tests/client/<feature>/`.
- Use Playwright's fixtures for browser lifecycle and traces. Keep a manual
  browser launch only when browser startup or shutdown is itself under test.
- Preserve assertions when moving an existing test. Update its callers and
  imports, then remove the obsolete executable script.
- Keep Python component tests beside their package. Register cross-cutting
  server contracts with the server runner; do not rely on default discovery to
  find filenames that do not match `test*.py`.
- For long-lived browser fixture servers, import `spawnFixture` from
  `tests/client/playwright.mjs`. The shared test fixture cleans up registered
  children after success, failure, or timeout. Keep bounded one-shot commands
  separate. Never kill processes that the test did not create.

Storybook retains its separate Vitest browser configuration and its existing
`npm --prefix web run test:storybook` command. Electron and installed native
helpers have separate verification boundaries; a unit mock does not replace
those checks.

`npm run test:desktop` runs the desktop logic suite and its Python recovery
contract. `npm run test:desktop:native` selects the hidden Electron application
check and needs desktop dependencies. The commands `test:server:transport` and
`test:server:images` retain the process transport and macOS image-helper checks.
`test:native:schema` explicitly enables the installed Codex schema check.
`npm --prefix web run test:rxdb-cache` verifies the runtime patch with an exposed
garbage collector in a child process. These boundaries are not replaced by
successful browser discovery or mocked unit tests.

## Mutation testing

Stryker changes selected client modules in a temporary sandbox and asks the
colocated Vitest tests to detect those changes. The explicit source selection,
concurrency and report paths are owned by
[the Stryker configuration](../web/stryker.config.mjs).

A surviving mutation is a candidate for a missing assertion, not an instruction
to add a test that merely repeats the implementation. Check whether the changed
behavior is observable and required. Record equivalent or deliberately excluded
mutations with a reason. Mutation reports cover only the selected sources; they
do not measure Python, Electron or the whole application's reliability.

Reports remain local. The initial mutation command is diagnostic and has no
arbitrary score gate. A failing baseline test or runner error is a failed run,
not a successful mutation score. Choose any future merge threshold using a
measured baseline and an explicit scope.

## Interpret evidence

Report which suites actually ran, which failed, and which were excluded. A
successful test listing proves discovery, not behavior. A browser fixture with a
real HTTP server and SQLite proves integration with that fixture, not a live
provider request. Replay transcripts are maintained offline contracts; see
[their provenance](../tests/fixtures/provider-replay/README.md).

Before committing, run the affected suites plus the repository lint and format
checks. The [pre-commit hook](../.githooks/pre-commit) checks staged content only;
it does not replace behavior tests.
