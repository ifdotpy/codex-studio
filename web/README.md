# Codex Studio web client

React and TypeScript components, built with Vite. Mantine provides controls,
menus, dialogs, drawers, and the shared theme. Lucide provides icons. The Python server owns agents,
SQLite, message delivery, command monitors, and the complaint book.

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
- `src/components/Canvas.tsx`: the shared agent graph and stored positions.
- `src/components/ComplaintBook.tsx`: complaints, lead decisions, and user submissions.
- `src/components/Usage.tsx`: context usage, compaction count, and account limits.
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

## Live conversation

Managed conversations use `/api/transcript/stream`, a server-sent event stream.
The server sends a snapshot on connection and changed records after that.
It waits on a condition while idle and sends a heartbeat every 15 seconds.
Fast notifications are coalesced with an 80 ms delay between frames.
These UI updates do not call the model.

The client reconnects with a fresh snapshot. It uses the transcript GET endpoint
as a fallback during connection loss. Agent rooms and the team list retain their
existing refresh intervals.

Assistant text appears in complete paragraphs. Open fenced code blocks wait for
the closing fence. Item completion, turn completion, interruption, and stop reveal
the remaining text. Earlier paragraph elements stay mounted as new text arrives.

`AgentPhase.tsx` shows the phase from runtime events. It does not display private
reasoning. `Activity.tsx` shows commands, inputs, outputs, exit codes, and durations.
Raw events remain available in a separate disclosure. Long payloads wrap and
scroll inside their cards.

`tests/live-chat-ui.mjs` injects app-server notifications into an isolated runtime.
It tests the real HTTP stream and React interface without model inference.

## Orchestration workspace

The **Work** button opens the shared work board, changes, attention inbox, search,
plans, checkpoints, tools, profiles, rules, and resource leases. The main screen
keeps the conversation and worker list. Canvas remains the shared graph.

Work items have an owner, dependencies, submitted evidence, and an acceptance
record. A completed agent turn does not accept a work item. The lead must inspect
the changes and checks before acceptance. Only acceptance unblocks dependencies.

The composer supports file upload, paste, and drop. Each message accepts eight
files, up to 20 MiB per file. Images enter Codex as local images. Other files enter
as explicit file references. HTML previews cannot run scripts or load remote files.
Attachment drafts survive reloads. Failed sends retain the draft and attachments.

Choose **Queue** for the next turn or **Steer** for the current turn. Queue entries
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

Agents create user tasks with `orchestration_user_task`. The compact list appears
above the composer. **Work > Your tasks** shows tasks across all teams, with
search, owner and status filters, and history.

Check a task to send its result to the requesting agent. An optional note can
include a result or link. The task enters review. The agent can accept it or
return it with a reason. Only its requesting agent or team lead can change it.
Repeated requests do not send duplicate events. An explicit Stop prevents an
automatic wake; the pending review returns when the agent resumes.

Completed `mermaid` fences render diagrams. `html` fences and raw HTML blocks
render isolated static HTML, CSS, and SVG. Each preview keeps its source, copy,
and download controls. Scripts, external resources, and navigation are disabled.
An invalid diagram shows its error. Mermaid loads only when a diagram is present.

## Direct controls and quotes

The navigation bar opens common workspace sections directly. Agents create
monitors; the background panel shows their controls. Account capacity appears below
the composer. The account panel groups percentages and reset times by limit.

Select part of a message and choose **Quote selection**, or press Alt+Shift+Q.
Each quote appends to the current draft. You can add several excerpts from the
same message and write comments between them. The message quote button uses
its selected excerpt, or the full message when no excerpt is selected.
