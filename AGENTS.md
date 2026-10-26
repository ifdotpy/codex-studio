# Codex Studio

This repository owns the application, command tools, and the project-local
`.agents/skills/codex-workspace` skill for its managed capabilities. Keep that
skill specific to this application. The application requires no external skill
repository.

## Simplicity

Implement only the requested behavior, with the least code. Add no state,
branches or recovery layers beyond it; if a change needs them, ask the owner
first. Prefer a small documented limitation to complex machinery. Code must be
describable in a few sentences.

## Fixes and test load

Each bug fix needs a failing test first and a passing `just test-fast` stated in the PR;
a package for installation also needs `just test-comprehensive` on its exact commit.
Keep test load low: run focused tests, use `cargo -j 4`, and run at most one full
`just test-fast` or workspace nextest at a time per agent.

Read README.md and the relevant source before editing. Use the engineering-protocol
skill for substantive work when it is available. Match checks to the changed scope.

Keep runtime state outside this checkout. Preserve the existing state directory identity. Never start a second backend against an occupied state directory.
Do not stop active user agents, monitors, terminals, or waves for a source update.
Change the live server of this host only with `just deliver-server`; never edit its
systemd drop-ins or binary path by hand.

The server owns SQLite and orchestration. The renderer uses its HTTP API. Native
privileges belong to the isolated Electron main process. Preserve permission checks,
exact request identities, and uncertainty after lost responses. A retry must not
repeat a command, user message, or reset-credit charge.

Each shared value or constant must have one named owner with its purpose and
unit. Runtime validators must reference that owner; other languages, schemas,
and UI must consume a generated or shared contract. Where generation is
unavailable, add a fast parity check that identifies the owner and checks every
consumer. Numerically equal values with different purposes or units remain
distinct contracts.

Use hidden windows for automated desktop checks. Do not activate applications or
inject operating-system input during tests. A successful fixture does not prove a
live model request or a real reset-credit redemption.

Commit named files. Preserve unrelated work and build outputs. Do not publish the
repository or change its license without explicit authorization.

Write everything in this repository in English: code, comments, commit messages,
pull requests, issues, and documentation.

Product UI copy and visual labels must be platform-neutral. Refer to the device,
server, project folder, virtual machine, file manager, or secure storage instead
of naming an operating system or platform brand. This includes status and error
messages supplied by the runtime. Preserve capability checks and technical API
identifiers; explain an unavailable capability without implying platform support.
Platform-specific installation and operator documentation may name the platform
when it is required to perform the documented action.

Run `just test-fast` before every pull request and merge, and state its result
in the pull request. This short local tier selects deterministic request,
recovery, schema, and boundary witnesses. Hosted CI continues to run the full
server, Rust, web, tooling, and browser-smoke checks in parallel jobs.

Run `just test-comprehensive` locally on the exact commit before every push
to `main`. A successful short tier does not replace this publication gate.
The comprehensive command preserves the
previous broad local gate, including provider bridge tests, workspace Rust
tests, web type and unit checks, tooling tests, and browser smoke. Run
heavy-tier checks by hand as announced runs; they are not in hosted CI for now.
Each heavy run has a Just command. A test that needs repeated runs to be trusted
is defective and must be rewritten or moved to the heavy tier. New tests must
fit the short tier. See [docs/testing.md](docs/testing.md) for the tier commands
and selection owner.

## Load testing

Keep load, stress, and performance-under-contention runs separate from ordinary
checks. An ordinary test, suite, or script must not start synthetic CPU, memory,
disk, or network load. Run a load test only as its own explicitly selected,
announced run, never alongside the ordinary checks of other agents or the user's
work on the same machine. State its duration and resource budget before it
starts. Its owner stops every load generator when the run ends or fails; do not
leave detached generators. Results of ordinary checks taken while a load run was
active are not valid evidence. See [docs/testing.md](docs/testing.md) for the
manual-run rules.

## Live data

Server data reaches the client only as SSE events; the client never polls. See
[the no-polling rule](docs/sync-protocol.md#rule-no-polling-only-sse-events).

## Code checks

The repository root owns Oxlint and Oxfmt. Install their pinned dependencies with
`pnpm install --frozen-lockfile`. Activate the required pre-commit hook once per clone with
`git config --local core.hooksPath .githooks`. The hook checks staged JavaScript,
TypeScript, CSS, HTML, Markdown, YAML, and JSON from the Git index, including
files outside `workspaces/client/apps/web/`; it must not rewrite the index or working files. Run
`pnpm run test:pre-commit` to verify the hook's staged-content behavior.
Use `pnpm run lint:all` and `pnpm run format:check:all` for whole-repository audits;
append explicit paths after `--` when checking selected files.
