# Codex Studio

This repository owns the application, command tools, and the project-local
`.agents/skills/codex-workspace` skill for its managed capabilities. Keep that
skill specific to this application. The application requires no external skill
repository.

Read README.md and the relevant source before editing. Use the engineering-protocol
skill for substantive work when it is available. Match checks to the changed scope.

Keep runtime state outside this checkout. Preserve the existing state directory identity. Never start a second backend against an occupied state directory.
Do not stop active user agents, monitors, terminals, or waves for a source update.

The server owns SQLite and orchestration. The renderer uses its HTTP API. Native
privileges belong to the isolated Electron main process. Preserve permission checks,
exact request identities, and uncertainty after lost responses. A retry must not
repeat a command, user message, or reset-credit charge.

Use hidden windows for automated desktop checks. Do not activate applications or
inject operating-system input during tests. A successful fixture does not prove a
live model request or a real reset-credit redemption.

Commit named files. Preserve unrelated work and build outputs. Do not publish the
repository or change its license without explicit authorization.

Write everything in this repository in English: code, comments, commit messages,
pull requests, issues, and documentation.

Run the fast tier before every pull request and merge, and state its results in
the pull request. Run heavy-tier checks by hand as announced runs; they are not
in hosted CI for now. Each heavy run has a Just command. A test that needs
repeated runs to be trusted is defective and must be rewritten or moved to the
heavy tier. New tests must fit the fast tier. See [docs/testing.md](docs/testing.md)
for the tier commands and suite lists.
A direct push to `main` is allowed, but it skips the pull-request checks, so run
the same fast tier locally first: `just test-fast` (four server shards plus the
remaining checks: Rust tests, web unit tests, type checks, and browser smoke; a
few minutes). Push only when it passes on the exact commit being pushed.
`just test-server-opt-in` alone is not enough: it covers neither the convention
guards in the server shards nor the Rust and web suites, and both gaps have
broken `main`. Browser smoke is included in that fast tier and runs in hosted
CI.
The `just test-fast` checks run in hosted CI on every pull request and push to
`main`; CI splits the server tier across four parallel jobs and runs browser
smoke in its own job.

## Load testing

Keep load, stress, and performance-under-contention runs separate from ordinary
checks. An ordinary test, suite, or script must not start synthetic CPU, memory,
disk, or network load. Run a load test only as its own explicitly selected,
announced run, never alongside the ordinary checks of other agents or the user's
work on the same machine. State its duration and resource budget before it
starts. Its owner stops every load generator when the run ends or fails; do not
leave detached generators. Results of ordinary checks taken while a load run was
active are not valid evidence. See [docs/testing.md](docs/testing.md) for the
single-suite Just entry.

## Code checks

Run `pnpm run typecheck:runtime` (or `python3 workspaces/runtime/apps/server/src/codex_python.py --mypy`) to run the strict mypy ratchet across the runtime. It is intentionally not part of the pre-commit hook.

The repository root owns Oxlint and Oxfmt. Install their pinned dependencies with
`pnpm install --frozen-lockfile`. Activate the required pre-commit hook once per clone with
`git config --local core.hooksPath .githooks`. The hook checks staged JavaScript,
TypeScript, CSS, HTML, Markdown, YAML, and JSON from the Git index, including
files outside `workspaces/client/apps/web/`; it must not rewrite the index or working files. Run
`pnpm run test:pre-commit` to verify the hook's staged-content behavior.
Use `pnpm run lint:all` and `pnpm run format:check:all` for whole-repository audits;
append explicit paths after `--` when checking selected files.
