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

For browser tasks, Studio uses the installed OpenAI Chrome plugin by default.
Set up the ChatGPT browser extension and browser runtime in ChatGPT Desktop first.
Studio registers the installed Chrome skill with native `skills/extraRoots/set`.
It passes the browser runtime to native `thread/start` and `thread/resume`. Codex
advertises the skill with its path and exposes MCP tools to leads and workers.
Studio does not copy skill text, browser code, or another account’s marketplace.
Each account keeps its own credentials and browser turn history. Explicit plugin
or MCP disable settings and custom runtime commands remain in effect.
Already loaded threads can retain their previous browser runtime. To reconnect an
idle session, unsubscribe and resume that same native thread with the current
configuration. A resume while still subscribed can ignore changed settings.
Active work is not interrupted. Browser website permissions remain native.
Studio detects native Chrome discovery failures and makes one automatic recovery
at an idle turn boundary. It waits for active commands and monitors, preserves
the native thread, and sends a read-only verification instruction. It never
replays a browser action. A failed probe or unknown reconnect response stops
automatic retries; exact request IDs remain in the session recovery record.

For browser access, run `codex-canvas` and open <http://127.0.0.1:4620>.
Use `codex-control list` to inspect the same runtime from a terminal.
Run `npm --prefix desktop run package` to build the desktop application.

The desktop compares its installed backend source with the running server.
It shows an update notice when they differ. Closing the window preserves the
server and its active work, so reopening the window does not apply backend changes.

## iPhone access

The mobile layout shows orchestrator chats, team agents, existing projects, account selection,
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
After one complete online load, the browser can cache the interface for later use
without the Mac connection. Cached chats and drafts can open while the Mac is
unavailable. Browser storage must still contain that data.
Text messages for existing chats enter a local queue before the composer clears.
The queue retries the same message identity when Studio can reach the Mac again.
New chats, new file uploads, and voice require the Mac connection.
Return to Studio to resume sync and delivery. Delivery while iOS suspends Studio
is not guaranteed. Keep the page open for voice.

The chat snapshot excludes work result histories. The work view loads those
histories through its existing API. Mobile sync uses one shared event stream.
While Studio is visible, it prepares unarchived chats and the selected team's
agent chats in the background. It updates these saved histories before selection.
The current chat loads first. At most two background histories load at once.
A ready history appears immediately on selection, including its saved scroll position.
After a network change or a return to Studio, sync replaces the old connection.
A cached workspace cannot send drafts to a different workspace before verification.

Run `npm --prefix web run test:mobile` for the mobile regression suite.
Set `BROWSER=webkit` for the WebKit lifecycle and delivery checks.
The performance fixture uses Chromium network and CPU controls in either run.
These checks do not replace a test on a physical iPhone.

Start voice inside the selected chat. Native Codex voice uses that chat's
ChatGPT account through its app-server. No separate API key is required.
Voice can pass spoken tasks directly to the orchestrator. It does not wait for
a separate Send transcript button. Normal orchestrator replies return to voice.
End voice stops the microphone and voice connection. It does not stop the task.
Transcripts remain on the Mac until the chat is deleted. Old courier drafts
remain available for recovery; Studio does not automatically send them.
`orchestration_speak` supplies additional speakable context to native voice.
Its receipt does not confirm exact audible playback. Use chat buttons for
permission requests. Lock-screen and background voice are not supported.

Existing loaded threads need the realtime feature. Studio enables it by reloading
only an idle lead without background commands. Active threads remain running;
start voice again after their current work finishes.

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

## Projects

Use **Rename project** to change the project label. The directory path stays the same.
Use **New folder** and **New subfolder** to add folders for chats.
Use **Move to folder** in the chat menu. These folders exist only in Studio.
A chat keeps its directory and account.

Each project has one default account. Use **Project account** in the project menu
to select it on desktop or mobile. New chats use that account. Existing chats
keep their native account identity. An empty chat can use any available account.
Models can use files and skills outside the project directory. Native sandbox and
approval settings still apply.

## Messages and agent roles

Open **Messages** in the chat header on a computer or phone.

- **For you** opens first. It contains questions, permissions, your tasks, and messages from the main agent.
- **Team** contains **To orchestrator**, **Team broadcast**, and **Between agents**.

The main agent has the orchestrator role. Subagents ask it for help.
Only the main agent sends conversational messages or tasks to you.
Native tool permissions still require your approval.

Use a stable `request_id` with `orchestration_send` to recover a lost reply.
An exact retry returns the saved receipt without another instruction.
Worker recovery reconciles saved read and validation failures before archive checks.
Unknown mutations still block archive. Tool history and files remain available.
Team status includes the configured concurrency and each queued agent's current blockers.
Messages replace the separate Inbox, Agent chats, and Complaint book screens.
The main conversation and its draft stay open.
You can close a reply and return to its draft.
**Send for review** submits a task result to the main agent.

The harness reads the repository's `codex-orchestrator` or `codex-subagent` skill
from the server's `isLead` identity. It adds that skill to the native thread
instructions. Existing threads receive it on their next turn. The versioned turn
context repeats it after a skill change or context compaction.
Workers cannot create or change user tasks or send spoken responses to the user.

**Team** shows subagent status and opens subagent chats.
**Back to main agent** returns to the main agent.
The sidebar and Team panel can collapse at any window width.

**Chat settings** contains the account, project, model, permissions, and appearance.
Appearance supports **System**, **Light**, and **Dark**.
**Chat actions** contains agent tasks, your tasks, changes, plan, rules, search, and background tasks.
**Search chats** searches full history. **Filter projects and chats** filters the sidebar list.
The search control above the transcript searches the current chat.
**Edit** and **Another answer** prepare a draft in a new branch. They do not send it automatically.

On a phone, **Add project** opens the server's folder list.
The first **New chat** opens this list when no project exists.
**Plan** shows only the plan reported by the agent. Use **Change plan in chat**
to send instructions. Historical saved plan text remains in storage.

## State

Existing chats, receipts, and agent state remain in
`~/.local/state/codex-agents/canvas.sqlite3`. Source extraction does not move state.
`CODEX_AGENTS_STATE_DIR`, `CODEX_BOARD_STATE_DIR`, and `CODEX_HOME` retain their meanings.
Historical Canvas positions remain in each browser profile. Message drafts remain available.
Closing Electron leaves the backend active.

## Checks

Run checks for the changed area. These commands use isolated fixtures:

```sh
node tests/portable-smoke.mjs
node tests/state-contract-smoke.mjs
python3 -B tests/daemon-contract.py
python3 -B tests/runtime-contract.py
python3 -B tests/role-skills-contract.py
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
