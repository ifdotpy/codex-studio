# Codex Studio web client

React and TypeScript components, built with Vite. Mantine provides controls,
menus, dialogs, drawers, and the shared theme. Lucide provides icons. The Python server owns agents,
SQLite, message delivery, and command monitors.

## Run

From this directory:

```bash
npm ci
npm run build
../scripts/codex-canvas
```

Open <http://127.0.0.1:4620>. The server serves `dist/`.
Build output is local and ignored by Git. Rebuild it after a frontend change.
The server reports a missing build instead of serving an older client.

## Develop

Keep the Python server on port 4620. Start Vite in another terminal:

```bash
npm run dev
```

Vite proxies `/api` to the local Python server. `npm run build` checks TypeScript
and creates the production bundle. `npm test` builds that bundle and checks it in
a separate headless Chrome process against an isolated server and database.
Set `CHROME_BIN` if Chrome uses another executable path.

## Source

- `src/App.tsx`: screen selection, drafts, conversation actions, imports, and team panel.
- `src/components/Sidebar.tsx`: lead and agent tabs, search, previews, unread state, inline names, and deletion.
- `src/components/Conversation.tsx`: Markdown messages, agent bubbles, composer, and questions.
- `src/components/TurnHistory.tsx`: collapsible results with recorded terminal outcomes.
- `src/components/ConversationResults.tsx`: linked files, images, saved patches, and previews.
- `src/components/WorkerOverview.tsx`: worker assignments, reports, and team counts.
- `src/components/Requests.tsx`: active questions, deferral, and answer history.
- `src/components/Dictation.tsx`: saved recordings and explicit transcript insertion.
- `src/components/TeamChats.tsx`: messages in order by recipient and team channel.
- `src/components/UserMessages.tsx`: user requests, replies, and review decisions.
- `src/components/Usage.tsx`: context usage, compaction count, and account limits.
- `src/components/Analytics.tsx`: response usage history, tool payload measurements, and export.
- `src/hooks.ts`: server snapshots and transcript updates.

The sidebar renders 60 rows initially and adds rows as the user scrolls.
Unread markers and drafts remain in browser storage. Chat names remain in SQLite.
Native tools and permissions remain in the Codex app-server.

## UI conventions

Use `src/theme.ts` for control defaults and theme tokens. Use Mantine components
for forms and overlays. Keep layout rules in `src/style.css`.

The main conversation uses an AI chat layout with unboxed assistant responses.
Only agent rooms use messenger avatars, timestamps, unread markers, and paired
message bubbles. The team panel and navigation become drawers on narrow screens.

New transcript records preserve input sources. The client presents orchestration
events as expandable activity, separate from user messages. Older records retain
their original display when source metadata is absent.

The browser checks use 40 workers and more than 60 rooms. They cover widths from
320 to 1920 pixels, long drafts, table overflow, focus return, and menu placement.
Screenshots use an isolated database. They do not represent live model results.

`useConversationScroll.ts` owns transcript position. Keep browser scroll anchoring
disabled on that viewport. Follow new content only while the reader stays at the
bottom. Preserve the visible paragraph during content and composer size changes.
Explicit navigation must record its new position before the next React render.

Keep live message nodes and their order when a turn ends. Collapse that turn only
on user action. Keep button geometry stable during requests.
Retain confirmed same-chat data during refresh. Discard older responses after a
newer request or stream update. Never retain data across a different chat or account.

The conversation-motion, composer-stability, snapshot-order, and team-motion
browser checks measure these transitions against the production bundle.

## Live conversation

Send delivers after active tool calls. Desktop Enter does the same.
**Queue after turn** holds the message until the current turn ends.
Shift+Enter adds a line. Tab and Shift+Tab move the keyboard focus.
An idle agent starts a new turn with either send action.

Managed conversations use `/api/transcript/stream`, a server-sent event stream.
The server sends a snapshot on connection and changed records after that.
It waits on a condition while idle and sends a heartbeat every 15 seconds.
Fast notifications are coalesced with an 80 ms delay between frames.
These UI updates do not call the model.

The client shows bounded cached history when a managed chat is reopened.
Fresh events replace it. A delayed initial stream falls back to HTTP after 200 ms.
Cached history never supplies the current agent status.

The client reconnects with a fresh snapshot. It uses the transcript GET endpoint
as a fallback during connection loss. Agent rooms and the team list retain their
existing refresh intervals.

Assistant text and incomplete code appear as they arrive. Complete sentences use
a short fade. Earlier text nodes stay mounted as new text arrives.

`AgentPhase.tsx` shows the phase from runtime events. It does not display private
reasoning. `Activity.tsx` shows commands, inputs, outputs, exit codes, and durations.
Raw events remain available in a separate disclosure. Long payloads wrap and
scroll inside their cards.

`tests/live-chat-ui.mjs` injects app-server notifications into an isolated runtime.
It tests the real HTTP stream and React interface without model inference.

## Orchestration workspace

The **Work** button opens the work board, changes, Messages, search, the agent plan,
checkpoints, tools, profiles, rules, and resource leases. The main screen keeps the
conversation and worker list. Canvas and the saved-plan editor are removed.
**Plan** displays native steps and explanation. Changes go through the agent chat.

**Messages** opens user requests and agent conversations. **For you** shows user
requests, questions, and replies. **Team** shows orchestrator conversations, the
team channel, and private subagent chats. Only the orchestrator contacts the user.
Subagents send requests to the orchestrator, which decides whether to forward them.
Message replies retain their request identity after a lost response and drawer closure.

Work items have an owner, dependencies, submitted evidence, and an acceptance
record. A completed agent turn does not accept a work item. The lead must inspect
the changes and checks before acceptance. Only acceptance unblocks dependencies.

The composer supports file upload, paste, and drop. Each message accepts eight
files, up to 20 MiB per file. Images enter Codex as local images. Other files enter
as explicit file references. HTML previews cannot run scripts or load remote files.
Attachment drafts survive reloads. Rejected sends retain the draft and attachments.
Offline messages remain in the device outbox.

Use **Queue after turn** for the next turn or **Send** after active tools. Queue entries
can be edited, moved first, or cancelled. Uncertain delivery never silently retries
as a new turn. Conversation branches include the complete selected native turn.

Checkpoints retain the workspace files and visible conversation references. Restore
requires an idle, isolated worker worktree and an unchanged preview. A recovery
checkpoint preserves the state before each restore. Ignored files are outside the
Git checkpoint. Restores do not change application databases or external services.

Rules wait for a schedule, file change, or agent event without a model call. Script
checks wake the agent only after exit zero; a final JSON line with
`{"wakeAgent":false}` suppresses that wake. Pausing or deleting a rule cancels its
active check. Stop and restart never silently repeat a command with an unknown result.

Interactive monitors support input, interrupt, EOF, resize, and saved log downloads.
Native Codex command sessions use **Send via agent**, because the public app-server
cannot directly write to a native `unified_exec` session. Managed monitors use the
connection's `command/exec` controls directly.

The tool panel lists managed tool definitions, discovered skills and MCP tools,
and observed native calls. Codex does not expose its complete native tool inventory
through this API. Existing threads can use new managed tools through the documented
`orchestration_send` workspace route. Worker profiles set the model, role, effort,
and instructions. They do not grant additional permissions.

Search covers stored conversations, tool output, work, plans, complaints, and agent
rooms. Source previews open records beyond the chat's recent-message window.
Model search respects room membership and cannot search another agent's private
transcript. The resource panel uses the existing `codex-board` registry.

Desktop alerts require an explicit browser permission. The inbox remains available
when that permission is absent. Browser notification delivery depends on system settings.

Backend regression commands run from the repository root:

```bash
python3 -B tests/runtime-contract.py
python3 -B tests/canvas-contract.py
python3 -B tests/user-tasks-contract.py
python3 -B tests/workspace-contract.py
python3 -B tests/workspace-races.py
python3 -B tests/workspace-protocol.py
python3 -B tests/workspace-native-turn.py
```

The last two checks use the installed Codex binary with temporary state. The native
turn check supplies a local Responses fixture and blocks external traffic.

The background panel includes all active processes and the latest 100 completed
monitors and tool records. Older monitor records and logs remain on disk. Expected
negative script checks remain in rule history; they do not fill the attention inbox.

## User tasks and previews

The orchestrator creates user tasks with `orchestration_user_task`. The compact list appears
above the composer. **Work > Your tasks** shows tasks across all teams, with
search, owner and status filters, and history.

Select **Send for review** to send a task result to the orchestrator. An optional note can
include a result or link. The task enters review. The agent can accept it or
return it with a reason. Only its team lead can change it.
Repeated requests do not send duplicate events. An explicit Stop prevents an
automatic wake; the pending review returns when the agent resumes.

Completed `mermaid` fences render diagrams. `html` fences and raw HTML blocks
render isolated static HTML, CSS, and SVG. Each preview keeps its source, copy,
and download controls. Scripts, external resources, and navigation are disabled.
An invalid diagram shows its error. Mermaid loads only when a diagram is present.

## Direct controls and quotes

The chat menu opens the workspace sections. Agents create monitors.
The background panel shows their controls. Settings contains accounts, models,
permissions, and the theme. The account panel groups usage and reset times by limit.
A fresh account response with available quota clears the current limit warning.
Studio keeps that result for the same error after reopening the chat.
The historical error remains. Recovery does not send or retry a message.

Select part of a message and choose **Quote selection**, or press Alt+Shift+Q.
Each quote appends to the current draft. You can add several excerpts from the
same message and write comments between them. The message quote button uses
its selected excerpt, or the full message when no excerpt is selected.
