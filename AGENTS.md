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

Run the tests before you open a pull request against `main` or merge into `main`.
Use the server and client suites in [docs/testing.md](docs/testing.md), and state
the results in the pull request.

## Load testing

Keep load, stress, and performance-under-contention runs separate from ordinary
checks. An ordinary test, suite, or script must not start synthetic CPU, memory,
disk, or network load. Run a load test only as its own explicitly selected run,
never alongside the ordinary checks of other agents or the user's work on the
same machine. State its duration and resource budget before it starts, and tell
the team. Its owner stops every load generator when the run ends or fails; do not
leave detached generators. Results of ordinary checks taken while a load run was
active are not valid evidence.

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
