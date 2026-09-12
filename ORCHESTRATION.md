# Managed Codex teams

Use the [Electron desktop app](desktop/README.md), or build the [React interface](web/README.md), start `scripts/codex-canvas`, and open <http://127.0.0.1:4620>.
Use the new-chat action inside a project. The server creates an empty lead conversation immediately.
If the current lead chat is empty, it reuses that chat and preserves the draft.
Write the task in the conversation. The lead generates its title with `orchestration_title`.
Creation itself does not call the model. A repeated creation request returns the same chat.
Leads carry an explicit `isLead` marker in SQLite. Only Astra and Sol can be leads.
Use the command tools to inspect standalone workers and registered sessions.
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

Studio enables `features.context_management.experimental_mode` in native thread
configuration. This keeps the opt-in out of the shared user configuration, where
older ChatGPT builds reject nested feature tables. Account eligibility still applies.

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
Worker cards show the original assignment and the last report from a completed
turn. Open each excerpt to read more. Reports do not imply acceptance by the lead.
The team summary counts workers who are active, need your answer, or have finished.
Deferred questions remain unanswered and leave the attention count.

Assistant text stays visible in chronological order, outside tool disclosures.
Only consecutive tool calls share a group. Groups start closed and retain the
user's choice across chat switches and reloads. A new paragraph splits a group.
Failed and interrupted turns retain their outcomes. Missing outcomes do not
imply success. **Results** opens linked
files, images, previews, and recorded patches from the loaded history. New turns
retain separate patch records; a patch preview does not fetch the current Git diff.

Use **Defer** to keep a question without its active card or reminder. A required
answer still blocks its agent. **Deferred questions** restores the card.
**Question history** shows past answers and delivery status for the current team.
Secret values remain hidden there. An uncertain native reply cannot be sent again
under the same request identity.

The microphone button opens [recoverable dictation](desktop/README.md#recoverable-dictation).
Audio stays in this browser profile and chat. Transcription runs through macOS Speech;
**Insert into message** adds the text to the draft without sending it.
Use **Team** to inspect worker status and open a worker chat.
Use **Messages** for user requests, conversations with the orchestrator, team
broadcasts, and private worker chats. The main chat draft stays open.
Only the orchestrator contacts the user. A subagent sends its request to the
orchestrator, which decides whether to resolve it or forward it.

The harness injects `codex-orchestrator` or `codex-subagent` from the server's
`isLead` identity. Native thread instructions contain the selected skill.
Versioned turn context supplies it to existing threads and after changes or compaction.
The native permission system still owns tool approval.

The conversation header provides context compaction, review, and team stop.
The **Projects** sidebar groups lead chats by their actual working directory.
Use **+** beside Projects to add a folder, including a project with no chats.
Use **+** beside a folder to start a chat there. An empty current chat is reused.
Folder registration lives in SQLite. Removing an empty project from the sidebar does not delete files.
Each folder initially shows five chats. **Show more** expands the list, and search includes hidden chats.
Folder collapse preferences stay in browser storage. Agent conversations appear in **Messages**.
Use the sidebar row menu to rename or delete a lead or agent chat.
Manual names take priority over an automatic lead title. Studio stores both manual
and generated names in SQLite and copies them to Codex with `thread/name/set`.
The copy uses the session's account and thread. It starts no model turn. A native
failure preserves the Studio name and retries with backoff. Drafts sync when they
have a native thread; existing sessions and transferred threads sync as well.
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
The bottom terminal panel shows only user shells. **Background** lists commands,
monitors, and other tools for the current chat and its descendants. It has no
cross-team filter. Its history keeps up to 100 completed records of each type
within that chat, plus every active record.

Inbox, workspace agent choices, and attention counts use the same lead tree.
Two chats remain separate even when they use the same account or project folder.
Switching chats clears open request forms, task details, and pending view responses.
`/api/workspace?agent=<id>` returns that agent's lead tree. An omitted `agent`
retains the global API view for explicit administrative callers.

**Changes** shows the selected agent's latest reported native turn diff. It does
not use the shared working-tree diff. If no report exists, the view is empty.
The timestamp and turn identify the report; comments retain that turn identity.
File previews read current files, which can have changed since the report.
`/api/changes?agent=<id>&scope=chat` supplies this view. The legacy scope-less
API still reads the working tree for callers that explicitly need that state.
An older server without the chat scope shows an update notice instead of an
unscoped diff.

### Files and images

File links open a preview inside Studio. Markdown has rendered and source views.
Links and images inside a Markdown file resolve relative to that file.
CSV and TSV have table and source views. JSON has formatted and source views.
Studio also previews text, PDF, static HTML, raster images, SVG, audio and video.
Audio and video support depends on the system codecs.
Image controls provide fit, actual size and zoom.
Expanded image tool cards show supported image results. A click opens the image viewer.
Closed tool cards do not load local image files.

On macOS, file actions use the default application, Finder or Quick Look.
Quick Look can preview additional formats that macOS supports, including office documents.
The native process resolves file metadata through `/api/file-info` before each action.
Open refuses executable files. Quick Look and Finder remain available.
Save a copy uses the system Save dialog and preserves the original bytes.
Cancel leaves the destination unchanged. A failed save shows an error.
On iPhone, Save a copy uses the system share sheet when file sharing is available.
Other browsers use an explicit download fallback.

The existing file API limits inline previews to 20 MiB.
Native file actions remain available when that limit prevents a preview.
Table previews show at most 200 rows and 100 columns.
Their text parser stops after 1,048,576 UTF-16 code units.
Source and Save a copy retain the complete file within the file API limit.

### Agent progress file

Each managed conversation displays its agent's plain `PROGRESS.md` between the
transcript and composer. The file path is `<stateDir>/progress/<agentId>/PROGRESS.md`.
Studio supplies the exact path in runtime instructions and through
`orchestration_context topic=panel`, together with the bundled
[progress guide](.agents/skills/codex-workspace/references/panel.md).
The file remains outside Git. Agents that share a working directory have separate
files. A project's own `PROGRESS.md` does not control this display.

Agents read and edit the file with ordinary file tools. Content must be UTF-8
Markdown and at most 128 KiB. The display has a maximum height of 150px.
Its current width, font and available height determine fit. Studio uses passive
Markdown and checks the complete rendered revision before display. Overflow or
unsupported content produces a compact notice with access to the original file.
The panel does not scroll, cut content or shrink the font.
It does not execute scripts or provide agent callbacks. An update does not require command output or a model
turn. Existing read-only permissions remain unchanged.

The file persists across restarts. Provisioning creates an empty file only if it
is absent; it never overwrites agent content. An empty or missing file clears the
display. Read errors, invalid UTF-8, and oversized files produce a visible error.
A read failure does not present an old revision as current. A later successful
read checks the new revision. File reads occur outside the global runtime lock.
Atomic file replacement is supported; the next read observes the new revision.

The selected visible conversation refreshes `GET /api/panel?agent=<id>` in the
background. The response contains `format=markdown`, `markdown`, `path`,
`revision`, `updated`, `exists`, and `error`. Hidden and offline views pause these
reads. Old responses from another chat cannot replace the current display.
File content stays outside shared snapshots. File changes do not wake agents.

The visible client reports measured dimensions to `POST /api/panel/layout`.
The server writes revision-specific, expiring feedback to `PROGRESS.layout.json`
beside the source file. Separate clients retain separate results. Agents use the
check command in their runtime instructions after edits. The
[progress guide](.agents/skills/codex-workspace/references/panel.md) defines this workflow.
An absent or stale measurement means unmeasured, not success.

The old `orchestration_panel` and `orchestration_panel_feed` schemas are no longer
advertised. New calls, including the workspace fallback, return migration
instructions without writing legacy panel state. Stored panels, callback receipts,
and feed records remain intact. Already accepted callbacks and active work can
finish. No old panel data is copied into or replaces `PROGRESS.md`.
See the [legacy feed note](.agents/skills/codex-workspace/references/panel-feed.md).

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
| `orchestration_request` | List, recover, or cancel your own durable tool requests. |

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
Completed managed tools retain their exact execution receipts even when the
caller loses the response. Response write failures record request identities
in `runtime-errors.log`, without tool input or output.

Give every spawn batch a stable `request_id`. Studio commits the complete worker
batch, initial messages, and result receipt in one SQLite transaction. Reusing
the same ID and payload returns that receipt across calls and turns. Changed
payloads are rejected. A model-catalog failure occurs before worker creation.
Verified catalog data is cached for five minutes per account and connection.
An uncached metadata response has a five-second wait; its late result can still
fill the cache without another native request.

Use `orchestration_request` with `action=get` and `request_id` after a lost reply.
It accepts the stable spawn ID, native call ID, or returned request ID. `list`
returns at most 50 recent request summaries. Outcomes have these meanings:

| Outcome | Evidence |
|---|---|
| `pending` | The request is queued or executing. |
| `applied` | The operation has a successful receipt. This does not prove worker completion. |
| `not_applied` | Queued cancellation or atomic spawn failure proves no mutation. |
| `unknown` | Evidence cannot yet determine whether the operation applied. |

`action=cancel` prevents queued requests from executing. For running requests it
records cancellation intent and returns promptly. It does not kill an operation
that may have committed. Spawn checks this intent again before its transaction.
Missing and old failed receipts remain unknown. Restart does not replay mutations.
Tool intake, start, and completion timestamps remain available for diagnosis.
If the operation committed before its final tool reply, recovery also returns
`operationApplied`, `operationResult`, and `evidenceSource=operation_receipt`.
These fields prove the saved operation, not completion of the enclosing tool or
the next command in a sequential script. The tool can still remain `pending`.

Recovery reads and coordination use separate execution pools from model startup,
slow tools, and monitor waits. Native response reads continue while ordered
notification callbacks run. A command completion waits for its preceding output
callbacks before the runtime records its final result.
Work-board actions and result submissions use the coordination pool, including
the workspace fallback for older threads.

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
profile or sandbox. The runtime drains output without model calls. It stores the
complete log and retains a 12,000-character tail for the live view. The final event includes
exit code, command status, output tail, log path and total output bytes.
The default command timeout is one hour; the maximum is 24 hours.
An acknowledgement timeout does not finish a command. Cancellation retains the
active workspace reservation until the command exits. The interface shows
**Waiting for exit** during this interval. Closing terminal input blocks further
input while its acknowledgement is pending. An unknown close is not retried.
Shell configuration reads also retain late responses before command submission.
Monitor waits use separate owned threads, so they do not occupy the shared
queue for agent tools and messages.

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

Each project selects one default account for new chats. This setting adds no
folder restrictions. Agents can use files and skills outside their project under
the native permission settings. Native permission grants do not answer agent questions.

## Messages to the user and orchestrator

Use `orchestration_complaint action=submit` for a message that needs a recorded response.
The technical tool name remains for compatibility. Responsibility comes from the author:

- Subagent request: the orchestrator responds, under **To orchestrator**.
- Orchestrator message: the user responds, under **For you**.
- Historical user complaint: the selected orchestrator responds.

Workers cannot close complaints. A lead cannot respond to or close their own
complaint. The user cannot take over a complaint assigned to a lead through the
user response endpoint. Only the assigned recipient can record a response.

A worker complaint delivers its full text, author, and id to the lead and starts
a turn, including after a final answer. The lead calls `action=respond` with a
concrete action or reason and `in_progress`, `resolved`, or `declined`. The lead
must not poll the book or routinely call `action=read`.

The orchestrator decides whether to handle a subagent request or send its own message to the user.
Messages to the user do not start an orchestrator turn. The user records a response in the message dialog. That response notifies
the reporting lead. A stopped lead keeps the record but does not resume.

Unanswered complaints assigned to the lead remain in that lead's turn context.
Three consecutive turns that ignore presented complaints stop automatic lead
continuation with a visible error. Complaints assigned to the user do not block
lead completion. A recorded next step counts as a response; `in_progress` entries
remain visible under **All messages**.

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

After a server restart, queued input and verified interrupted work can resume.
Continuation requires the original permission and authoritative native evidence.
An unacknowledged delivery stays uncertain and is not replayed. Command watches
without a definitive result remain lost. See [restart recovery](docs/restart-recovery.md)
for the persistence contract and exact limits.

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

### Native ownership

Prefer native Codex primitives when they remove a Studio mechanism and preserve its behavior.
Experimental APIs are acceptable. Keep one execution path for each operation.
Preserve account scope, workspace reservations, team limits, and delivery recovery before removing a Studio mechanism.

Stopping a native background command uses `thread/backgroundTerminals/list` and
`thread/backgroundTerminals/terminate`. Studio checks the item and process identities
in the selected agent's account and thread. Stop does not send model input.
The command completion event supplies its exit code. Terminal input still uses the
agent's `write_stdin` tool and preserves whitespace, including the submitted newline.

User terminals use `process/spawn`, `process/writeStdin`, `process/resizePty`,
and `process/kill` on a dedicated local app-server connection. This connection
starts no model turns and does not follow account switches. Native Codex owns
PTY allocation and byte transport. Studio keeps bounded history and durable
create/input receipts. A timeout does not permit replay; late replies reconcile
the same receipt. Connection loss ends affected terminals with an unknown exit
code. It never recreates their commands. A new terminal starts a new connection.

Codex 0.153.4 kills the shell's process group. Interactive jobs can use other
groups in the same session. Studio therefore retains session cleanup, using the
PID recorded by its child bootstrap. The bootstrap no longer allocates a PTY.
This migration removes direct PTY I/O but does not reduce the total adapter size.
See [terminal checks](tests/terminals-contract.py).

Voice uses Codex 0.153.4's native `thread/realtime/start` with v3 WebRTC
and ChatGPT sign-in. Studio enables `features.realtime_conversation` for leads.
The RPC acknowledgement is not an SDP answer: `thread/realtime/sdp` completes
the browser handshake. Session receipts retain the original account, connection,
thread and offer. A lost acknowledgement never starts another session.
`thread/realtime/item/completed` supplies canonical transcript records. Native
Core handles incoming delegation and sends lead responses back to voice.
There is no separate courier send gate. Ending voice disables transcript-tail
flushing and does not interrupt the lead's turn. Stop-before-start retains a
cancellation tombstone. Connection loss preserves the transcript without replay.
`appendSpeech` provides speakable context, not an exact-playback receipt.
The browser never converts a native transcript into a second Studio message.
The old REST voice and TTS paths are removed. Legacy transcripts remain readable.
See [voice lifecycle checks](tests/native-voice-contract.py).

Codex 0.153.4's native user queue persists across app-server restarts. However,
two `thread/queue/add` calls with the same `clientUserMessageId` create two entries.
Its idle hook starts queued turns without consulting Studio's workspace or team scheduler.
Retain the Studio queue and receipts until a replacement preserves these controls.
See [native integration checks](tests/native-primitives-integration.py).

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
python3 tests/harness-response-contract.py
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
directory. Historical Canvas positions remain browser-profile data.

The terminal panel has a searchable session list without a fixed session count.
It retains complete future output in SQLite, with a 1,048,576-character live view.
Reopening the UI restores that view. Download saved output reads the archive. A backend restart marks old sessions ended; it does not
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

## Model context and output budgets

These rules apply at the managed dynamic-tool boundary. The HTTP workspace views,
task evidence, chat messages, and execution receipts remain in SQLite. Native
Codex tools such as `exec_command` retain Codex's own output controls.

- `orchestration_task action=list` returns one `items` array of brief records.
  Filter with `owner` and `state`. `limit` defaults to 20 and cannot exceed 50.
  Continue with `nextCursor`. A changed list rejects an old cursor explicitly.
  `action=get` reads one task's description, dependencies, and latest evidence.
  `action=reject` takes the review reason and required corrections in `result`.
  It returns the task to `ready` and sends `work_decision` to its owner atomically.
  The owner uses that event as its next instruction and submits revised evidence.
  The event queues an owner with automatic continuation enabled. It does not
  resume an explicitly stopped agent or clear a native failure hold.
  `action=history` pages its results and decisions. Mutations return brief receipts;
  the operation receipt retains the complete task and its evidence.
- `orchestration_peers` returns a paged team directory and readable room identities.
  `scope=all` discovers other teams without their private chat contents.
  `orchestration_status` returns compact team and monitor states. Pass the returned
  `revision` as `since_revision` to receive changes and removals. The server retains
  eight snapshots per caller. An expired revision returns a full compact snapshot
  with `reset=true`. Read profiles, schemas, and monitor details with
  `orchestration_context`. Directory reads no longer attach message or log tails.
- When a managed tool's encoded text exceeds 16,000 UTF-8 bytes, the model receives
  an excerpt, outcome, operation identities, and `outputRef`. The full response is
  saved before this projection. `orchestration_read` accepts `output_ref`, a Unicode
  character `offset`, and an optional `contains` search. Each page contains at most
  3,000 UTF-8 bytes. Only the original caller can read that reference. A missing or
  pending result does not authorize a mutation retry. Images and immutable time
  metadata remain in the response. Completed UI tool entries recover the saved
  text within the existing transcript allowance.
- `orchestration_message importance=progress` opts routine progress into a one-second
  batch window, checked by the scheduler. The model receives the latest such update
  per sender, room, and `progress_key` in that batch. `progress_version` must
  increase for each update of that task or topic. Missing or ambiguous revisions
  retain every update. Earlier updates remain in chat history with
  their original message and event IDs. Questions, blockers, results, unclassified
  messages, and user input bypass this delay. Urgent input takes priority over a
  page of pending progress. Completion notifications remain distinct events.
- Agent plans, role skills, and required requests carry content versions. Only confirmed
  event delivery establishes a known version. An unchanged complaint retains its
  required-response reminder and ID. The server supplies full context again after
  thread replacement or observed compaction. Clearing a plan invalidates its older
  steps. Historical saved-plan text remains stored but is not part of model context.
  `orchestration_context` reads the current full context on demand.
- `orchestration_monitor wake_on=failure` retains successful results in the UI and
  suppresses their model notification. Failures still notify the owner. Use this
  option only if success requires no further agent work. The default, `exit`, still
  notifies on every exit. Record verified status in the per-agent `PROGRESS.md`
  file with ordinary file tools. Its display updates without model turns.

Existing native threads keep their original tool schemas. They can use new fields
through `orchestration_send` with `agent_id="workspace"` and a JSON `text` value:
`{"tool":"orchestration_task","arguments":{"action":"get","task_id":"…"}}`.
This route also supports `orchestration_read`, `orchestration_context`,
`orchestration_status`, `orchestration_peers`, `orchestration_message`, and
`orchestration_monitor`. It preserves the caller and original request identity.
New threads expose the current schemas directly. No reasoning, Fast mode, model,
account admission, or permission settings change as part of these budgets.

## Worker archive and recovery

`orchestration_agent_manage` belongs to the lead. It manages only descendants in
that lead's team. Actions are `inspect`, `recover`, `archive`, `restore`, and
`list_archived`. Inspection returns bounded blocker IDs. Recovery reads the exact
native turn through the existing reconciler; a failed read leaves its outcome unknown.

Archive requires a reason. It hides a worker through the existing tombstone filter
and stores a separate archive receipt with its actor, time, and epoch. No history,
worktree, file, native thread, or resource claim is deleted. Active work and uncertain
receipts block archive. Archive children before their parent. Restore checks the
receipt, parent, and team limit; it leaves automatic continuation disabled. A later
user deletion or stop invalidates that archive receipt. Repeated calls are safe.

The shared Studio skill documents the workspace bridge for existing native threads.
Tests: `tests/agent-management-contract.py`.
