# Codex Studio

A desktop workspace for one lead agent and many workers. The application uses
Codex app-server for model sessions, tools, and permissions. It adds durable
orchestration, agent messages, command monitors, user tasks, and shared resources.

This repository owns the application, command tools, tests, and runtime prompts.
Its project-local [codex-workspace skill](.agents/skills/codex-workspace/SKILL.md)
describes managed tools and application workflows. No external skill repository
is required. Standard Codex CLI delegation belongs to the independent native
agent skill, not this application skill.

## Setup

Requirements: Python 3.11 or later, Node.js 22 or later, and the signed-in Codex CLI.
The desktop package currently targets macOS on Apple silicon.

From this repository:

```sh
python3 scripts/install-cli.py
npm --prefix web ci
npm --prefix web run build
npm --prefix desktop ci
npm --prefix desktop start
```

The command installer creates links in `~/.local/bin`. Add that directory to PATH
if your shell does not include it. Use `--bin-dir` for another directory.
It refuses to replace unrelated files or links. To update links from the old
checkout, pass `--replace-from /path/to/previous-checkout`.

For browser access, run `codex-canvas` and open <http://127.0.0.1:4620>.
Use `codex-control list` to inspect the same runtime from a terminal.
Run `npm --prefix desktop run package` to build the desktop application.

## Source and contracts

| Path | Contents |
|---|---|
| `web/` | React, TypeScript, Mantine, Vite |
| `desktop/` | Electron host, native bridge, package tools |
| `scripts/` | Python server and command tools |
| `prompts/` | Runtime worker instructions |
| `tests/` | Backend, protocol, browser, and process contracts |

- [Orchestration](ORCHESTRATION.md): managed agents, messages, monitors, and state.
- [Command guide](CLI.md): waves, steering, resource claims, and CLI usage.
- [Web development](web/README.md): client structure and browser checks.
- [Desktop](desktop/README.md): native boundaries, launch, and packaging.
- [Accounts](ACCOUNTS.md): account selection, profile discovery, and isolation.
- [Time awareness](TIME-AWARENESS.md): native time reminders and cache evidence.
- [Extraction record](EXTRACTION.md): source history and local migration checks.

## State

Existing chats, receipts, and agent state remain in
`~/.local/state/codex-agents/canvas.sqlite3`. Source extraction does not move state.
`CODEX_AGENTS_STATE_DIR`, `CODEX_BOARD_STATE_DIR`, and `CODEX_HOME` retain their meanings.
Canvas positions and drafts belong to each browser profile.
Closing Electron leaves the backend active.

## Checks

Run checks for the changed area. These commands use isolated fixtures:

```sh
node tests/portable-smoke.mjs
node tests/state-contract-smoke.mjs
python3 -B tests/daemon-contract.py
python3 -B tests/runtime-contract.py
python3 -B tests/turn-start-contract.py
python3 -B tests/accounts-contract.py
python3 -B tests/runtime-accounts-contract.py
python3 -B tests/canvas-contract.py
python3 -B tests/install-cli-contract.py
npm --prefix web test
npm --prefix desktop test
```

Browser checks use the installed Chrome, or `CHROME_BIN`. Desktop checks use hidden
Electron windows. Native protocol fixtures can run without paid model requests.
Read each test before running a check that uses the live Codex service.
