# Testing

## Change contract

Client tests own renderer behavior; server tests own Python runtime and provider
contracts. Keep a module's unit tests beside its source in its feature folder.
Cross-component browser scenarios belong in `workspaces/client/apps/web/tests/<feature>/`. A browser
test for one component can live beside that component as `*.spec.mjs`.

Tests must use disposable state and must not attach to an existing Studio backend,
send paid model requests, or interrupt user agents. Desktop checks use hidden
windows. A simulated provider proves only the supplied protocol scenarios.

Representative cold and warm check measurements are recorded in [the 2026-10-11 timing report](verification/2026-10-11-check-timings.md).

The root [package scripts](../package.json) are the entry points. Client discovery
and execution belong to the configurations in `workspaces/client/apps/web/`; the server inventory belongs
to [the server runner](../workspaces/runtime/apps/server/tests/server/run.py). Do not add a second handwritten list
of the same tests to this document.

## Fast and heavy tiers

The fast tier is deterministic. Each server suite is expected to finish in at
most 10 seconds when run alone; browser smoke targets under 120 seconds total.
It is the default developer loop, the required
pre-PR and pre-merge check, and runs in hosted CI on every pull request and push
to `main`, with the server tier split across four parallel jobs and browser
smoke in its own job. A direct push to `main` is preceded by `just test-fast`,
which includes browser smoke. Use
`just test-fast` for the whole repository. For one
server suite between edits, use
`just test server <exact-suite-path>`; exact paths automatically include suites
from the automated or heavy list. At the end of a default run, the runner reports
suite durations over 10 seconds measured inside that parallel run. These values
are informational; check a suite's 10-second budget by running it alone with
`just test server <exact-suite-path>`.

Heavy runs are manual, announced runs and are excluded from hosted CI for now.
The runner owns the server suite categories; the commands below select them
without maintaining a second suite inventory.

| Tier or purpose     | What it runs                                                                                                                              | Just command                               |
| ------------------- | ----------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------ |
| Fast                | Default server run, server bridge, Rust nextest, web type checks and unit tests, tooling tests, browser smoke subset                      | `just test-fast`                           |
| Fast browser smoke  | Modal/popover focus and layering, mobile composer geometry, receipt pagination, schema-cache checkpoint retention, account settings smoke | `just test-browser-smoke`                  |
| Heavy server        | All suites in the heavy server list                                                                                                       | `just test-server-heavy`                   |
| Heavy browser       | Full web browser project, capped at 900 seconds                                                                                           | `just test-browser`                        |
| All heavy           | Heavy server suites, then the full browser project, sequentially                                                                          | `just test-heavy`                          |
| Other server opt-in | One runner category: `automated`, `native`, `browser`, `live`, or `vm`                                                                    | `just test-server-opt-in <category>`       |
| Load-class          | One exact load-class suite, with conditions printed before it starts                                                                      | `just test-server-load <exact-suite-path>` |

The Rust runtime differential corpora use one isolated Python JSON-lines oracle
per test process and Python-written SQLite seed backups. Default nextest runs
explicit fast witnesses; `just test-rust-runtime-corpora` sets
`STUDIO_RUNTIME_CORPUS_FULL=1` and runs the full lifecycle/notification/preparation matrices
as an announced heavy run. This command also runs the full account-worker crash
matrix at every before/after store-bridge boundary; default nextest runs one
witness per scenario and the reservation-recovery point. Worker scenarios clone
one shared Python-written seed, and report scenario replay separately from
restart work when `MATRIX_REPORT` is set. The notification [coverage manifest](../workspaces/runtime/packages/runtime-services/tests/turn_lifecycle/notification_coverage.json)
records every method, ordering, review regression and approved deviation; its
runner requires every method and review regression to remain in the fast set.
See the [notification corpus guide](../workspaces/runtime/packages/runtime-services/tests/turn_lifecycle/NOTIFICATION-CORPUS.md)
for the protocol, normalization and timing evidence. These corpora use disposable
state, stub provider transport and background workers, and create no load generators. The
[preparation corpus](../workspaces/runtime/packages/runtime-services/tests/turn_prepare/README.md)
uses a minimal host and in-memory SQLite without constructing a Runtime; it checks
pure context selection, event projection, transcript inputs and acceptance.

`just test-rust-runtime-corpora-mutations` is a separate manual heavy run. It copies
the checkout's sources below the external `CARGO_TARGET_DIR`, applies each entry
in `tests/turn_lifecycle/mutations.json` and `tests/turn_prepare/mutations.json` only to that scratch copy, and runs both
default fast corpora and account-worker witnesses. It uses two build jobs and no load generators. Each mutation
must cause a test failure; build and infrastructure failures are invalid evidence.
The untouched baseline must pass. Logs, CPU/wall timings, and `results.json` stay
in that target directory. `--full --allow-survivors` records a diagnostic baseline
for both tiers; the default fails for any surviving mutation. Set the isolated
environment and prepared read-only interpreter as for the corpus runs.

The probabilistic provider-session scheduling diagnostic is excluded from the
default set and runs only through `just test-rust-session-fairness`. It uses real
Tokio scheduling; it is not evidence of a deterministic fairness bound. The
worker has a separate deterministic command-priority witness. Real stdio and
supervisor tests use disposable processes and wall-clock watchdogs; only scripted
worker deadline scenarios use paused time throughout.

### Background-owned contract state

The Python dispatcher and periodic ticks can update these fields between an HTTP
response and a test's database read. Contract suites use the shared
[`no_write_snapshot`](../workspaces/runtime/apps/server/tests/http_backend_harness.py)
helper, which snapshots every table and removes dispatcher- and scheduler-owned
fields by table and field name. Its `BACKGROUND_OWNED_FIELDS` map is the sole
field-list authority, including individual `timing.fast*` projections. Tests
that intentionally exercise a background-owned value should hold dispatch and
assert that value directly. Scheduler journal tests may also assert those
fields explicitly.

The heavy server suites and the reason for each classification are owned by
`HEAVY_SUITES` in the [server runner](../workspaces/runtime/apps/server/tests/server/run.py):

| Suite                               | Measured duration | Reason                                                |
| ----------------------------------- | ----------------: | ----------------------------------------------------- |
| `agent-move-signed-integration.py`  |           44.13 s | Signed-in multi-server lifecycle flow                 |
| `server-exec-signed-integration.py` |           38.16 s | Signed-in server execution flow                       |
| `process-supervisor-contract.py`    |           36.05 s | Contract requires a replay wait of about 10.4 seconds |

The load-class suite `workspaces/runtime/apps/server/tests/notification-scheduler-contract.py`
contains a 1,500-notification storm with a live scheduler. Its busy-retry connection
count depends on runtime contention, so select it only with
`just test-server-load workspaces/runtime/apps/server/tests/notification-scheduler-contract.py`.

Measure a suite by running it alone through `just test server <exact-suite-path>`.
If it exceeds 10 seconds, speed it up first; the maintainer decides whether it
must move to heavy because it cannot be made fast and deterministic. A suite
that becomes fast moves back to the fast tier. A test that needs repeated runs
to be trusted is defective and must be rewritten or moved. New tests must fit
the fast tier. Load-class suites remain separate and require their own selected
run; they are never included in `just test-heavy`.

## Choose an evidence scope

Use the root [`justfile`](../justfile) for the three independent scopes:

1. **One hypothesis:** `just test <package> [case]`; use `just test-plan` first
   to print the exact native command and setup without executing it.
2. **One change:** `just check <package>` runs that package's local checks;
   `just check <package> --dry-run` prints the planned commands.
3. **Pre-PR gate:** run `just test-fast` and report its result. A focused
   hypothesis does not replace this gate.

The repository checks add three focused commands: `just contracts-check` runs
the existing generated API check in read-only mode; `just boundaries-check`
checks manifest-declared imports and workspace direction; and
`just check-affected <base>` checks changed packages and reverse dependents
discovered from workspace manifests. It widens to all packages for root policy
or unowned-file changes and prints the reason per selected package.
`just check-affected <base> --dry-run` prints that selection without running
checks.

Package names and directories come from pnpm and Cargo manifests, with `server`
representing the current Python app. Native manifests and runners remain the
owners of test discovery and selection. All facade commands work from a
repository subdirectory and save complete test logs outside the checkout in
`$XDG_CACHE_HOME/codex-studio/checks` (or `~/.cache/codex-studio/checks`). Set
`CODEX_STUDIO_CHECKS_CACHE` to choose another location. The facade removes run
directories older than 14 days at the start of each run; recent logs are kept
for reproductions.

For Vitest packages, the positional case is always a substring of the test
file path, so `just test codex-agents-web errorPresentation` loads the matching
file. Add `--name <pattern>` to pass a test-name filter (`-t`) as well, which
lets a named case stay within one file. Cargo uses the positional case as a
nextest test-name filter; `--name <pattern>` intersects another nextest
test-name substring filter. Runner options after `--` remain available (for
example, `just test studio-diagnostics tests::formatter_matches_python_json_dump_golden -- --exact`).
Cargo target selectors such as `--test compatibility` are routed to Cargo, for
example `just test studio-diagnostics '' -- --test compatibility`. The server
uses its runner's `--filter` suite selector. `just test-plan` prints the
selector and the exact native command without execution.

Install the required runner version in the [nextest configuration](../.config/nextest.toml) locally with
`cargo install cargo-nextest --version 0.9.146 --locked`. Nextest runs each
test in its own process and does not run doctests; `just check` and CI run
`cargo test --doc` separately.

Each pnpm package declares its focused recipe mapping in its manifest under
`codexStudioChecks`: `test` names the focused test script, and `check` lists
the package-local check scripts. These mappings must invoke only that package;
they must not install dependencies or build unrelated packages.

## Choose a suite

Run commands from the repository root after one `pnpm install --frozen-lockfile`.
The root workspace install includes the app dependencies, including the Claude
bridge used by fixture-backed provider replay checks.

Choose the root `test:*` script that matches the layer being changed; the root
[package manifest](../package.json) owns the available command names and wiring.
Client unit checks run without a production build. Browser checks build the
renderer before exercising it. The browser runner plans workers from its
measured per-worker CPU budget, memory headroom, and competing runnable tasks;
an explicit Playwright `--workers` value overrides the plan. It reports the
selected worker count before discovery. Fixture scratch is selected by a
write-and-fsync probe: tmpfs is preferred when it has the per-worker reserve,
with disk scratch as the fallback. Set `PLAYWRIGHT_SCRATCH_ROOT` to request a
specific writable location. The global setup resolves the equipped fixture
Python once, checks its API dependencies before workers start, and passes that
interpreter to fixtures. Shared fixtures isolate `HOME`, `CODEX_HOME`, and
`XDG_*` state; tests that need a signed-out account opt out of the synthetic
fixture account explicitly. Registered fixture process groups are reaped after
success, failure, and timeout.

The server listing displays discovered suites; use the server runner's `--help`
for filters and categories. The root server script also runs the colocated
JavaScript bridge tests. For a focused Python contract, use
`pnpm run test:server:python -- --filter <name>`. The runner owns the audited
automated suite list used by `just test-server-opt-in automated` and the hosted
CI audited automation step. Load-class suites include load, scale, latency, and
contention checks; select one only with `just test-server-load <exact-path>`.

## HTTP boundary contract tests

A boundary contract test starts a backend process and observes its public HTTP
responses, provider protocol frames, and HTTP state after a clean restart. It
does not import `codex_runtime` or any server module on the assertion side and
does not inspect the database. Run the first reusable corpus with:

```sh
just test server workspaces/runtime/apps/server/tests/http-boundary-contract.py
```

By default it launches the same Python entry point used by the desktop launcher
with an ephemeral port. To run the corpus against another backend without
changing the tests, set `STUDIO_HTTP_BACKEND_COMMAND` to a shell-style command;
the selected executable must accept `--port 0` and print its loopback ready URL
as `Codex Canvas: http://127.0.0.1:<port>`:

```sh
STUDIO_HTTP_BACKEND_COMMAND='/path/to/codex-backend' \
  just test server workspaces/runtime/apps/server/tests/http-boundary-contract.py
```

The command setting applies to every suite using `http_backend_harness.py`.
The harness clears Python-backend selector variables for an explicit command,
keeps state and credentials disposable, and restricts the child `PATH` to the
replay Codex executable and system directories. A representative provider plus
supervisor fixture can be checked with:

```sh
just test server workspaces/runtime/apps/server/tests/protocol-fixtures-contract.py
STUDIO_HTTP_BACKEND_COMMAND='/path/to/codex-backend' \
  just test server workspaces/runtime/apps/server/tests/protocol-fixtures-contract.py
```

The fixture checker consumes the existing
`restart-mid-turn-supervisor-replay.json` provider transcript and the
`workspaces/runtime/apps/server/tests/fixtures/supervisor-protocol/supervisor-v1-http.json`
socket frames, then checks the public transcript and message receipt. The fixture's supervisor
response projections were captured from the disposable supervisor. Its negative
control changes the expected health protocol on a live exchange and requires
the same frame checker to fail.
The checker currently executes Unix sockets; Windows named-pipe execution is not
covered by this checker.

The supervisor v1 action inventory comes from
`codex_process_supervisor.py:Supervisor.handle`:

| Action             | Fixture coverage | Reason                                                                                             |
| ------------------ | ---------------- | -------------------------------------------------------------------------------------------------- |
| `health`           | Covered          | Health snapshot and durability are compared.                                                       |
| `open`             | Covered          | Starts a disposable Python child and checks its open receipt.                                      |
| `next`             | Covered          | Replays recorded stdout, stderr, and exit events.                                                  |
| `finishFallback`   | Not covered      | Requires the recovery fallback state transition.                                                   |
| `adminCloseHandle` | Not covered      | Privileged operator-only action; no operator process is started.                                   |
| `retireCommand`    | Not covered      | Requires the separate completed `server-command:` retirement lifecycle.                            |
| `write`            | Not covered      | Provider RPC writes are exercised by provider replay, not hand-built socket calls.                 |
| `operationStatus`  | Not covered      | Requires a persisted monitor operation and native RPC identity.                                    |
| `ack`              | Not covered      | Destructive journal cursor mutation is covered by Python implementation tests.                     |
| `replay`           | Not covered      | Durable journal replay internals remain covered by Python implementation tests.                    |
| `status`           | Not covered      | The fixture checks open receipt, health snapshot, and event stream instead.                        |
| `detach`           | Not covered      | Connection close is exercised by backend restart; standalone detach has no observable HTTP result. |

The recorded frame kinds are handshake (`ok`), successful request response
(`result`), rejected handshake (`error`), and child event kinds `stdout`,
`stderr`, and `exit`. All are covered by the fixture. Provider reattach after a
backend restart is exercised by the referenced provider fixture and its HTTP
transcript recovery assertion.

During this specification phase the harness still launches the disposable
Python supervisor. `docs/plans/rust-workspaces.md` decision D-01 and milestone
M-07 call for the owned backend, supervisor, and CLI to move together after
compatibility is frozen; selecting a separate supervisor executable here would
test a partial migration the plan explicitly avoids. An arbitrary backend can
therefore be tested against the current Python supervisor's wire behavior;
the runtime move will replace that implementation at the agreed whole-runtime
cutover.

Some suites intentionally test Python implementation details and are not part
of this portable HTTP corpus: `provider-replay-runner.py` and
`server/test_provider_replay_protocol.py` import `Runtime`/`AppServer` to test
the Python replay adapter; `http-cache-contract.py` constructs `Canvas` and
`Runtime` to inject static assets; `http-request-traces-contract.py` patches
the Python tracing module and controls a blocked callback. The supervisor
implementation suites (`process-supervisor-contract.py`,
`supervisor-persistence-retry-contract.py`,
`supervisor-reattach-barrier-contract.py`,
`supervisor-restore-selection-contract.py`,
`supervisor-snapshot-schema-contract.py`,
`supervisor-response-map-contract.py`,
`supervisor-journal-connection-reuse-contract.py`, and
`supervisor-stream-phase-load-contract.py`) inspect Python supervisor state or
fault-injection points; those remain implementation tests until their
observable cases have matching backend-neutral fixtures.

The server runner supplies `TMPDIR` on memory-backed scratch.
The harness places state, home directories, Codex and Claude configuration,
XDG directories, and provider replay files beneath that root. New boundary
contracts must keep assertions at HTTP/provider/process boundaries and must not
read SQLite or import server modules.
Write each contract as a separately named `unittest` method so the runner can
report, select, and reproduce it by test ID. Share a backend only within a
class whose methods create independent data; put restart durability in its own
class, keep process-owning suites unsliced, and record readiness timings without
asserting on speed.

The backend requires the native lifecycle module. Build the module, then install
it before server tests or backend startup. For test runs, keep both the managed
Python cache and command-bin directory in a dedicated temporary root; never
install into a cache or bin directory used by a live backend:

```sh
test_install_root="$(mktemp -d /var/tmp/codex-studio-test-install.XXXXXX)"
export XDG_CACHE_HOME="$test_install_root/cache"
test_bin_dir="$test_install_root/bin"
mkdir -p "$XDG_CACHE_HOME" "$test_bin_dir"
cargo build --release -p studio-operations-python
python3 workspaces/runtime/apps/server/src/install-cli.py --dev --bin-dir "$test_bin_dir"
test_python="$(python3 workspaces/runtime/apps/server/src/codex_python.py)"
"$test_python" -c "import sys; print(sys.prefix)"
```

Use `--dev` when the checks need the isolated development and mypy environment;
omit it when only the runtime environment is needed. The installer copies the
built library and never builds it implicitly. Check that the printed interpreter
prefix is under the isolated temporary root. Both the cache and command-bin
directory must be isolated from any live backend.

The Python server runner samples runnable processes for five seconds and uses
their median. CPU jobs are `max(ceil(allowed CPUs / 4), allowed CPUs - competing
runnable tasks)`. The plan then applies the hard memory limit and suite-count
limit. Memory jobs divide the smaller of half `MemAvailable` and
`MemAvailable - 4 GiB` by peak suite RSS. Linux `MemAvailable` already reflects
memory currently occupied by tmpfs/shmem, so scratch is not counted a second
time. macOS has neither `/proc/stat` nor `MemAvailable`: there the competing
runnable count is the one-minute load average less one, and available memory is
the free, inactive and speculative pages from `vm_stat`. The runner selects tmpfs only when free space is at least 256 MiB per planned
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
Full server runs serialize on one per-user lock held for the whole run. A waiting
full runner prints `waiting for another test run to finish`, then samples load and
plans workers after the lock is released. A selection resolving to one runnable
suite uses a bounded per-user focused slot instead, so it can overlap a full run
or another focused run. Focused slots are capped at four and by allowed CPUs; their
combined measured peak RSS is limited by the full planner's available-memory budget
and existing [4 GiB memory reserve](../workspaces/runtime/apps/server/tests/server/run.py).
If that budget cannot admit one measured suite, the selection uses the whole-run lock.
If all focused slots are busy, the runner prints
`waiting for a focused-run admission slot`. Both lock types are kernel-managed and
are released when the runner exits. Focused runs keep the same per-suite isolated
temporary, home, XDG, Codex, Claude, and workspace directories, and do not write
the shared timing/profile cache. `--show-jobs` reports whether a selection uses
focused slots or the whole-run lock. Set `--jobs <count>` or
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

For manual native client checks outside the facade, forward a file filter to Vitest or Playwright:

```sh
pnpm --filter codex-agents-web run test:unit -- tokenRate
pnpm --filter codex-agents-web run test:browser -- tests/notices/browser-access-notice-ui.spec.mjs
pnpm --filter codex-agents-web run test:browser:list
```

The browser command builds the renderer, then uses the web package's browser
launcher. It redirects `HOME`, `CODEX_HOME`, `CLAUDE_CONFIG_DIR`, and XDG
configuration, cache, data, state, and runtime paths to temporary storage for
the run. It keeps Playwright's browser lookup on the standard per-user cache
even when fixture setup redirects `XDG_CACHE_HOME`; an explicit
`PLAYWRIGHT_BROWSERS_PATH` or `CHROME_BIN` still takes precedence. The launcher
prints its unique temporary output directory at the end; JSON results, traces,
and attachments are kept there so concurrent or later runs cannot overwrite
them.
The launcher inherits the host's `LD_LIBRARY_PATH` and `FONTCONFIG_FILE` when
set and relies on the system browser libraries and fonts otherwise. Install
Playwright's managed Chromium with
`pnpm --filter codex-agents-web exec playwright install chromium` when it is not
available. Browser checks run headlessly. WebKit requires both its Playwright
browser and its host system libraries; the mobile usability and request-deadline
tests skip their WebKit case with an explicit reason when a required library is
missing. WebKit coverage runs only on machines where WebKit is installed; this
machine and CI currently do not have it. Set `BROWSER=webkit` to select
Playwright WebKit. The default browser project is Chromium, although the mobile
usability test also starts WebKit directly. Live or installed-environment checks
and performance measurements are explicitly selected separately from the
default browser suite. The current CI workflow runs the client unit tests but
does not run the Playwright browser suite.
Browser runs need a prepared Python environment and the native operations module
that matches this checkout. Set a fresh scratch root, build the native module,
then install the CLI into an isolated cache and bin directory. The installer
must not inherit `CODEX_AGENTS_PYTHON`; resolve the resulting interpreter only
after installation:

```sh
BROWSER_TEST_ROOT=/var/tmp/codex-browser-test
CARGO_TARGET_DIR="$BROWSER_TEST_ROOT/cargo-target" cargo build --release -p studio-operations-python
XDG_CACHE_HOME="$BROWSER_TEST_ROOT/python-cache" env -u CODEX_AGENTS_PYTHON CARGO_TARGET_DIR="$BROWSER_TEST_ROOT/cargo-target" python3 workspaces/runtime/apps/server/src/install-cli.py --dev --bin-dir "$BROWSER_TEST_ROOT/bin"
export CODEX_AGENTS_PYTHON="$(XDG_CACHE_HOME="$BROWSER_TEST_ROOT/python-cache" python3 workspaces/runtime/apps/server/src/codex_python.py)"
export PATH="$(dirname "$CODEX_AGENTS_PYTHON"):$PATH"
```

The launcher checks the Python dependencies and imports the server's native
operations adapter before discovery, which reports the build instruction for a
missing or mismatched native module. It keeps run state private.

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
lifecycle protocol, so a stub would remove the contract under test.
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

## Continuous integration

The public GitHub Actions workflow runs on pull requests and pushes to `main`
on `ubuntu-latest`. It uses read-only repository permissions, no secrets,
locked dependency installation, and the same package commands listed in each
check step. The Rust job reads the pinned toolchain in `rust-toolchain.toml`.
The Node.js jobs use the Node 22 release line; the documented minimum is
22.15. The Python job tests API generation at the configured minimum and
maximum supported versions and prepares digest-keyed managed environments
through `install-cli.py --dev`; that command creates virtual environments and
installs the pinned PyPI requirements, without installing provider binaries.
The Python version matrix is owned by the
[CI workflow](../.github/workflows/ci.yml). Cache and command-bin directories
stay under the runner temporary directory. The facade job downloads the Just
1.58.0 Linux release, checks its pinned SHA-256, and checks the reported
version before invoking it.

Current hosted checks run on pull requests and pushes to `main`. The server
workflow runs the 27 audited automated suites, the bridge suite, and one focused
Python suite; it does not run the whole default server run. Heavy and load-class
suites are excluded from hosted CI. The audited automation step was restored
after its timing-sensitive suites were made deterministic in PR #80.

- `pnpm run lint:all` and `pnpm run format:check:all` currently exit 1 on
  `main` because of existing findings. M-03.5 will add conservative affected
  checks; changed-file lint and formatting can then be introduced without
  making unrelated baseline findings block a pull request.
- Provider-dependent and load-class suites remain separate opt-in evidence.
- M-03.6 is complete: representative cold and warm timings are recorded in
  [the 2026-10-11 timing report](verification/2026-10-11-check-timings.md).
- Desktop tests that require Electron and a display remain outside this
  headless Linux gate; M-02.5 tracks the remaining desktop verification.
- macOS and Windows are not CI targets in this workflow. M-09.1 owns the
  platform install, update, and rollback matrix that will define those jobs.

## Interpret evidence

Report which suites actually ran, which failed, and which were excluded. A
successful test listing proves discovery, not behavior. A browser fixture with a
real HTTP server and SQLite proves integration with that fixture, not a live
provider request. Replay transcripts are maintained offline contracts; see
[their provenance](../workspaces/runtime/apps/server/tests/fixtures/provider-replay/README.md).

Before committing, run the affected suites plus the repository lint and format
checks. The [pre-commit hook](../.githooks/pre-commit) checks staged content only;
it does not replace behavior tests.
