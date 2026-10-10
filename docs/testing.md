# Testing

## Change contract

Client tests own renderer behavior; server tests own Python runtime and provider
contracts. Keep a module's unit tests beside its source in its feature folder.
Cross-component browser scenarios belong in `workspaces/client/apps/web/tests/<feature>/`. A browser
test for one component can live beside that component as `*.spec.mjs`.

Tests must use disposable state and must not attach to an existing Studio backend,
send paid model requests, or interrupt user agents. Desktop checks use hidden
windows. A simulated provider proves only the supplied protocol scenarios.

The root [package scripts](../package.json) are the entry points. Client discovery
and execution belong to the configurations in `workspaces/client/apps/web/`; the server inventory belongs
to [the server runner](../workspaces/runtime/apps/server/tests/server/run.py). Do not add a second handwritten list
of the same tests to this document.

## Choose a suite

Run commands from the repository root after one `pnpm install --frozen-lockfile`.
The root workspace install includes the app dependencies, including the Claude
bridge used by fixture-backed provider replay checks.

Choose the root `test:*` script that matches the layer being changed; the root
[package manifest](../package.json) owns the available command names and wiring.
Client unit checks run without a production build. Browser checks build the
renderer before exercising it. The server listing displays discovered suites;
use the server runner's `--help` for filters and optional categories. The root
server script also runs the colocated JavaScript bridge tests. For a focused
Python contract, use `pnpm run test:server:python -- --filter <name>`.
The Python server runner samples runnable processes for five seconds and uses
their median. CPU jobs are `max(ceil(allowed CPUs / 4), allowed CPUs - competing
runnable tasks)`. The plan then applies the hard memory limit and suite-count
limit. Memory jobs divide the smaller of half `MemAvailable` and
`MemAvailable - 4 GiB` by peak suite RSS. Linux `MemAvailable` already reflects
memory currently occupied by tmpfs/shmem, so scratch is not counted a second
time. The runner selects tmpfs only when free space is at least 256 MiB per planned
worker, then runs a 64 MiB write+fsync quota probe. The bound rounds up the
measured peak of about 4.5 GiB across 25 workers (184 MiB per worker) to leave
headroom for concurrent suite scratch. The probe has caught roots that report free
space but cannot actually write. If tmpfs is unavailable, it uses disk scratch
with the same CPU/memory plan; disk-backed full runs under heavy load are not
currently green (see [known failures](../workspaces/runtime/apps/server/tests/KNOWN-FAILURES.md)).

Measured suite durations only order the longest suites first. Profile data is
stored per repository in the short test cache; peak RSS is replaced with the
latest completed run's measurement, so one outlier affects planning only until
the next run. The plan line reports
`availableCpus` (CPU allowance after competing load and floor), `otherRunnableProcesses`
(sampled competitors), `cpuLimit` (affinity/cgroup ceiling), `cpuFloor`,
`availableMemoryBytes`, `memoryBudgetBytes` (half available memory, retaining the
4 GiB reserve), `measuredPeakSuiteRssBytes`, `measuredWorkerMemoryBytes`,
`memoryWorkerSlots`, `cpuWorkerSlots`, `unclampedCpuCount`,
`estimatedSuiteSeconds`, `runnableSuites`, `workers`,
`codexExecutable` (the one resolved app-server executable or null), and
`limitingBound` (CPU, memory, suite count, or explicit request). `cpuLimit` is the affinity/cgroup ceiling; `cpuFloor` is the
quarter-CPU minimum. `otherRunnableProcesses` is the five-second median minus
the planner's own runnable sample. `--jobs` and `CODEX_SERVER_TEST_JOBS` request a
job count; if a resource bound reduces it, the runner prints the reduction.
`--show-jobs` is advisory and does not reserve slots; use
`--load-sample-seconds 0` or `CODEX_SERVER_TEST_LOAD_SAMPLE_SECONDS=0` for a
fast query. The summary reports suite counts (plain assertion scripts count once per suite),
opt-in/environment skips, runner errors, and elapsed time. It then prints the exact
expected-failure and unexpected-success test IDs and structured unittest outcome counts. Those IDs are
pinned in `workspaces/runtime/apps/server/tests/server/expected_failures.txt`; a mismatch fails the run.
Known product issues and reproductions are in
[`workspaces/runtime/apps/server/tests/KNOWN-FAILURES.md`](../workspaces/runtime/apps/server/tests/KNOWN-FAILURES.md), while expectation edits
are tracked in [`workspaces/runtime/apps/server/tests/TEST-STATUS-CHANGES.md`](../workspaces/runtime/apps/server/tests/TEST-STATUS-CHANGES.md).
Use `python3 workspaces/runtime/apps/server/tests/server/compare_runs.py LOG1 LOG2 ...` to compare failure sets.
Concurrent runners serialize on one per-user lock held for the whole run. A waiting
runner prints `waiting for another test run to finish`, then samples load and plans
workers after the lock is released. The operating system releases the lock if a runner exits abnormally.
Use `--show-jobs` to inspect the plan. Set `--jobs <count>` or
`CODEX_SERVER_TEST_JOBS` to override it manually.
Each suite gets separate short temporary, home, XDG, Codex, Claude, and workspace
directories under the selected scratch root, so parallel suites do not share
mutable test state. `--audit-home` uses Python audit hooks in suites and Python
children to block and record access to the real user's `.codex`, `.claude`, and
`.local/state/codex-agents` trees. For the default PTY suite only, it allows a
read-only open and exec of the single canonical `codexExecutable` printed in the
plan, and permits that exact resolved path as the `CODEX_BIN` and audit-policy
environment values passed to the isolated supervisor. Exec detection also
recognizes the exact executable immediately after a namespace wrapper's `--`
marker; the wrapper and all other command arguments remain audited. The audit
also requires `HOME`, `CODEX_HOME`, and all `XDG_*` home paths in each launched Codex child to
stay outside protected state; this redirected child environment is the
isolation boundary for native executable state. The audit hooks observe Python-level
`open`, list, create, remove, rename, SQLite connect, and `subprocess.Popen`
argv/environment events. They do not observe metadata checks, `exec*`, `posix_spawn`, shell `system`, or filesystem access performed
internally by non-Python native child processes. The child isolation relies on its
redirected `HOME`/`CODEX_HOME`/`XDG_*`; the audit does not inspect native child internals. The host binary is statically
linked, so there are no adjacent shared-library paths to allow. The runner
fails if any blocked access was logged and prints the allowed executable
separately.

For a focused client check, forward a file filter to Vitest or Playwright:

```sh
pnpm --filter codex-agents-web run test:unit -- tokenRate
pnpm --filter codex-agents-web run test:browser -- message-delivery-ui
pnpm --filter codex-agents-web run test:browser:list
```

Install Playwright's managed Chromium with the locally installed CLI when it is
not available. `CHROME_BIN` can select an existing compatible browser. Browser
checks run headlessly. Set `BROWSER=webkit` to select Playwright WebKit after
installing that browser. Live or installed-environment checks and performance
measurements are explicitly selected separately from the default browser suite.
Use `pnpm --filter codex-agents-web run test:performance` for the browser performance group.
To inspect the optional installed-environment group, enable
`PLAYWRIGHT_INCLUDE_SPECIAL=1` and select the `installed-only` project explicitly.

## Add or move a test

- Put a pure TypeScript module and its `*.test.mjs` or `*.test.ts` in the same
  feature folder. Import the real module and use Vitest's named tests.
- Put a component's browser test beside that component as `*.spec.mjs`. Put a
  multi-component workflow in `workspaces/client/apps/web/tests/<feature>/`.
- Use Playwright's fixtures for browser lifecycle and traces. Keep a manual
  browser launch only when browser startup or shutdown is itself under test.
- Preserve assertions when moving an existing test. Update its callers and
  imports, then remove the obsolete executable script.
- Keep Python component tests beside their package. Register cross-cutting
  server contracts with the server runner; do not rely on default discovery to
  find filenames that do not match `test*.py`.
- For long-lived browser fixture servers, import `spawnFixture` from
  `workspaces/client/apps/web/tests/playwright.mjs`. The shared test fixture cleans up registered
  children after success, failure, or timeout. Keep bounded one-shot commands
  separate. Never kill processes that the test did not create.

Storybook retains its separate Vitest browser configuration and its existing
`pnpm --filter codex-agents-web run test:storybook` command. Electron and installed native
helpers have separate verification boundaries; a unit mock does not replace
those checks.

`pnpm run test:desktop` runs the desktop logic suite and its Python recovery
contract. `pnpm run test:desktop:native` selects the hidden Electron application
check and needs desktop dependencies. The commands `test:server:transport` and
`test:server:images` retain the process transport and macOS image-helper checks.
`test:native:schema` explicitly enables the installed Codex schema check.
`workspaces/runtime/apps/server/tests/terminals-contract.py` contains 15 PTY/app-server integration tests and
runs by default when a Codex executable is available. The runner resolves the
binary once (`CODEX_BIN`, then `codex` on `PATH`, then `~/.local/bin/codex`),
prints the resolved path in its plan, and passes that one path to this suite
and the decoy-supervisor isolation test while keeping `HOME`, `CODEX_HOME`, and
`XDG_*` isolated. With `--audit-home`,
the audit permits only read-only open and exec of that exact resolved executable
under the real home; adjacent shared libraries are not needed for this host's
statically linked executable. All other real-home state stays blocked and
logged. If no executable resolves, this suite is reported as an environment
skip with the missing Codex executable named. There is no fake-binary split:
all 15 tests exercise PTY process creation and the real supervisor/app-server
lifecycle protocol, so a stub would remove the contract under test. On the
verified head the default run reports 330 passed suites, 0 failures, and 62
opt-in skips.
`pnpm --filter codex-agents-web run test:rxdb-cache` verifies the runtime patch with an exposed
garbage collector in a child process. These boundaries are not replaced by
successful browser discovery or mocked unit tests.

## Mutation testing

Stryker changes selected client modules in a temporary sandbox and asks the
colocated Vitest tests to detect those changes. The explicit source selection,
concurrency and report paths are owned by
[the Stryker configuration](../workspaces/client/apps/web/stryker.config.mjs).

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
[their provenance](../workspaces/runtime/apps/server/tests/fixtures/provider-replay/README.md).

Before committing, run the affected suites plus the repository lint and format
checks. The [pre-commit hook](../.githooks/pre-commit) checks staged content only;
it does not replace behavior tests.
