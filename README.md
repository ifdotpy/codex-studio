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

Requirements: Python 3.11 or later, Node.js 22.15 or later, and the signed-in Codex CLI.
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

## iPhone access

The mobile layout shows orchestrator chats, existing projects, account selection,
and chat settings. Add the page to the iPhone home screen for a standalone window.
The Mac remains the server. SQLite remains the authoritative store.

After the source update, restart Studio when active tasks and monitors finish.
Do not stop active work for this update. Then run:

```sh
python3 scripts/codex-mobile.py
```

The command checks the server version and enables private HTTPS through
[Tailscale Serve](https://tailscale.com/docs/features/tailscale-serve).
It prints the address for Safari. Connect Tailscale on both devices first.
If Tailscale requests HTTPS setup, complete that setup and run the command again.
Existing unrelated Serve configurations require manual setup.
The server accepts the exact origin in `remote-access.json` in its state directory.
Set `enabled` to `false` there to revoke remote access without a restart.
Tailscale access rules control who can open Studio. Studio adds no separate login.

RxDB replicates SQLite projections to IndexedDB. It keeps message identities across
reloads and retries. Different browser drafts remain separate and can be combined.
A lost response does not authorize another underlying command.
The page must load before it can work during a connection loss. There is no offline
application shell. Keep the page open for sync and voice.

Start voice inside the selected chat. The OpenAI Realtime courier clarifies speech.
It does not solve the project task. Send the full new transcript with its button or
an explicit voice command. You can edit the text before you send it.
The orchestrator uses `orchestration_speak` to save exact text for audio playback.
Outside a voice session, that text remains silent. Interrupt stops audio only.
The Repeat button replays interrupted text.

Voice requires a separate OpenAI API key. Set `OPENAI_API_KEY` in the server
environment, or create `voice-config.json` in the state directory:

```json
{"apiKeyFile": "/absolute/path/to/private-key-file"}
```

The key stays on the Mac. Voice uses the OpenAI API account's usage and charges.
Voice text and events remain until the chat is deleted. Audio expires after seven
days; the active server removes expired files on its hourly maintenance pass.
Lock-screen and background voice are not supported.

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
- [Context analytics](ANALYTICS.md): response tokens, tool payloads, history, and measurement limits.
- [Extraction record](EXTRACTION.md): source history and local migration checks.

## Team conversations

Open **Agent chats** beside **Background** in the selected chat's toolbar.
The panel shows that team's broadcast channel and private conversations between its members.
Search the room list, select a conversation, or load earlier messages.
The main conversation and its draft stay open. On mobile, use **Chat settings → Agent chats**.
The sidebar contains project conversations only.

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
python3 -B tests/prepare-steer-contract.py
python3 -B tests/monitor-lifecycle-contract.py
python3 -B tests/harness-response-contract.py
python3 -B tests/protocol-reader-contract.py
python3 -B tests/catalog-recovery-contract.py
python3 -B tests/tool-request-contract.py
python3 -B tests/spawn-request-recovery-contract.py
python3 -B tests/tool-request-http-contract.py
python3 -B tests/request-recovery-legacy-contract.py
python3 -B tests/task-completion-recovery-contract.py
python3 -B tests/analytics-contract.py
python3 -B tests/analytics-history-contract.py
python3 -B tests/accounts-contract.py
python3 -B tests/runtime-accounts-contract.py
python3 -B tests/limits-refresh-contract.py
python3 -B tests/canvas-contract.py
python3 -B tests/install-cli-contract.py
python3 -B tests/panel-contract.py
python3 -B tests/panel-feed-contract.py
python3 -B tests/panel-feed-monitor-contract.py
python3 -B tests/ec2-panel-feed-contract.py
python3 -B tests/ec2-panel-render-contract.py
python3 -B tests/panel-callback-contract.py
python3 -B tests/panel-render-contract.py
python3 -B tests/structured-panel-render-contract.py
python3 -B tests/worker-overview-contract.py
python3 -B tests/turn-history-contract.py
python3 -B tests/question-history-contract.py
npm --prefix web test
npm --prefix desktop test
```

Browser checks use the installed Chrome, or `CHROME_BIN`. Desktop checks use hidden
Electron windows. Native protocol fixtures can run without paid model requests.
Read each test before running a check that uses the live Codex service.
