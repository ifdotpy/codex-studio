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

## Code checks

Run `npm run typecheck:runtime` (or `python3 scripts/codex_python.py --mypy`) to run the strict mypy ratchet across the runtime. It is intentionally not part of the pre-commit hook.

The repository root owns Oxlint and Oxfmt. Install their pinned dependencies with
`npm ci`. Activate the required pre-commit hook once per clone with
`git config --local core.hooksPath .githooks`. The hook checks staged JavaScript,
TypeScript, CSS, HTML, Markdown, YAML, and JSON from the Git index, including
files outside `web/`; it must not rewrite the index or working files. Run
`npm run test:pre-commit` to verify the hook's staged-content behavior.
Use `npm run lint:all` and `npm run format:check:all` for whole-repository audits;
append explicit paths after `--` when checking selected files.
