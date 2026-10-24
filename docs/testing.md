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

`just test-fast` runs the [`fast` nextest profile](../.config/nextest.toml), desktop
recovery and retained-helper boundaries, web API/type witnesses, tooling tests,
generated-contract and dependency guards, and the VM guest Python suite through its
[runner](../workspaces/runtime/apps/vm-guest/run-guest-suite.mjs). The facade uses three
fixed concurrent lanes: Rust preparation and tests, JavaScript checks, and the guest
suite. Cargo compilation stays in the Rust lane with four build jobs. WASM is
generated once; the renderer is built once before the native crate embeds it.
Type checks wait for WASM; web tests wait for the workspace build's debug
`examples/draft-server`, and the guest waits for its debug supervisor. The gate
passes both binary paths explicitly. These checks overlap the Rust build or test run. The
[WASM builder](../workspaces/client/apps/web/scripts/build-client-wasm.mjs) requires
the exact binding generator version pinned by the client-wasm crate; its error
gives the installation command. Cold builds are outside the warm short-tier
target. The short tier starts no browser or model provider.

`just test-comprehensive` runs the provider bridge, workspace Clippy, workspace
Rust nextest with the `ci` profile, web type and unit checks, tooling tests, the
shared desktop/helper and guest witnesses, and browser smoke. Both local tiers use
the same full workspace, Cargo features, development/test profiles and target
directory, including native coverage. A workspace `cargo build --tests --bins`
prepares the supervisor, contracts tool and HTTP fixture without package-only or
release builds; one workspace nextest invocation selects the tests. Consumers get
explicit debug binary paths. Browser smoke uses the prepared renderer and fixture,
without repeating WASM generation, main TypeScript checking or the renderer build.
Run it on the exact commit before every push to `main` and before packaging for
installation. Run `just test-fast` before every PR and merge and state its result
in the PR. Workers run focused witnesses; the lead runs these gates on the
integrated batch.

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

Package names and directories come from pnpm and Cargo manifests. Native manifests and runners remain the
owners of test discovery and selection. All facade commands work from a
repository subdirectory and save complete test logs outside the checkout in
`$XDG_CACHE_HOME/codex-studio/checks` (or `~/.cache/codex-studio/checks`). Set
`CODEX_STUDIO_CHECKS_CACHE` to choose another location. The facade removes run
directories older than 14 days at the start of each run; recent logs are kept
for future reproductions. Gates save one log per step and an aggregate `full.log`
and `summary.json` in declaration order, including per-step wall times and every
independent failure. A failed preparation skips its consumers; the first declared
failure is `failedStep`. Interrupts stop and reap only the run's owned process groups.
Gate test children and bridge Vitest workers discard ambient `CLAUDE*`, `STUDIO_*`
and `AI_AGENT` settings using `scrubAgentEnvironment` in `gate-config.mjs`;
fixtures and the gate supply their explicit binary paths afterward.

`just test-fast-rest` runs the comprehensive remainder without browser smoke.
`just test-fast-rest-non-rust` consumes prebuilt WASM, a debug supervisor and draft-server and
runs only the JavaScript and guest lanes: no Clippy, WASM builder or Rust
compilation. Hosted CI restores commit- and SHA-256-verified artifacts from the
workspace build job for this remainder and browser smoke. Standalone browser
smoke prepares itself; `just test-browser-smoke --prepared` consumes those inputs.

Fast Vitest witnesses live in each package's `vitest.fast.config.*`; each package
runs once with `--project fast`. File isolation and the existing worker bounds
remain enabled. The selected resource-event file includes the draft ordering
witness, so it needs no second `-t` invocation. Main, API-contract and client-helper
TypeScript programs remain separate. Their `--incremental` build information is
stored outside the checkout, with a separate cache file per program and a
checkout-specific gate cache directory.

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

## Upgrade canary

`just upgrade-canary --to <N+1 app> [--from <N app>]` installs build N+1 over
build N on an isolated clone with real processes. Run it before every live
install; `--from` defaults to the installed app. The
[script](../workspaces/tooling/apps/upgrade-canary/canary.mjs) owns the steps
and their time limits:

1. Copy build N into a new run folder below the platform temporary directory, with
   its own state folder, port, launchd stand-in and sandboxes.
2. Accounts: one Codex login of the live registry (`--codex-account`, default
   `default`), whose `auth.json` and `config.toml` the clone only links; the
   sandbox keeps them read-only. A Claude login of the canary itself, in
   `~/.config/codex-studio/canary-claude` (`--claude-config`, or
   `CANARY_CLAUDE_CONFIG_DIR`). Explicit Keychain mode uses a separate native profile
   described below. Without a file login the Claude row fails and the Codex part
   still runs (`--no-claude` skips it).
3. Start build N as a new Mac does: a first app start creates the state, build N's
   recovery helper registers the jobs with the launchd stand-in, and the real
   process supervisor and recovery job start in their sandboxes.
4. On the cheapest model: an idle chat with one finished turn, a running turn with
   a monitor that runs across the install and a message queued behind it, and a
   running turn (Claude when the canary has its login) that gets a steer.
5. Install build N+1 the way a paired-server install does: a staged copy and
   `pending-update.json` in the clone state, the real installer of build N with
   the clone's application folder and no window, and the recovery job started
   again from the swapped app.
6. Check that every provider handle kept its PID and generation, the monitor turn
   and the idle chat continue, the steer and the queued message are delivered
   exactly once, `/api/models` answers for every account, and no log line says
   "did not start" or "could not be verified". With a Claude login, its rate-limit
   windows show a subscription (five hours and a week).
7. Always: report the hosts that the providers asked the proxy for, the tokens per
   chat with their price at API rates, and that the live recovery configuration,
   plists and app did not change; stop the clone jobs and its server, and delete
   the run folder by its marker (`--keep` keeps it).

Isolation: every clone process runs in a sandbox that writes only below the run
folder, the canary Claude profile, and the user temporary folder. By default, it cannot reach the Keychain, the live state
sockets, `/Applications` or the live app, and reaches the network only through
the canary's proxy. The proxy refuses `auth.openai.com`, so the clone never
refreshes, and never rotates, a live Codex login: when the live access token has
expired, the turns fail. The installer never touches `/Applications` or the live
window, which has the same bundle id.

### Native Claude login for the upgrade canary

Current native Claude releases can refuse a file login when the sandbox denies Keychain access.
Do not change the Keychain settings or copy a live login to correct this refusal.
The canary supports two explicit sources:

- File mode: an existing private `.credentials.json` in `~/.config/codex-studio/canary-claude`, or a directory below it.
  This mode still denies Keychain access.
- Keychain mode: a fresh native profile from the existing Studio Add Claude dialog.
  The canary requires a reservation from before that profile exists.
  Use `--claude-keychain` with `--claude-reservation`, or set both environment variables in the example below.
  `CANARY_CLAUDE_KEYCHAIN=0`, or an unset variable, keeps Keychain access disabled.
  Profile existence never enables this mode.

Keychain mode requires user approval. It permits the supervisor, provider processes, and recovery server to contact Keychain services.
The installer still cannot contact these services. Other canary file and network restrictions remain unchanged.
This permission does not restrict Keychain access to one service label.
The native CLI selects its separate service from the exact private profile path.
Do not treat this mode as Keychain isolation from a provider process.

Create the reservation before you start Add Claude:

```sh
node workspaces/tooling/apps/upgrade-canary/claude-login.mjs --reserve \
  --record "$HOME/.config/codex-studio/canary-claude-keychain/studio-add.json"
```

1. Open Studio Settings, Accounts, Add account.
2. Select Claude.
3. Enter a separate test name, such as `Upgrade canary`.
4. Complete the existing sign-in flow with the intended subscription.
5. Wait for `Signed in as` and a ready Add receipt.
6. Record the Add receipt's `requestId` as `LOGIN_UUID`.
7. Delete only this fresh profile from the Studio account list.

Studio stores the Add receipt in `<live-state>/claude-logins/<LOGIN_UUID>.json`.
The account-list deletion preserves native credentials and history.
It excludes this profile from new chats and the account auth watcher.
Studio can start an SDK process for account metadata before any chat exists.
A closed metadata handle does not prove model input.
Admission still refuses every chat, execution, usage, or transfer footprint and every active matching handle.

Bind the exact fresh profile after its metadata process exits:

```sh
node workspaces/tooling/apps/upgrade-canary/claude-login.mjs --bind \
  --login-id LOGIN_UUID \
  --record "$HOME/.config/codex-studio/canary-claude-keychain/studio-add.json"
```

If Studio uses another state directory, pass its exact path with `--live-state` to both commands.
The helper reads only application metadata.
It does not start authentication, read credentials, copy a profile, or change Keychain settings.
A repeated reservation keeps its original baseline.
An incomplete baseline or changed identity refuses admission.
The helper checks indexed metadata and bounded new rows, not historical transcript bodies.

Run the upgrade check with the explicit mode:

```sh
CANARY_CLAUDE_KEYCHAIN=1 \
CANARY_CLAUDE_RESERVATION="$HOME/.config/codex-studio/canary-claude-keychain/studio-add.json" \
  just upgrade-canary --to /absolute/path/to/Build.app
```

Use both environment variables for `just deliver-server` when it runs the upgrade canary.
The helper, registry, and provider environment use the same exact path for `CLAUDE_CONFIG_DIR` and `CLAUDE_SECURESTORAGE_CONFIG_DIR`.
Do not copy, rename, or alias the profile. Native Keychain service identity depends on this path text.
The reservation binds the state directory, Add receipt, profile, account, and native executable.
The canary checks this evidence again before it starts.
Only this exact profile can receive writes inside the live state directory.
Existing profiles, symbolic links, another owner, or nonprivate profile directories refuse admission.
File-mode `--claude-config` paths must stay inside the separate file-mode namespace.
The runtime still requires native account email and subscription proof before it submits model input.

## Add or move a test

Keep Rust tests in the owning crate. Register integration modules in its
`tests/integration.rs` entry point. Add each short-tier witness as its own clause
in the `fast` profile's scoped union in [nextest.toml](../.config/nextest.toml),
preserving its package and binary-kind scope and adding a purpose comment. The
old facade `nextestAnyOf` lists are removed. Use nextest's native `list --profile
fast` to inspect selection; `ci` is the comprehensive superset. The migration's
[recorded parity evidence](../workspaces/tooling/apps/repository-checks/fixtures/gate-selection-parity.json)
preserves the original 484-ID migration evidence and compares the 526 witnesses
on the integration baseline, with the explicitly added schema-preparation witness
bringing the current selection to 527. Every Linux clause matches tests, and the
selection is a subset of comprehensive.
Fast Vitest includes belong in the package's `vitest.fast.config.*`, never a
second facade or package-script file list. Package manifests own
`codexStudioChecks` mappings.

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
