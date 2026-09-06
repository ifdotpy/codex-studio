# Codex Agents

This repository owns the application, command tools, and the project-local
`.agents/skills/codex-workspace` skill for its managed capabilities. Keep that
skill specific to this application. The application requires no external skill
repository.

Read README.md and the relevant source before editing. Use the engineering-protocol
skill for substantive work when it is available. Match checks to the changed scope.

Keep runtime state outside this checkout. Preserve existing state directory and
board identities. Never start a second backend against an occupied state directory.
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
