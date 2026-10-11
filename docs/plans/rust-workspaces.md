# Rust migration and modular workspaces

## Change contract

This specification is for maintainers and agents migrating Codex Studio. It
owns migration requirements and acceptance criteria, not current runtime
configuration. Package manifests will own dependencies; schemas will own wire
shapes; package tests will own executable checks. The tracking issue will own
completion status and evidence links.

Preserve state-directory identity, permissions, exact request identities,
uncertain outcomes, active user work, and the existing client protocols. No
second backend may open an occupied state directory. A package extraction must
include its callers, packaging, tests, and documentation.

Status: **updated 2026-10-11**. [Issue #37](https://github.com/ifdotpy/codex-studio/issues/37)
owns completion status and evidence links; this specification owns requirements,
decisions, and acceptance criteria. Implementation statements for completed work
are verified against merged changes through `316c73b8`.

Sequencing: PR 1 relocated the source tree and migrated npm to pnpm. M-01 added
the Just facade, Cargo workspace, diagnostics CLI, and cargo-nextest runner; M-04
connected the Rust operation lifecycle core to Python through the PyO3 module.
Requirement, decision, and acceptance IDs remain stable; issue #37 owns item
status.

### Source path mapping

The relocation keeps Python module names and moves source roots as follows:

| Former path                           | Current path                                                          |
| ------------------------------------- | --------------------------------------------------------------------- |
| `scripts/` implementation             | `workspaces/runtime/apps/server/src/`                                 |
| `scripts/claude_bridge/`              | `workspaces/providers/apps/claude-bridge/`                            |
| `tests/` server and runtime contracts | `workspaces/runtime/apps/server/tests/`                               |
| `tests/client/`                       | `workspaces/client/apps/web/tests/`                                   |
| `web/`                                | `workspaces/client/apps/web/`                                         |
| `desktop/`                            | `workspaces/client/apps/desktop/`                                     |
| `vm/guest/`                           | `workspaces/runtime/apps/vm-guest/`                                   |
| `prompts/`                            | `workspaces/runtime/apps/server/prompts/`                             |
| `scripts/check-code.mjs`              | `workspaces/tooling/apps/repository-checks/check-code.mjs`            |
| `tests/pre-commit-hook.mjs`           | `workspaces/tooling/apps/repository-checks/tests/pre-commit-hook.mjs` |

## Outcome and scope

**G-01.** Incrementally replace the owned production Python backend with Rust
while keeping Studio usable, dividing the repository into domain workspaces
with independently testable apps and packages, generated interface types, and
matching concise documentation.

The final Rust scope is approved decision D-01.
Implementation is delivered through small reviewed changes with observable
callers, never an unused parallel implementation counted as completion.

Supplied requirements:

- **R-01.** Gradual Rust migration with preservation of existing user flows.
- **R-02.** `workspaces/<domain>/{apps,packages}` source hierarchy; apps are
  runnable end products, packages are reusable libraries with explicit APIs.
- **R-03.** One discoverable command interface for development and checks.
- **R-04.** Separate modules support concurrent development by multiple agents.
- **R-05.** New behavior tests and independently executable module tests.
- **R-06.** Generate interface types from an authoritative contract.
- **R-07.** Documentation follows the source hierarchy and stays concise.
- **R-08.** A GitHub tracking issue has stable, individually checkable work items.

Existing constraints remain: server ownership of SQLite/orchestration, renderer
access through HTTP, native privileges in the isolated Electron main process,
no interruption of active work for source updates, no license change, and no
publication of packages or releases by this specification.

Non-goals: rewriting React, replacing Electron with Tauri, changing providers'
native SDKs, changing user data identities, introducing a hosted build service,
rewriting every test in Rust, or fixing unrelated historical defects. Defects
that invalidate a migrated flow's acceptance cannot be carried past its gate.

## Facts, assumptions, and decisions

Verified from source:

- `workspaces/runtime/apps/server/src/codex_runtime.py` coordinates mutable records, SQLite transactions,
  native provider processes, delivery, and recovery. Its migration is behavioral.
- `workspaces/runtime/apps/server/src/studio_api/` owns strict HTTP models and the offline OpenAPI generator;
  `workspaces/client/apps/web/src/generated/` contains generated TypeScript and schema identity.
- Root, web, desktop, and Claude bridge previously had separate npm manifests
  and lockfiles. Packaging and launchers contain explicit relative source paths.
- `workspaces/runtime/apps/server/tests/server/run.py` isolates suite directories but holds a per-user lock over
  an entire run. Parallel suites do not imply independent concurrent runners.
- `workspaces/runtime/apps/server/tests/KNOWN-FAILURES.md` records unresolved failures and environment limits.
  The current suite is not a uniformly green migration oracle.
- Supervisor update evaluation already belongs to
  [issue #9](https://github.com/ifdotpy/codex-studio/issues/9). Its proposal is
  measurement-only; production restart safety has not been established here.
- `.github/workflows/ci.yml` runs the hosted checks on pull requests and pushes
  to `main`; the audited offline server automation suites are included.

| ID   | Approved decision                                                                                                                                                                                                                                                              | Status / consequence                                                                                              |
| ---- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ----------------------------------------------------------------------------------------------------------------- |
| D-01 | Eventually migrate the owned production Python backend, supervisor, and CLI to Rust; retain React/Electron and the required Node SDK bridge; Python may remain in development/testing.                                                                                         | Approved. Core-only would change the completion criteria.                                                         |
| D-02 | Domain source workspaces share one root Cargo workspace and one root pnpm workspace, with one lockfile per ecosystem.                                                                                                                                                          | Approved. Domain independence means independent checks/ownership, not independent release universes.              |
| D-03 | Permit a bounded UI reconnect during supervisor-backed backend replacement, preserving active work; approve the outage bound after isolated measurement and before rollout.                                                                                                    | Approved. Not permission to restart the user's backend.                                                           |
| D-04 | Use pinned Just as a thin command facade; keep Cargo, pnpm, and the existing Python runner as execution owners.                                                                                                                                                                | Implemented in M-01; no new build scheduler or cloud cache.                                                       |
| D-05 | Keep Pydantic/OpenAPI authoritative initially; transfer each migrated HTTP domain's schema authority to Rust with compatibility checks and one composed API schema.                                                                                                            | Approved; no simultaneous handwritten authorities.                                                                |
| D-06 | Preserve current Linux, macOS, WSL, and Windows-specific behavior where currently supported; report the actual checked platform matrix.                                                                                                                                        | Approved compatibility requirement; this does not promise new platform support.                                   |
| D-07 | During the first orchestration-core Rust slice, call `studio-operations` in-process through the PyO3 extension module `studio_operations_native`, using protocol version 2; Python retains persistence and effects. Retire the binding as ownership moves to the Rust backend. | Revised and approved 2026-10-10; no public listener, subprocess, shadow execution, or duplicated lifecycle rules. |

**P-01: update-contract opportunity.** D-03 changes the live-update contract.
Issue #9 records a historical inventory of 27 patch modules and 17 related test
files, not current measurements. Expected benefit: one implementation of a fix
and an explicit executable generation, instead of maintaining in-process patches.
Unknowns: reconnect duration, journal recovery under load, retained process
identity, and supervisor upgrade behavior. Keep live patches until the isolated
measurement and compatibility gates pass. Supervisor replacement itself must
wait for a safe idle boundary unless an independently verified handoff exists.

**P-02: tool choice.** Prefer Just plus native workspace tools over adding a
second dependency graph/cache platform. Just runs recipes rather than replacing
build tools. The checked upstream release is 1.58.0 (2026-08-03), CC0-1.0;
upstream was active on 2026-10-08. It supports Linux/macOS/Windows; recipe shell
portability still needs validation. It is a development dependency, not a
runtime service, and requires no hosted account or project-data upload.
Prebuilt binaries or a locked source install must be pinned and verified;
source compilation adds upstream Cargo dependencies (including OS-specific
libraries). No transitive dependency/security audit or target-platform install
has been performed here. Retain native commands so removing Just does not
change build semantics. Do not change the repository license. Exact tooling
pins and any notices become owned by the tooling manifest in M-01.

Sources: [Just manual](https://just.systems/man/en/),
[release](https://github.com/casey/just/releases/tag/1.58.0),
[manifest](https://github.com/casey/just/blob/1.58.0/Cargo.toml),
[Cargo workspaces](https://doc.rust-lang.org/cargo/reference/workspaces.html),
[npm workspaces](https://docs.npmjs.com/cli/v11/using-npm/workspaces/).

**P-03: JavaScript package-manager decision.** D-02 selects pnpm for the root
workspace and lockfile. The expected benefit is better visibility of
undeclared dependencies, local-only `workspace:` links, package/dependent
selection, and a recorded dependency patch replacing manual RxDB mutation.
These support independent LLM iterations; no installation-speed improvement has
been measured here. PR 1 owns the migration evidence and any dependency-version
differences.

Upstream pnpm is active; release v12.10.1 was published 2026-10-06, with an MIT
license. The documented supported platforms include Linux, macOS, and Windows;
pnpm installation requires Node 22.13+, compatible with this repository's
stated minimum. Pin and verify the chosen tool artifact and lockfile; review
allowed dependency build scripts. This adds a development/package-build tool,
not a production service or hosted account. Existing registry requests continue;
no project source upload is needed. Transitive security/license checks and actual
Electron/bridge installation compatibility remain unverified until M-01.

Migration must replace hardcoded `workspaces/client/apps/web/node_modules` tool paths, materialize
self-contained bridge dependencies for packaging, and turn the RxDB workaround
into a pinned patch without losing its behavior tests. Do not mutate pnpm's
linked dependency files in place. Do not enable broad public hoisting merely to
hide undeclared dependencies. Roll back by reverting the isolated package-manager
change and restoring the saved npm locks/install recipes; pnpm patch metadata and
`workspace:` references need explicit conversion if reverting later.

Sources: [workspace guarantees](https://pnpm.io/workspaces),
[filtering](https://pnpm.io/filtering),
[portable deployment](https://pnpm.io/cli/deploy),
[dependency patches](https://pnpm.io/cli/patch),
[installation/platforms](https://pnpm.io/installation),
[release](https://github.com/pnpm/pnpm/releases/tag/v12.10.1).

Alternative directory organizations were considered: flat root `apps/` and
`packages/` minimize path depth; domain-first `workspaces/<domain>/{apps,packages}`
keeps ownership/docs together; language-first `rust/`, `python/`, `typescript/`
fits toolchain ownership but moves a feature's home when its language changes;
product-first trees favor independent releases but can duplicate shared runtime
contracts. D-02 retains domain-first directories for this migration. Directory
layout does not itself enforce isolation: manifests, public APIs, fixtures, and
focused checks do. All domain directories share the native workspace roots.

## Target hierarchy and dependency rules

The target hierarchy below matches the layout created by PR 1. Empty packages
are not retained; extract one only with an actual caller and a focused test.

```text
package.json / pnpm-workspace.yaml / pnpm-lock.yaml
README.md / AGENTS.md
docs/                         cross-domain explanations and migration decisions
workspaces/
  runtime/
    README.md
    apps/server/              Python backend, supervisor, and CLI
      src/                    flat Python import root
      tests/                  server and runtime contracts
      prompts/                runtime prompts
    apps/vm-guest/            Linux guest service
  providers/
    README.md
    apps/claude-bridge/       Node SDK process
  client/
    README.md
    apps/web/                 React renderer
    apps/desktop/             Electron host
  tooling/
    README.md
    apps/repository-checks/   staged-content and repository checks
```

The root Cargo workspace manifests and pinned Just facade now sit alongside
this relocated layout; M-01 delivered them in PR #40.

- **R-09.** Apps assemble packages and own startup/shutdown, deployment, and
  user entry points. Packages never import apps. Cross-package imports use
  declared public interfaces; no sibling private-source imports.
- **R-10.** The production dependency graph is acyclic. `runtime/domain` owns
  domain IDs, state transitions, and required ports without provider, HTTP,
  SQLite, UI, or process implementations. Adapters implement those ports;
  server assembly selects implementations. Transport DTOs are not domain state.
- **R-11.** Each domain has a named integration owner; each active package and
  shared file has one edit owner. Root manifests, lockfiles, wire contracts,
  migrations, and shared fixtures have a designated integration owner.
- **R-12.** Tests are colocated with their packages. Cross-package scenarios
  belong to the consuming app; repository-wide lifecycle checks live under
  tooling. Test-only dependencies are explicit and never leak into shipping apps.
- **R-13.** Retained Python code moves in bounded component extractions when a
  module moves to Rust or gains a second consumer. Do not extract Python
  packages solely to create boundaries; no massive file move combined with
  behavioral rewriting, permanent `legacy` dumping ground, or duplicate source.
  Type only Python modules that will remain Python. (M-03.7 decision, issue #37;
  typed Rust boundary, PR #63.)

Source workspace is a repository term, distinct from agents' runtime worktrees
and image workspaces. Moving source must not change their state paths or identity.

## Behavior, state, and failure contracts

Affected actors: desktop/mobile users, lead and worker agents, provider sessions,
CLI operators, package maintainers, and CI. Affected data: SQLite records,
operation receipts, event journals, histories, drafts/sync cursors, configuration,
workspace handles, and process generations. Lifecycle coverage includes startup,
request admission, execution, cancellation, reconnect, recovery, update, rollback,
and shutdown.

- **R-14.** Same scoped request ID and same canonical content returns the saved
  operation or its unresolved state; different content under the same ID fails
  before another effect. A missing receipt alone does not prove non-execution.
- **R-15.** Model `not_applied`, `applied`, and `unknown` explicitly. A timeout,
  lost connection, invalid post-execution response, or crash never converts
  `unknown` into `not_applied`. Recover by evidence, never by a fresh ID retry.
- **R-16.** There is one authoritative writer/executor per operation domain.
  Rust shadow evaluation cannot write SQLite, send provider requests, launch
  commands, charge credits, acknowledge events, or contact users. During partial
  migration, the existing database owner commits Rust decisions through a
  versioned transaction interface; the current Rust core is called in-process
  through the PyO3 binding and never opens the production store. Moving the
  physical database owner is an explicit exclusive handoff. Remaining Python services then use that owner
  through its interface, with no direct database access.
- **R-17.** State transition, durable operation evidence, and outgoing-event
  intent are committed atomically where they share a database. Effects outside
  SQLite use stable identities and reconciliation. No end-to-end exactly-once
  claim is made for providers that cannot prove it.
- **R-18.** Events carry identity and generation. Duplicate/out-of-order delivery,
  old process generations, and old account/thread epochs cannot regress new
  state. Cancellation before dispatch differs from cancellation after dispatch;
  the latter cannot erase an uncertain outcome.
- **R-19.** No blocking provider call or long filesystem operation occurs while
  holding a global runtime lock or write transaction. Queues are bounded; overflow
  creates backpressure or an explicit durable failure, never silent loss.
- **R-20.** Validate authentication, origin, workspace, team/role, and permissions
  before effects. Preserve native provider permissions and the Electron boundary.
  Rust types do not replace trust checks on incoming data.
- **R-21.** Startup, migration, update, and rollback preserve the existing state
  directory and acquire exclusive ownership. Backward-compatible schema changes
  support a tested rollback; otherwise the stage blocks rollout until a safe
  forward-recovery or compatibility path exists. Never restore an old database
  over acknowledged new work to claim rollback success. There is one installation:
  code used only to read or migrate an old format is removed after that
  installation is shown to have completed the migration. Startup visibly refuses
  an incomplete state. The payload migration has not completed on the installation
  and its migration code remains. (#61, #64.)
- **R-22.** Preserve HTTP methods/status/error semantics, null versus absent,
  schema hash handshake, SSE ordering/reconnect, sync cursors, CLI behavior, and
  native protocol framing. Unknown extensible provider fields remain supported;
  stricter known-field validation requires an explicit compatibility decision.
- **R-23.** Errors in persistence, diagnostic logging, or subprocess transport
  cannot silently kill scheduling. Health reports distinguish live process from
  healthy scheduling, delivery, and persistence.

## Contract generation and transitional interfaces

**R-24.** Every published schema/type has one authority and an explicit owner.
Initially keep `workspaces/runtime/apps/server/src/studio_api/schema.py` and `generate_types.py` as the
source pipeline. Generate without opening user state or starting providers.

For the first Rust slice, the private PyO3 module `studio_operations_native`
exposes `transition_record(...)` and `PROTOCOL_VERSION` (currently 2). Serde JSON
carries a typed projection of the persisted record to Rust in process; it is an
FFI data encoding, not a separate wire protocol. (#46, #63.) Python calls the
core while holding the existing lock and transaction, and persists the returned
decision there. The backend requires the built module and fails startup visibly
if it is missing or its protocol version mismatches. Build it with
`cargo build --release -p studio-operations-python`, then install the built
platform library with `workspaces/runtime/apps/server/src/install-cli.py`;
installation and desktop packaging do not build implicitly.
There is no subprocess, decision comparison, shadow mode, or second Python
lifecycle implementation. Python continues to own authorization, SQLite,
record/payload shape, timing, and provider effects until a later ownership stage.

For each HTTP domain migration, one change transfers schema ownership and route
ownership together. Rust-owned DTOs produce that domain's OpenAPI fragment;
remaining fragments come from Python. A deterministic composition step rejects
duplicate routes/components and generates the existing TypeScript API client
contract and schema hash. Shared DTOs keep one owner; namespace or reference
shared definitions rather than copying them. Generated references are committed
when required by the existing build, and drift checking must not rewrite files.

**R-25.** Wire compatibility tests cover integer ranges used by JavaScript,
timestamps, enum values, finite numbers, null/missing distinctions, aliases,
error envelopes, binary/streaming routes, and strict-versus-extensible objects.
Passing TypeScript or Rust compilation alone is insufficient evidence.

Do not select new HTTP/schema generator libraries solely in this plan. M-03
must validate a pinned candidate against actual existing contracts and dependency
policy. A candidate that changes D-05 or wire semantics needs a revised decision;
a replacement preserving them is a bounded implementation choice.

## Commands, independent tests, and configuration

**R-26.** Just is a thin entry point with help/listing and stable package names.
Proposed commands (not available yet):

```text
just check <package>             local lint, type, unit, and boundary checks
just test <package> [case]       one case or a deterministic package suite
just test-plan <package> [case]  explain target/command/setup without executing
just integration <app>           explicitly selected cross-package scenarios
just contracts-check            generation drift and protocol compatibility
just boundaries-check           forbidden dependencies and undeclared imports
just docs-check                 links, required contracts, and discovery
just check-affected <base>       relevant checks with explained dependency scope
just check-all                  full non-live matrix, with explicit skip report
```

Use Cargo/package manifests to derive dependencies. Transitional Python membership
must have one declared owner, not a second handwritten test inventory. For an
unknown file or incomplete dependency map, affected checks conservatively widen
and explain why; a focused test never silently turns into check-all. Full-app
execution remains an explicit integration/release selection. Unknown package names and unsupported suites fail rather than succeed with zero
tests. Native commands remain usable; runners propagate exit codes and signals.

**R-27.** A package check runs from any working directory after pinned dependencies
are installed; builds only its required dependencies; does not build the UI or
start Studio unless that is the selected app's contract. Fixture state is outside
the checkout and cannot access real accounts or runtime state.

**R-28.** Independent agents can run two unrelated package suites concurrently
with distinct state, ports/sockets, output, caches that contain mutable state,
and owned child processes. Rust build outputs may use per-agent target roots to
avoid a global build lock. Shared immutable downloads/caches are allowed. Global
heavy-suite admission remains resource-limited until its safety replacement is
proven; removing the existing per-user lock alone is not an acceptable change.

**R-29.** Unit tests use controllable clocks/IDs and exercise public behavior.
Component tests use actual local adapters/SQLite as appropriate. Protocol replay,
crash injection, property tests, and selected mutation tests target the invariants
above. No coverage percentage is a substitute for observable correctness.

**R-30.** CI runs the same commands with locked dependencies on declared target
platforms. Default checks need no credentials, paid requests, live backend,
foreground windows, or OS input. Installed-provider and live checks are explicit
separate evidence classes. Mandatory missing dependencies fail their job; optional
platform/provider checks report named skips and cannot satisfy a release gate.
Untrusted PR code gets no secrets or privileged self-hosted runner access.
The server runner owns one `AUTOMATED_SUITES` list: 26 audited offline suites run
in the default server run or the hosted CI step. Load-class suites are excluded
from all automation. (#74, #80.)

Configuration owners: root Rust toolchain/Cargo policy, root pnpm dependency policy,
root Just recipes delegating to domain recipes, package test manifests, and
existing runtime configuration (unchanged until its owning slice moves). No test
or formatting command installs tools or changes runtime settings implicitly.

## Fast hypothesis testing for agents

This is an explicit user requirement, not an optional runner optimization.

**R-35.** The default hypothesis loop selects one production package and a named
test, scenario, or behavior. It does not start the backend, browser, Electron,
provider, or unrelated tests unless that selected behavior requires them.
Compiling the selected package and its declared dependencies is allowed; building
unrelated applications is not. Keep fixture setup small and local. A pure domain
test takes typed inputs and checks transitions/effect intents without launching
Python, SQLite, a provider, or the application.

**R-36.** A plan command reports selection, native command, required setup,
reason for each dependency/check, and any execution-cost category before running.
It does not install, build, run tests, or start services. Unknown targets, an
unmatched requested case, and zero executed tests fail visibly. Native runners
remain the authority for test discovery and selection; the facade must not
maintain a second list of test names.

**R-37.** Separate three evidence scopes: (a) one hypothesis, (b) affected package
and interface/consumer contracts, (c) pre-PR/merge integration. A successful
hypothesis is not claimed as application verification. Do not run (c) automatically
inside (a). Before opening a PR against main or merging into main, run the server
and client suites required by AGENTS.md/docs/testing.md and report their results.
Reuse valid evidence for the same inputs; changed source, contracts, environment,
or unresolved failures invalidate the relevant evidence.

| Change under investigation             | Smallest useful verification                                                                       |
| -------------------------------------- | -------------------------------------------------------------------------------------------------- |
| Pure transition or calculation         | Named unit/property case in the owning package; no I/O                                             |
| Persistence or receipt transaction     | Focused storage component scenario against disposable SQLite; no UI/provider                       |
| Public API or generated schema         | Owning boundary cases plus affected consumer contract/type checks                                  |
| Renderer component                     | Colocated unit/component test; selected browser scenario only if DOM/browser behavior is essential |
| Provider transport or process recovery | Named protocol/process scenario with owned fake/replay peers; no live account                      |
| Cross-component user flow              | One app integration scenario and the required dependencies                                         |
| Release or PR/merge boundary           | Required server/client suites and applicable platform/package checks                               |

**R-38.** Default output is concise: package/case, pass/fail/skip counts, duration,
failed assertion location, and exact reproduction command. Preserve full logs,
seed, traces, and structured results in external artifact files and link them;
provide a machine-readable result option for LLM tools. Report omitted scopes;
never bury an error in a truncated successful summary. A test timeout cleans up
only its owned processes and returns a failed/timeout result.

**R-39.** M-03 measures warm and cold setup separately for representative pure,
storage, and browser checks. Record time-to-first-result, executed-test count,
unrelated processes/builds, and output size on a stated machine/load. Pure
hypothesis checks target a seconds-scale warm loop; the owning package records
its measured budget and a reason for exceptions before acceptance. Performance
claims require measurements, not folder structure or changing npm to pnpm.

M-03.6 recorded representative cold and warm timings in
[`docs/verification/2026-10-11-check-timings.md`](../verification/2026-10-11-check-timings.md)
(PR #78). Performance work measured on the running backend found Python
structure as the bottleneck and fixed it in place. Each measurement was
single-threaded and was not a load run: transcript search on generated history
of 250,000 items held the runtime lock for 2.55 s of a 2.63 s scan, then 0 s of
a 2.07 s scan (#58); command-monitor output on a disposable state with 64 active
monitors and 100 chunks of 256 bytes each fell from 7.9 ms to 1.54 ms of lock
time per chunk (#67); sync maintenance on one disposable database with 256
agents, 200,000 items, and 256 monitors fell from p50/p99 3.59/3.64 ms to
0.08/0.13 ms (#69); default `Runtime.db()` entry on a disposable state with 201
agents and 200,000 items fell from p50/p95 1.133/1.152 ms to 0.618/0.624 ms
(#71); and native release-tick lock time on a disposable state with 301 agents,
200,001 events and items, and 40,000 requests fell from median/worst 6.03/7.16
ms to 2.77/2.79 ms (#76). The live 4.07 s sync-pull and 1,466 ms release-tick
lock outliers were not reproduced or claimed fixed.

## Documentation and multi-agent delivery

**R-31.** Root README is a short entry portal. Workspace README explains purpose,
apps/packages, dependency direction, and navigation. Package/app README starts
with a Change Contract: responsibility, public interface, forbidden dependencies,
invariants, configuration owner, and focused check. Longer how-to or rationale
pages live beside their owner; cross-domain decisions stay in `docs/`.

**R-32.** Avoid copied command inventories, schemas, numeric defaults, and parallel
status ledgers. Link to authoritative manifests and generated reference. Local
AGENTS files contain only applicable differences; root safety rules remain in
force. A new agent can discover its API and check through root → workspace →
package, without reading unrelated domains.

**R-34.** Rust domain code uses named IDs and closed state/error enums, with
`unsafe` forbidden in the domain layer. Persisted and external bytes are
validated before entering that layer. Necessary platform-level `unsafe` is
contained in reviewed adapters with documented safety invariants. Typed error
codes replace internal decisions based on human-readable message text, while
public error compatibility is preserved.

**R-33.** Assign complete module outcomes, including callers, failure tests,
packaging, and docs. Lockfile/schema/migration changes go through the designated
owner. Interface changes update affected consumers before acceptance. Review is
required before integration; a worker's successful isolated test is not a merged
or live result. No package is split merely to create more agent tasks.

## Acceptance criteria

| ID    | Observable pass condition                                                                                                                                                                                                                                                                     |
| ----- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| AC-01 | A fresh checkout with pinned tools discovers all apps/packages and their commands; old supported entry points still work during relocation.                                                                                                                                                   |
| AC-02 | Every migrated package has a public API, real caller, named owner, colocated tests, and reachable concise README; forbidden imports/cycles fail a negative check.                                                                                                                             |
| AC-03 | Two unrelated package checks overlap in time in separate agent checkouts, do not wait for the legacy whole-run lock, and leave no foreign processes/state changes. Resource admission remains bounded.                                                                                        |
| AC-04 | Generation is deterministic without runtime credentials/state; changing an authoritative field makes drift checks fail until regeneration; duplicate schema ownership fails.                                                                                                                  |
| AC-05 | Python and Rust run the same recorded/synthetic transition cases; all differences are explained and approved, not blindly copied from Python.                                                                                                                                                 |
| AC-06 | Fault injection before reservation, after commit, before/after dispatch, after effect but before receipt, and before/after acknowledgment preserves IDs and never blindly replays uncertain effects.                                                                                          |
| AC-07 | Same ID/same content, same ID/different content, duplicate events, stale epochs, cancellation races, full queues, disk-full, failed logging, and process death have asserted outcomes.                                                                                                        |
| AC-08 | HTTP/CLI/schema-hash/SSE/sync and authorization compatibility suites pass for migrated flows; malformed requests fail before effects and response failures preserve uncertainty.                                                                                                              |
| AC-09 | Installed/package artifacts launch without relying on checkout-relative paths; source inventory/build identity includes the actual shipped components; existing state identity, bundle ID, signing identity, browser profile, installed CLI links, and configuration semantics are unchanged. |
| AC-10 | Under isolated supervisor update, native process identities and active work survive, journals reconcile, and UI reconnect meets the separately approved bound on relevant platforms. Updating the supervisor itself has a proven safe boundary.                                               |
| AC-11 | Rollback after new operations preserves their receipts and prevents replay; incompatible state blocks downgrade visibly.                                                                                                                                                                      |
| AC-12 | Reports name source revision, commands, platform, fixture/live class, failures, skips, and known-failure disposition; no hidden retries or blanket xfail additions.                                                                                                                           |
| AC-13 | Final owned production backend/CLI/supervisor run without Python; remaining Python is documented dev/test tooling. Depends on approved D-01.                                                                                                                                                  |
| AC-14 | A tracking checkbox has a linked reviewed change plus relevant acceptance evidence; creation of a folder, stub, or mock alone cannot complete it.                                                                                                                                             |

Additional acceptance criteria for v1.1:

- **AC-15.** A named pure-transition test runs from a fresh agent checkout after
  dependencies are prepared; it executes the intended case and launches no
  Python/backend/browser/provider or unrelated test suite. An unmatched case is
  a failure. A negative sentinel verifies forbidden processes are not started.
- **AC-16.** A plan and actual focused run agree on target and required setup.
  An interface change selects the affected consumer checks; a whole-app suite
  is not silently substituted. The pre-PR/merge server/client gate remains
  explicit and separately reported.
- **AC-17.** Representative checks publish cold/warm timing and selected scope,
  concise human output, machine-readable results, a full artifact path and exact
  reproduction command. Concurrent independent agent runs retain AC-03.

## Delivery stages and dependencies

Stage IDs are stable. The GitHub issue contains granular checkboxes under these
stages; this document deliberately does not duplicate completion status.

| Stage | Deliverable and gate                                                                                                                                                                                                                                                                                               | Dependencies                                        |
| ----- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | --------------------------------------------------- |
| M-00  | Approve the exact specification, record platform/failure baseline and owners, and publish the tracking issue.                                                                                                                                                                                                      | D-01–D-07                                           |
| M-02  | Relocate the source tree and migrate npm to pnpm; fix packaging, installs, inventories, imports, test discovery, and docs together. Root `scripts/` compatibility launchers were removed (M-02.11, #57).                                                                                                           | M-00                                                |
| M-01  | Add pinned Just and root Cargo workspace policies with a real first Rust member: the read-only diagnostics CLI. Preserve its installed command, flags, output, and HTTP caller; validate packaging. Wrap current checks; introduce CI discovery and dependency policy. Rust tests use cargo-nextest (M-01.6, #59). | M-02                                                |
| M-03  | Independently runnable package tests, deterministic contract generation, boundary checks, migration fixtures, and measured check timings (M-03.6, #78). M-03.7 Python package extraction was dropped; extract a module only when it moves to Rust or gets a second consumer.                                       | M-01; proceed incrementally alongside M-02          |
| M-04  | Rust operation-lifecycle core called in-process by the required `studio_operations_native` PyO3 binding (protocol 2); connect the tool-request caller and remove its Python lifecycle decisions. Python keeps database writes and provider effects. Backend setup requires building and installing the module.     | Relevant M-02/M-03 slices                           |
| M-05  | Transfer operation decisions/execution and recovery ownership for one complete slice using the single database owner. Move physical storage ownership only with an exclusive handoff and access through its transaction interface; migrate remaining operation domains individually.                               | M-04; AC-06–AC-08, AC-11                            |
| M-06  | Isolated supervisor update measurements and decision under #9; implement safe backend replacement only after its gate.                                                                                                                                                                                             | M-00 baseline; research may run alongside M-01–M-05 |
| M-07  | Migrate scheduler, process control, provider adapters, workspace/move/federation operations, and remaining stateful services, one caller-complete slice at a time.                                                                                                                                                 | M-05 and relevant M-06 lifecycle evidence           |
| M-08  | Transfer HTTP domain authorities and CLI assembly, finish Rust production packaging, remove obsolete Python owners and transitional bridges.                                                                                                                                                                       | Corresponding M-07 slices and M-03 contracts        |
| M-09  | Validate target platform/install/update/rollback matrix; retire superseded code and docs only with replacement evidence; meet AC-13.                                                                                                                                                                               | M-06–M-08                                           |

Ordering is by ownership and safety, not percentage of translated lines. An
unchanged historical failure may be recorded outside the slice; a failure in
that slice's invariant or rollout path blocks that slice. No big-bang switch,
dual execution of real commands, or permanent double-write database mode.

The maintainer decided on 2026-10-11 that Rust migration continues step by step,
starting with M-05. Standing requirements for every stage are to apply the
opportunity check; continue visible Python improvements alongside the migration;
avoid duplicated code or declarations; use the strictest typing and trait-based
seams; keep Rust code as small as possible and prefer popular, proven crates to
new implementations; and state the maintenance, license, and transitive
dependencies for each proposed crate. Shared types have one authority, with
Python-side types generated from Rust rather than declared twice. A final pass
removes Python integration layers once all migrated functionality has moved.
The separate M-05 slice design is in progress and has not been approved; this
plan does not specify it.
[Maintainer decision in issue #37](https://github.com/ifdotpy/codex-studio/issues/37#issuecomment-6108525254).

## Tracking and approval

Issue #37 owns completion status and evidence links. Issue items use `M-xx.y`
IDs and reference `R-xx` / `AC-xx`. Keep IDs when rewording items. If a task is
split, retain its parent and add child IDs. Check a box only
after the relevant reviewed integration and evidence exist; unchecked native/live
gates remain visible. PRs report package owner, dependency impact, workspaces/runtime/apps/server/tests/skips,
rollback consequences, and docs changes. This plan does not automatically close
#9 or treat a measurement proposal as permission to restart production.

This approved specification selects D-01–D-07. Issue #37 owns checklist status
and links to reviewed evidence. Approval authorizes the staged plan; each stage
still requires its named review, checks, and safety gates. Rollout budgets remain
an explicit later measurement gate.

### Changes — 2026-10-11

Reconciled the in-process native boundary, test automation, completed timing work,
Python extraction and typing rules, old-state migration policy, and the M-05
direction decision with the maintainer decisions and merged work through PR #80.
M-02.11 removed the root `scripts/` launchers in PR #57; M-03.6 is recorded in
PR #78. Test-time installer runs use isolated cache and command-bin directories.
The lead merges its own PR after hosted CI passes and checks the subsequent push
run on `main`.
