# Managed Codex teams

Use the [Electron desktop app](desktop/README.md), or build the [React interface](web/README.md), start `scripts/codex-canvas`, and open <http://127.0.0.1:4620>.
Select **New chat**. The server creates an empty lead conversation immediately.
If the current lead chat is empty, it reuses that chat and preserves the draft.
Write the task in the conversation. The lead generates its title with `orchestration_title`.
Creation itself does not call the model. A repeated creation request returns the same chat.
Leads carry an explicit `isLead` marker in SQLite. Only Astra and Sol can be leads.
Standalone workers and registered sessions remain visible on **Canvas**.
The project defaults to the previous lead's directory, `CODEX_CANVAS_CWD`, or the server's current directory.
Before the first message, select **Project** beside the account to change the folder.
Enter a folder path, browse directories, or use the native Finder picker.
Open **Lead** in the chat header to choose Astra or Sol, its reasoning level, and **Fast**.
**Lead** and **Subagents** use the same dropdown. Changes save immediately.
Changes to an agent's execution settings require an idle turn.

**Subagents** sets the model, reasoning level, and Fast default for future workers in this team.
The lead can override each value in `orchestration_spawn`.
Omitted fields use the latest root lead defaults, including for workers created by another worker.
A null default model uses the root lead model. A null effort uses the selected model's native default.
Fast defaults to off. The main agent's reasoning and Fast settings do not become worker defaults.
Explicit profile model and effort values override team defaults; explicit spawn fields override the profile.
A profile's null effort leaves the team default in effect.
If a worker selects another model that cannot use the inherited reasoning level, it uses that model's native default.
An unsupported explicit reasoning level or Fast mode returns an error.
The account's native model catalogue defines supported values.

Only the user can change team defaults, including during a running turn.
Existing workers retain their settings. A new chat in the same account copies the previous team's defaults.
Fast uses native `serviceTier=priority`; off sends `serviceTier=default`.
Reasoning and service tier apply on thread start, resume, and each turn.

Codex 0.153.4 accepts dynamic tool definitions only when a native thread starts.
Existing threads still use user defaults, but their old spawn schema lacks the per-worker `fast_mode` override.
Start a new lead chat to expose that new tool parameter. Existing histories are preserved.

The conversation shows Markdown, tool results, queued messages and agent questions.
Consecutive tool calls share one collapsed activity row. Running calls and failures remain visible as counts.
Open the row to inspect compact tool entries. Open an entry for its command, arguments, and output.
New calls and output updates do not reopen a group that the user closed.
Enter sends a message. Shift + Enter inserts a new line.
Workers appear on the right. Select a worker to read its conversation, then select **Lead** to return.
On a narrow screen, **Team** opens the worker panel.
Select **Canvas** for all orchestrators, workers and registered sessions. Drag nodes to move them, drag the background to pan, and scroll to zoom.
The canvas has one control, **Fit**. Parent links come from the runtime.
The browser keeps existing canvas positions and message drafts when views change.
Manual graph selection modes, chat wiring, the minimap, and duplicate zoom controls were removed.
Shared agent chats remain in the **Agents** tab. Registered sessions remain on **Canvas**.

The conversation header provides context compaction, review, and team stop.
The **Projects** sidebar groups lead chats by their actual working directory.
Use **+** beside Projects to add a folder, including a project with no chats.
Use **+** beside a folder to start a chat there. An empty current chat is reused.
Folder registration lives in SQLite. Removing an empty project from the sidebar does not delete files.
Each folder initially shows five chats. **Show more** expands the list, and search includes hidden chats.
Folder collapse preferences stay in browser storage. Agent chats retain their separate **Agents** tab.
Use the sidebar row menu to rename or delete a lead or agent chat.
Renaming stays in SQLite and takes priority over an automatic lead title.
Deleting an agent chat removes it from the user list until the next agent message. Its participants retain their history.
Deletion stops the selected agent and its descendants and removes them from the interface.
Stored transcripts and project files remain on disk. Deleted agents cannot resume from late events or message retries.
Agents create monitors through `orchestration_monitor`. The interface shows their output and controls but has no manual monitor form.
Monitor commands use the local user's shell and Codex's `allow_login_shell` setting.
With shell snapshots enabled, they load the same zshrc or bashrc setup as native
`exec_command`. Explicit `shell_environment_policy.set` values take priority over
startup files. Codex `command/exec` still applies account environment filters and
the selected sandbox. The harness does not replace PATH with its own value.
The monitor loads startup files when its process starts; it does not read a
temporary native snapshot that can disappear when the agent's turn ends.
The bottom terminal panel shows only user shells. **Background** lists agent commands and monitors across teams.

### Agent display panel

Each managed conversation has a fixed 150px panel between the transcript and
composer. The calling agent controls it through `orchestration_panel`:
`set` replaces HTML and optional CSS, `get` reads the current document, and
`clear` empties the display. A worker cannot overwrite its lead's panel.
Agent chat rooms and unmanaged conversations do not have this display.
The agent instructions call for visual status: stage tracks, segmented bars,
compact counters, CSS grids, and SVG diagrams with short labels. Chat paragraphs
and invented progress values do not belong in this panel. The composer uses one
action row for attachments, delivery, and send. The text field grows with its draft;
context, compactions, and account limits remain directly below it.

The panel and composer are adjacent siblings. Queued messages, errors, approvals,
and user tasks appear above the panel. Callback feedback stays inside the panel.
Semantic HTML inherits Studio colors, typography, buttons, and form controls.
Agent CSS can override those defaults, including the `--studio-*` variables.

The panel renders inline HTML/CSS/SVG and CSS animations in an opaque sandbox.
Agent scripts, external requests, navigation, and parent access are disabled.
Only the fixed host bridge runs. It handles declared buttons and forms after
trusted user input. HTML accepts up to 128 KiB of UTF-8 text; CSS accepts up to 32 KiB.
The server stores one current document per agent in `runtime_panels`. Each write
increments its version. A retry with the same tool identity returns its original
receipt and cannot replace a newer document. Native tool delivery returns the
cached result for a repeated call identity; a new update needs a new call identity.
The document survives a server restart. A new agent starts with an empty panel.

`set` and `get` include a rendered PNG in the native tool result. An isolated,
hidden Electron process renders the exact accepted revision at 1000x150 CSS pixels
with the same document builder as the visible panel. The capture does not depend
on an open chat. Real window widths can differ. At most two captures run together;
capture waits at most 30 seconds for a slot and has a 15-second process timeout. Image failure leaves the saved document intact and
returns an explicit error. A new `get` retries the image without another write.

`set` can declare up to 16 callbacks, each with `id`, `label`, and up to 32 `fields`.
Use `data-callback="id"` on a button or form and named form inputs. Field values
arrive as arrays of strings. The parent verifies the current iframe and channel,
then posts to `/api/panel/callback` with the local CSRF token. The token never enters
the iframe. The server checks panel version, declared action/fields, and owner.
It stores the receipt and `panel_callback` event in one transaction. The owner
receives that event after its current turn, including after a final answer.
Stopped or deleted agents do not resume through panel callbacks.

One action is accepted per callback per panel version. Double-clicks and exact
retries do not enqueue twice. Changed values for an already submitted action or a
stale panel return HTTP 409. Publish a new panel version to enable the action again.
Form submissions are limited to 16 KiB, 16 values per field, and 2000 characters
per value. File fields are not uploaded by this bridge.

Snapshots carry only `panelVersion`. The selected conversation fetches its
document from `GET /api/panel?agent=<id>` when that version changes. Full documents
are excluded from shared snapshots so dozens of agents do not multiply transfer
size. Older threads use the documented `orchestration_send` workspace fallback.
`/compact`, `/review`, `/stop`, and `/stop-team` are local commands.
Team capacity and token budgets remain available through `codex-control configure`.
One lead can delegate a batch of work to dozens of agents.
The browser can close while the canvas server continues the work.

The runtime uses the installed `codex app-server` through JSON Lines on stdio.
It uses the existing Codex account and configuration selected by `CODEX_HOME`.
It does not replace the Codex model loop or require a Codex fork.
The tested protocol version is Codex CLI 0.153.4.

## Agent and command lifecycle

The lead receives these additional tools:

| Tool | Behavior |
|---|---|
| `orchestration_complaint` | Submit a complaint, read the team book, or record the responsible lead's response. |
| `orchestration_peers` | Discover all managed agents and the caller's chat rooms. |
| `orchestration_message` | Send to an agent id, `parent`, `lead`, `broadcast` (team), or `all` (all teams). |
| `orchestration_chat_read` | Read a participant chat, with a cursor for older messages. |
| `orchestration_title` | Set the conversation title from the task. Only a lead can call this tool. |
| `orchestration_interrupt` | Stop a descendant and its descendants. A follow-up can resume them. |
| `orchestration_spawn` | Create up to 64 workers in one request. Each worker has a task, role, and optional model, effort, and `fast_mode` overrides. |
| `orchestration_send` | Queue a follow-up for a descendant. An explicit follow-up can resume a stopped descendant. Other targets use chat delivery. |
| `orchestration_status` | Read team status and command watches for a decision. |
| `orchestration_monitor` | Start a command watch. Deliver one result when the command exits. |
| `orchestration_cancel_monitor` | Cancel a command watch. |

After delegation, the lead can finish its turn. The runtime queues each child
result and starts the next lead turn, including after a final answer.
Events that arrive during a turn wait for that turn to finish. Up to 32 events
are combined in one input. Repeated completion notifications share an event id.
A worker with pending children or command watches stays in the waiting state.
Its parent receives a result after that work settles. A reported result still needs review.

A `turn/start` timeout does not prove that the turn failed to start.
The runtime retains its reservation until native events or the response establish the outcome.
New input cannot start another turn while that outcome is unknown.
Delivery requires the start response or a user-message event with the matching client ID.
A late response can acknowledge its original input batch but cannot replace a newer turn's state.
Unknown input is never replayed automatically.

Thread preparation shares one pending request per agent. A preparation timeout
retains the input reservation without submitting a turn. A late response resumes
that exact batch only while its agent, account, connection and settings still match.
Steer, Compact and Review also retain unknown outcomes after response timeouts.
Late acknowledgements cannot resume a stopped agent or replace a newer turn.

## Agent chat

Select the **Agents** tab in the sidebar to read private conversations and broadcasts.
Each message shows its author and timestamp. **Earlier messages** loads stored history.
The user can observe every room. Other agents can read private rooms only when they are participants.
This is a chat-tool rule, not filesystem isolation between processes on the same machine.

Agents discover one another with `orchestration_peers`. `orchestration_message`
creates a private room for two agents or publishes a broadcast. Team broadcasts
include the lead and its descendants. The `all` target includes other teams.
Agents created later can read earlier broadcasts but do not receive their old wake events.

The message and each recipient event commit in one SQLite transaction. Repeating
one tool call does not duplicate delivery. Messages wait behind an active turn and
wake a finished recipient. Stopped agents and unused blank leads receive stored
history only. Peer messages never resume a stopped agent. Agent messages carry
agent provenance; they do not add user authority. Instructions prohibit acknowledgement loops.

Codex persists dynamic tools when it creates a thread. Existing threads can use
`orchestration_status` for peers and recent chat history, and `orchestration_send`
for parent, lead, peer, or broadcast messages. New threads have all three dedicated chat tools.

Native subagent tools are disabled only in these managed threads with `agents.enabled=false`,
`features.multi_agent_v2=false`, and `features.multi_agent=false`. The last flag alone does not
disable Astra’s model-selected v2 agents in Codex 0.153.4. All delegation
uses the managed tools so the scheduler can enforce limits and track parent edges.
Existing native agents and legacy app-server waves remain visible on the canvas.
Their recorded creator edges alone do not give this runtime control of them.

A Monitor watch runs through `command/exec` with the thread's effective permission
profile or sandbox. The runtime drains output without model calls. It stores at
most 20 MiB per log and retains a 12,000-character tail. The final event includes
exit code, command status, output tail, log path and total output bytes.
The default command timeout is one hour; the maximum is 24 hours.
An acknowledgement timeout does not finish a command. Cancellation retains the
active workspace reservation until the command exits. The interface shows
**Waiting for exit** during this interval. Closing terminal input blocks further
input while its acknowledgement is pending. An unknown close is not retried.

The inherited `never` approval policy does not add an approval step. Other policies
require approval for a model-created Monitor command in the conversation.
Monitor execution retains that sandbox. User shells in the terminal panel run directly as the local user, like an ordinary terminal.
They do not start a model turn.

## Permissions

New teams start with **YOLO mode** enabled. The Lead settings menu controls it for
all team members, including future workers. It sends `approvalPolicy=never` and
`sandbox=danger-full-access` to Codex, the native CLI skip-permissions combination.
Turning it off explicitly selects `on-request` and `workspace-write` (read-only
for reviewers). Wait for team turns, tools, and monitors to end before changing it.
Each turn also receives the explicit policy because native resume can ignore
overrides for a loaded thread. Monitors use the same selected sandbox.
Existing teams retain their inherited Codex permissions until the user selects a
mode. A new chat copies an explicit mode preference from the previous chat.

Account project admission rules remain separate. YOLO does not enable
**Dangerously skip rules**. It does not supply answers to agent questions.

## Complaint book

Leads and workers call `orchestration_complaint` with `action=submit`, a concrete
problem, impact, and evidence. Responsibility comes from the author:

- Worker complaint: the team lead responds, under **For orchestrator**.
- Lead complaint: the user responds, under **For you**.
- User complaint: the selected team lead responds.

Workers cannot close complaints. A lead cannot respond to or close their own
complaint. The user cannot take over a complaint assigned to a lead through the
user response endpoint. Only the assigned recipient can record a response.

A worker complaint delivers its full text, author, and id to the lead and starts
a turn, including after a final answer. The lead calls `action=respond` with a
concrete action or reason and `in_progress`, `resolved`, or `declined`. The lead
must not poll the book or routinely call `action=read`.

Lead complaints stay in the user's inbox without starting a lead turn. The user
records a response and status in the complaint dialog. That response notifies
the reporting lead. A stopped lead keeps the record but does not resume.

Unanswered complaints assigned to the lead remain in that lead's turn context.
Three consecutive turns that ignore presented complaints stop automatic lead
continuation with a visible error. Complaints assigned to the user do not block
lead completion. A recorded next step counts as a response; `in_progress` entries
remain visible under **All complaints**.

Responses remain append-only and record their actual author. User responses use
an exact request id and complaint version. Repeated requests cannot add a second
response or notification. A stale version returns HTTP 409. Reads alone do not
close complaints. A status records the recipient's claim, not independent proof
of a repair. Records survive restarts and conversation deletion.

Existing lead-authored entries move to **For you**. Historical lead responses
remain visible, but cannot count as user action. Those entries reopen when no
user response exists. Existing worker entries keep their responsible lead.

Older threads can submit through `orchestration_send` with `agent_id=complaint`
and the same action object encoded as JSON in `text`. New threads receive the
dedicated complaint tool.

## Context and account usage

Below the composer, the client shows the most recent `last.totalTokens` and
`modelContextWindow` from `thread/tokenUsage/updated`. The percentage is their
ratio, not cumulative team token usage. It remains unavailable until Codex supplies
a usable window size. It is the last reported value, not a token-by-token estimate.

Completed `contextCompaction` items increment a durable count once per item id.
The previous context reading clears until the next token update. Threads created
before this counter existed show only observed compactions; the client does not
invent a full historical count.

**Limits** reads `account/rateLimits/read` without model inference and receives
`account/rateLimits/updated` events. It shows reported usage windows, reset times,
credits, and individual limits when available. Reads share a 30-second cache.
Missing or failed account data is explicit. These are account limits, not a separate
allowance for each agent.

[Context analytics](ANALYTICS.md) retains response usage and tool measurements
separately from the latest context indicator. The report supports agent, team,
and all-agent scopes, a period filter, call details, and JSON export.

## Capacity and work ownership

Each team defaults to 8 concurrent agents and a maximum of 64 total agents,
including its lead. `codex-control configure` supports 1 to 64 concurrent agents and 1 to 256
agents in the team. `CODEX_CANVAS_CONCURRENCY` sets the server-wide cap, default 16,
maximum 64. Lead turns have priority when a slot becomes free.
Lowering a limit does not interrupt existing turns.

Implementers receive separate Git worktrees under the parent's repository:
`.worktrees/codex-agents/<agent-id>`, on branch `codex-agent/<agent-id>`.
They start from committed HEAD. Parent changes that are not committed are absent.
Reviewers use the parent's directory. They have a read-only sandbox when YOLO is off.
The lead owns review and integration. The runtime never merges or deletes worktrees.

An optional team token budget sums Codex's reported thread usage. This includes
input tokens, including cached input. It is not a billing estimate or a strict
pre-request cap. When a usage notification reaches the limit, the runtime stops
the team and cancels its queued work and command watches. In-flight requests can
exceed the limit before their usage notification arrives.
Increase the budget with `codex-control configure`, then send a new instruction to resume.

## Stop and recovery

**Stop agent** stops only the selected agent. **Stop team** stops its lead and all
descendants. Stop disables automatic continuation, increments the agent generation,
cancels pending events and requests interruption of active turns and commands.
A late result from an earlier generation cannot resume that agent.
A new user message explicitly resumes the target. Other stopped agents stay stopped
until the user or a resumed lead sends them a new task.

The runtime stores agents, events, transcripts, command watches and approval requests
in `runtime_*` tables in `canvas.sqlite3`. Existing chat tables remain intact.
Only one runtime can own the state directory. SQLite uses write-ahead logging.

After a server restart, idle queues can resume. A turn that was active at shutdown
is marked interrupted. An unacknowledged delivery stays uncertain and is not replayed.
Review its transcript, then send an explicit new instruction. Active command watches
become lost when their exit result is unknown. They are not restarted automatically.
Command process survival across a server or machine restart is not guaranteed.

## Codex features

The underlying Codex configuration supplies model access, skills, tools, MCP servers,
context management and permissions. Agent transcripts include answers, tool results,
plans and changes. Internal reasoning records are not displayed.

The composer offers **After tool call** (native steer at the next model step) and
**After turn** (a queued message starts the next turn). Parallel tools must finish
before steer input is consumed; compaction can delay it.

The client exposes model selection, message queues, approvals, synchronous and asynchronous user questions,
context compaction and a native review of uncommitted changes.
Account login and advanced configuration remain in Codex CLI or `config.toml`.
The client answers `currentTime/read` and both current and legacy command/file approval requests.
Device attestation and externally supplied authentication refresh remain host-specific.
These requests fail visibly instead of receiving fabricated credentials or an automatic success response.

Import copies visible user and assistant messages from at most the last 20 turns,
limited to 24,000 characters, into a new managed lead. It does not take ownership
of another client's live session or copy its complete tool history.
The original Codex conversation remains available.

## Terminal control

`codex-control` talks to the same canvas server. It does not create a second
app-server process. Set `CODEX_CANVAS_URL` or pass `--url` for another local port.

```bash
scripts/codex-control models
scripts/codex-control create 'Review the project and delegate independent checks' \
  --cwd /absolute/project --name Lead --concurrency 8 --max-agents 64
scripts/codex-control list
scripts/codex-control send AGENT_ID 'Inspect the worker results and continue'
scripts/codex-control monitor AGENT_ID 'your-command' --timeout-minutes 60
scripts/codex-control transcript AGENT_ID
scripts/codex-control stop LEAD_ID
scripts/codex-control configure LEAD_ID --concurrency 12 --token-budget 2000000
```

## Verification

```bash
cd web
npm ci
npm test
npm run format:check
cd ..
python3 tests/runtime-contract.py
python3 tests/turn-start-contract.py
python3 tests/prepare-steer-contract.py
python3 tests/monitor-lifecycle-contract.py
python3 tests/canvas-contract.py
node tests/portable-smoke.mjs
```

`runtime-contract.py` exercises a 40-worker team, bounded concurrency, parent wakeup,
Monitor exit delivery without model polling, stop races, duplicate events, budgets,
worktree isolation and restart behavior against a protocol fixture.
The browser test checks the production React bundle in headless Chrome with an isolated
HTTP server and SQLite database. It covers complaints, agent chats, sidebar actions,
context, limits, a lost creation response, and command monitoring. The model protocol
is a fixture; this test does not call a live model.

The optional live check uses the configured account and model:

```bash
python3 tests/runtime-live.py --run
```

It requires one lead, two reviewers, one successful command watch and all three
completion events delivered to the lead. The test prints its evidence directory.

See [UI and tool evidence](UI-AND-TOOLS.md) for the interface decisions and Codex tool checks.

## Desktop, time, and local costs

The [desktop host](desktop/README.md) provides native file and folder dialogs,
Finder actions, external links, and optional notifications. Closing its window
preserves the backend and agent work. Existing SQLite chats use the same state
directory. Canvas positions remain browser-profile data.

The terminal panel has a searchable session list without a fixed session count.
It retains the latest 1,048,576 characters per user shell in SQLite. Reopening the UI
restores that text. A backend restart marks old sessions ended; it does not
recreate their processes. Input with uncertain delivery pauses until the user
reconnects and never retries the same keystrokes automatically.

[Time awareness](TIME-AWARENESS.md) uses the native Codex clock plus durable
message acceptance times. New timestamps do not rewrite earlier input.

Account limits show Codex before Spark, the remaining allowance, reset countdowns,
and exact local reset dates. Earned reset credits show their expiration dates.
Applying one requires a user confirmation. A retry retains the same account,
credit, and server idempotency key, including after an uncertain response.
Cost estimates come from the installed CodexBar CLI.
The display shows today and the last 30 days across local Codex logs. These are
API-rate estimates, not ChatGPT subscription charges. Missing prices or unknown
history coverage remain visible. The backend caches scans for 15 minutes.
