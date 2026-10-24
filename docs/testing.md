# Testing

## Change contract

Rust crates own runtime, state, provider, and protocol tests. Client packages own
renderer and desktop behavior. Keep unit tests beside their source and browser
scenarios in the web test feature folders. The root [Just recipes](../justfile),
[package scripts](../package.json), and [facade](../workspaces/tooling/apps/repository-checks/facade.mjs)
own check selection; this page explains how to use those owners.

Tests use disposable state and must not attach to an existing backend, send paid
model requests, or interrupt active agents. Automated desktop checks use hidden
windows without operating-system input. Fixture success does not prove a live
model request or a real reset-credit redemption.

## Fast, comprehensive, and manual checks

`just test-fast` runs selected deterministic Rust safety witnesses, desktop
recovery and retained-helper boundaries, web API/type witnesses, tooling tests,
and generated-contract and dependency guards, and the VM guest Python suite through its
[runner](../workspaces/runtime/apps/vm-guest/run-guest-suite.mjs). It generates web WASM before web
checks. The [WASM builder](../workspaces/client/apps/web/scripts/build-client-wasm.mjs)
requires the exact binding generator version pinned by the client-wasm crate;
its missing-tool error gives the installation command. Cold builds are outside
the warm short-tier target. The short tier starts no browser or model provider.

`just test-comprehensive` runs the provider bridge, workspace Clippy, workspace
Rust nextest, web type and unit checks, tooling tests, and browser smoke. It builds
the Rust HTTP fixture and the renderer embedded by the native client. Run it on
the exact commit before every push to `main` and before packaging for installation.
Run `just test-fast` before every PR and merge and state its result in the PR.
Workers run focused witnesses; the lead runs these gates on the integrated batch.

Both tiers use Cargo contracts and Rust-owned fixture data. The Python backend,
its pseudo-package, native operations extension, mypy gate, runner categories,
server shards, oracle processes, and Python mutation drivers have been removed.
The guest service and desktop Python helpers are retained and remain independent
of the backend. Historical verification reports describe their recorded revision;
their removed Python commands are not current instructions.

| Purpose                                              | Command                                  |
| ---------------------------------------------------- | ---------------------------------------- |
| Short safety gate                                    | `just test-fast`                         |
| Publication gate                                     | `just test-comprehensive`                |
| Generated contracts                                  | `just contracts-check`                   |
| Workspace boundaries                                 | `just boundaries-check`                  |
| Browser smoke                                        | `just test-browser-smoke`                |
| Full browser project, manual heavy                   | `just test-browser` or `just test-heavy` |
| Multi-server browser recovery, manual heavy          | `just test-browser-multi-server`         |
| Full deterministic Rust corpora, manual heavy        | `just test-rust-runtime-corpora`         |
| Full account-worker crash matrix, manual heavy       | `just test-crash-matrix-full`            |
| Provider-session scheduling diagnostic, manual heavy | `just test-rust-session-fairness`        |
| Many-agent scenario, manual heavy                    | `just test-rust-many-agents`             |
| Continuation corpus and store faults, manual heavy   | `just test-rust-usage-resume-corpora`    |
| Hidden Electron profile migration, manual heavy      | `just test-native-profile-migration`     |
| Hidden Rust native WebView scenarios, manual heavy   | `just test-native-webview`               |
| Hidden Rust native WebView scenarios, manual heavy   | `just test-native-webview`               |

Set `STUDIO_STORE_COMPRESS_EVERY_WRITE=1` to run any Rust selection in the
compress-every-write mode of [`stored_text.rs`](../workspaces/runtime/packages/runtime-store/src/stored_text.rs):
every writer of a compressed text column then stores the compressed form, so
the selected tests check that every reader decodes it. For example,
`STUDIO_STORE_COMPRESS_EVERY_WRITE=1 just test studio-runtime-services notification_dispatch`.

Heavy runs are announced and selected by hand. A test that needs repeated runs
to be trusted must be rewritten or moved to the heavy tier. New tests fit the
short tier. Load, stress, and contention runs require a separate explicit selection,
a declared duration and resource budget, and no simultaneous ordinary checks or
user work on the same machine. The run owner stops every generator on exit.
Ordinary check evidence collected during a load run is invalid.

Provider callbacks, preparation, monitor starts, recovery, and request identities
are covered by the witnesses selected in the facade. Shared limits and filters
remain owned by their source rather than copied into this document. Rust corpora
use isolated SQLite state and recorded fixtures; they do not claim live provider
behavior. The full corpus recipe selects the expanded scenario matrices.

## Choose an evidence scope

Use the root [`justfile`](../justfile) for the three independent scopes:

1. **One hypothesis:** `just test <package> [case]`; use `just test-plan` first
   to print the exact native command and setup without executing it.
2. **One change:** `just check <package>` runs that package's local checks;
   `just check <package> --dry-run` prints the planned commands.
3. **Pre-PR gate:** run `just test-fast` and report its result. A focused
   hypothesis does not replace this gate. Every push to `main` also requires
   a local `just test-comprehensive` pass on the exact commit.

The repository checks add three focused commands: `just contracts-check` runs
the generated API and persisted-state checks in read-only mode; `just boundaries-check`
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
The runtime services, HTTP, process, store, and server crates each link one
`integration` test target. Their `tests/integration.rs` facades register the
behavior files under `tests/`; add each new file as a module there. Tooling checks
that every behavior file is registered exactly once. Select a module with
`cargo nextest run -p studio-runtime-services --test integration -E 'test(/^monitor_recovery::/)'`
or `cargo test -p studio-runtime-services --test integration monitor_recovery::`.
Nextest still runs each test in its own process. Library tests keep their existing
names and `--lib` selector.

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

## Client and browser checks

Install dependencies once with `pnpm install --frozen-lockfile`. Root package
scripts select client unit, browser, desktop, and provider bridge checks. Browser
checks build the renderer, use isolated state and a Rust HTTP fixture, and reap
fixture process groups after success, failure, or timeout. Browser scratch and
parallelism are owned by the [launcher](../workspaces/client/apps/web/scripts/run-browser-tests.mjs).
Explicit worker counts override its automatic plan. The wrapper clears `BROWSER`
before launching and retains JSON diagnostics for failures.

The release-binary tooling tests check commit and SHA-256 identity for native
packaging; see the [package cycle](../workspaces/client/apps/native/README.md#package-cycle).
The native WebView harness uses Node and preserves hidden-window checks. Its HTTP
fixture witness runs in the short tier. A passing unit fixture is not
installed-build evidence.

## Installed scenario smoke

`just smoke-installed` checks the main user scenarios on the app that is installed
on this Mac, against its live backend. Run it after an install. The
[script](../workspaces/tooling/apps/installed-smoke/smoke.mjs) owns the steps and
their time limits:

1. Refuse to run while the backend is younger than 60 s, while another backend of
   the bundle runs or waits for the runtime lock, or while a second app instance runs.
2. Read only: the running server is the installed binary (SHA-256), the served
   renderer is the installed build, and each provider has a ready login.
3. Create one test chat per provider, named `Studio smoke <run> <provider>`, in a
   new folder below the platform temporary directory. Each chat gets one short
   turn on the cheapest model by the backend's models.dev pricing artifact.
4. In the first chat: one message is delivered exactly once (an exact retry adds
   nothing), a monitor runs `true` and reports its exit, a sign-in card is
   submitted through the chat's browser socket, shows in the client state, and
   is declined within 5 s, and the client transcript pull lists every transcript
   item. The smoke declines its card by the server id even when the card does
   not show, and waits until the chat handles the decline.
5. Always, also after a failure or an interrupt: decline the run's open cards,
   stop and archive its chats, check that the archive shows in the client state,
   remove its project entry, and then delete its folder only by exact path and
   run marker. Then check that the owner's chats keep their names and archive
   state.

The smoke sends small real model requests and never changes the owner's chats.
`--no-model` runs only the read-only steps; `--provider` and `--model
provider=id` narrow the scenarios. A pending card raises an approval alert in a
build without the 10 s alert grace.

## Add or move a test

Keep Rust tests in the owning crate. Register integration modules in its
`tests/integration.rs` entry point. Add each short-tier witness as its own line
in the facade's matching `nextestAnyOf` list, preserving existing selectors.
Package manifests own `codexStudioChecks` mappings. Do not add another suite list.

## Mutation testing

Stryker changes selected client modules in a temporary sandbox and asks the
colocated Vitest tests to detect those changes. The explicit source selection,
concurrency and report paths are owned by
[the Stryker configuration](../workspaces/client/apps/web/stryker.config.mjs).

A surviving mutation is a candidate for a missing assertion, not an instruction
to add a test that merely repeats the implementation. Check whether the changed
behavior is observable and required. Record equivalent or deliberately excluded
mutations with a reason. Mutation reports cover only the selected sources; they
do not measure Rust, Electron or the whole application's reliability.

Reports remain local. The initial mutation command is diagnostic and has no
arbitrary score gate. A failing baseline test or runner error is a failed run,
not a successful mutation score. Choose any future merge threshold using a
measured baseline and an explicit scope.

## Continuous integration

The [workflow](../.github/workflows/ci.yml) runs Rust formatting, workspace Clippy,
doctests, a macOS all-target build, workspace nextest partitions, broad remaining
checks, pre-commit tests, dependency guards, and browser smoke. The Rust test build
produces one nextest archive and the renderer it embeds. Partitions download
commit- and digest-verified build output; a failed build fails its consumers.
The remaining-check command retains its historical `test-fast-rest-non-rust` name.
Python jobs, server shards, Python setup, and managed-environment installation
are removed. Tool versions and cache keys are owned by the workflow and its
composite actions. Heavy and load checks remain outside hosted CI.

## Live UI team acceptance

`workspaces/client/apps/web/scripts/verify-ui-team-scale.mjs` drives a live Studio
backend through headless Chromium: it creates a lead chat in the dialog, types one
prompt, and measures agent visibility, UI responsiveness, API latency, SQLite lock
counters and process load while the team runs. It deletes the chat and its agents
afterward and checks that no provider or tool process leaked (`--keep` skips this).
It is a manual, announced live run, outside every test tier. A small run:
`PLAYWRIGHT_BROWSERS_PATH=~/.cache/ms-playwright node
workspaces/client/apps/web/scripts/verify-ui-team-scale.mjs --workers 3`; `--help`
lists the options. Evidence goes to a directory outside the checkout.

Both scale verifiers keep their signal handlers active until teardown settles.
An interrupt requests cancellation; another interrupt during cleanup waits for
the same teardown. The force-exit policy and signal exit codes are owned by
[`scaleVerifierTeardown.mjs`](../workspaces/client/apps/web/scripts/scaleVerifierTeardown.mjs).
A forced exit first reports the evidence path and last known remaining agents;
that snapshot does not prove the backend has stopped them. A failed audit or
evidence write still permits the UI verifier's single conversation-delete
attempt, and in-flight audit reads settle before deletion. Offline signal and
stub-API coverage runs in the short tooling tier.

Before the cleanup, a strict audit (`scripts/uiTeamScaleAudit.mjs`) reads the
lead and worker transcripts, the lead's tool requests, the backend journal of the
run window (`journalctl --user --unit codex-studio`, `--journal-unit` changes it)
and diagnostics counters. The run fails on a failed or interrupted turn, a worker
with more than one turn or an error, a `child_result` delivered twice or never,
any input delivered twice, a tool call or request without a result, a tool request
applied while the model was told it failed, a lead final report made before the
last worker reply, a backend restart, an `ERROR` or panic journal line, a growing
error counter or a non-empty input queue. Everything else suspicious (failed lead
tool calls, placeholder answers, repeated final reports, long lead input waits,
start-order inversions, provider retries, other journal warnings) is listed under
`anomalies` in `summary.txt` and `evidence.json`.

## Interpret evidence and clean up

Report which checks ran, failed, timed out, or were excluded. A listing or dry run
proves selection only. HTTP/SQLite fixtures prove their supplied scenarios and
cannot prove live model behavior. The staged-content pre-commit hook checks
format and lint; it does not replace behavior tests.

Keep temporary-directory owners alive until stores and child processes close.
Use the platform temporary root and Playwright-managed screenshot output.
The [temporary-state guard](../workspaces/tooling/apps/repository-checks/test-temp-state.mjs)
rejects cleanup-disabling calls and new hard-coded temporary paths. Browser and
smoke fixtures clean up after success and failure; use their existing explicit
artifact-retention controls only for debugging.

Runtime Clippy (`just clippy-runtime`) rejects blocking calls on Tokio threads,
blocking filesystem work in HTTP handlers, guards across awaits, and oversized
inline futures; policy lives in [runtime/clippy.toml](../workspaces/runtime/clippy.toml).
