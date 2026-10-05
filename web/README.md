# Codex Studio web client

## Change Contract

The web client owns Studio's renderer, presentation, and user interaction. It
reads application and agent state through the server HTTP API; the server owns
persistent state, execution, and native provider access. Keep permission and
request identity rules intact for actions that write state. Match checks to the
changed interface, and use the focused composer check with
`npm run test:skill-autocomplete` for skill completion changes.

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

## Component workbench

Storybook runs inside this package and reads stories colocated with components
under `src/`. Start the local workbench with `npm run storybook`, create its
static bundle with `npm run build-storybook`, and run story interaction tests in
headless Chromium with `CHROME_BIN=/path/to/chromium npm run test:storybook`.
The Storybook config and package scripts disable telemetry. The Vitest addon
runs each story's play function in a local browser; no hosted Storybook service
is used. Keep story props typed from the component or a small typed harness, and
include representative keyboard and disabled/loading states for interactive UI.

`npm run test:prompt-composer` remains the production composer regression check.
It verifies the actual prompt input and draft subscription path independently
from the isolated Storybook stories.

## Source

- `src/App.tsx`: screen selection, drafts, conversation actions, imports, and team panel.
- `src/components/Sidebar.tsx`: lead and agent tabs, search, previews, unread state, inline names, and deletion.
- `src/components/Conversation.tsx`: Markdown messages, agent bubbles, composer, and questions.
- `src/components/prompt-composer/`: production prompt input, isolated checks, benchmark entrypoints, and the [prompt composer guide](src/components/prompt-composer/README.md).
- `src/components/TurnHistory.tsx`: collapsible results with recorded terminal outcomes.
- `src/components/ConversationResults.tsx`: linked files, images, saved patches, and previews.
- `src/components/WorkerOverview.tsx`: worker assignments, reports, and team counts.
- `src/components/Requests.tsx`: active questions, deferral, and answer history.
- `src/components/Dictation.tsx`: saved recordings and explicit transcript insertion.
- `src/components/Usage.tsx`: context usage, compaction count, and account limits.
- `src/components/Analytics.tsx`: response usage history, tool payload measurements, and export.
- `src/hooks.ts`: server snapshots and transcript updates.

Shared UI views live beside their callers in the feature folders above. Typed presentational components
have colocated `*.stories.tsx` files; the application imports those same
components. Their callers keep request handling, draft storage, scrolling, and
focus behavior.

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
New chats use the server's default model without a model catalog request.
The server's creation response opens the chat before the full list refreshes.
An older list cannot remove that confirmed chat while synchronization catches up.
The client retains the confirmed chat across reloads until the list includes it.

The renderer sends its generated OpenAPI schema hash with API requests and
protocol-3 stream connections. Once the server hash is ready, API responses
carry it; ordinary reads during startup remain available without the header.
A hash-bearing protocol-3 stream starts with an `api-schema` handshake; a
mismatching stream closes after that event without a subscription. Hashless
non-renderer clients retain the earlier protocol-3 event sequence. If hashes
differ, the page keeps the loaded transcript and local composer draft visible,
then pauses stream, draft replication, outbox delivery, and send actions until
the renderer updates.
Protocol-3 event payloads no longer use generated per-event runtime validators;
workspace, epoch, revision, and ordering semantics remain enforced.
Desktop windows skip service workers, so Update reloads the renderer directly;
a mismatch that remains after reload requires rebuilding Studio.
The server caches its computed API schema hash outside the checkout at
`$XDG_CACHE_HOME/codex-studio-api-schema/hash-v1.json` (by default
`~/.cache/codex-studio-api-schema/hash-v1.json` on Linux and
`~/Library/Caches/codex-studio-api-schema/hash-v1.json` on macOS). Deleting this
file is safe; Studio recomputes it on the next start. A background verification
also repairs a valid but incorrect cached value.

Managed conversations use the workspace sync projection. One tab holds the
exclusive browser lock and owns `/api/sync/stream?protocol=3`, sharing
typed resource invalidations with other tabs in the same browser profile. Tabs
pull only the projections they use. If cross-tab coordination is unavailable,
each tab can open its own protocol-3 stream. Transcript updates use scoped
`transcript:<id>` pulls; the server does not expose a transcript stream or a
generation-poll endpoint. Hash-bearing protocol-3 connections begin with an
`api-schema` event; a mismatching connection receives only that handshake and
closes without subscribing. Hashless connections keep the prior event sequence.
During the one-time rollout, a pre-gate tab can hold the stream lock while its
hashless channel messages are ignored, leaving a new tab degraded until the old
tab is closed or reloaded.
Mutating HTTP requests with a hash mismatch receive the marked reload-required
response.
These UI updates do not call the model.

Draft recovery journal and local record writes happen synchronously on each
edit. RxDB coalesces upstream draft pushes according to the wait policy in
[`src/sync/client.ts`](src/sync/client.ts), while resume and the journal rescan
continue to recover pending local changes.

The client shows bounded cached history when a managed chat is reopened.
Fresh scoped pulls replace it. If no local projection is available after 200 ms,
the client makes one transcript read to show the missing or unavailable result.
Cached history never supplies the current agent status.

The lock-owning tab reconnects after network or page resume and forces a refresh so
peers do not rely on notices missed while asleep. A tab that becomes hidden
releases the stream lock so a visible peer can take over.
Cached transcript pages remain available offline and historical paging keeps
using the transcript page endpoint. Agent rooms and the team list retain their
existing refresh intervals.

In plain terms, eight coordinated tabs do not each phone the server. One tab
listens for updates and tells the other seven which small piece changed. The
other tabs then ask for only that piece. If tabs cannot coordinate, each uses a
protocol-3 resource stream and still pulls only its own subscribed data.

Assistant text and incomplete code appear as they arrive. Complete sentences use
a short fade. Earlier text nodes stay mounted as new text arrives.

`AgentPhase.tsx` shows the phase from runtime events. It does not display private
reasoning. `Activity.tsx` shows commands, inputs, outputs, exit codes, and durations.
Raw events remain available in a separate disclosure. Long payloads wrap and
scroll inside their cards.
The current turn shows tool calls by default, including completed commands while
the agent thinks. Manual collapse remains available. Turn completion preserves
the visible calls; older commands stay hidden when history is reopened.

`tests/client/chat/live-chat-ui.spec.mjs` injects app-server notifications into an isolated runtime.
It tests the real HTTP stream and React interface without model inference.

## Orchestration workspace

Desktop alerts request only the workspace inbox. They do not read checkpoint history.
Each alert check finishes before the next check starts.
Workspace reads use one committed database snapshot without the agent execution lock.

The **Work** button opens the work board, changes, Messages, search, the agent plan,
checkpoints, tools, profiles, and rules. The main screen keeps the
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

In managed chats, type `$` at a token boundary to browse installed skills. Filter
by name; the first match is selected automatically, and Up/Down changes the
selection and scrolls the selected option into view without moving input focus.
The reusable `ComposerAutocomplete` adapter uses Mantine Combobox for the popup
and option selection. Enter or Tab inserts the option, and Escape closes the list. The focused
browser check uses a delayed `/api/skills` fixture to cover
typing responsiveness, keyboard behavior, caret placement, and chat-scope changes.
The current Claude bridge does not expose skill listing; when listing is unavailable,
the composer reports it while manually typed `$text` remains ordinary prompt text.

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
transcript.

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

## Connection recovery

After a disconnect or server restart, **Check connection** reads the affected
thread and its exact turn from Codex. A confirmed terminal result replaces the
saved connection error. The transcript retains the earlier error and final text.
This check does not resume work, send a prompt, repeat a tool, or release queued
messages. An active thread, missing turn, failed read, or changed chat keeps the
recorded state. A successful read stores the check result even when the turn's
outcome remains unknown. The notice then says that the previous turn needs review;
it does not claim that Codex is still disconnected. Failed reads do not create
this receipt. Review an unconfirmed outcome before sending another instruction.

Targeted checks:

```sh
python3 tests/connection-recovery-contract.py
npm --prefix web run test:browser -- disconnect-recovery-ui.spec.mjs
```
