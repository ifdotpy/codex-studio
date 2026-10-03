# Codex Studio

## Change Contract

The repository pre-commit hook checks staged JavaScript, TypeScript, CSS, HTML,
Markdown, YAML, and JSON. It reads the Git index, leaves staged and unstaged
files untouched, and requires locally installed tools. Oxlint and Oxfmt
configuration belongs to [`.oxlintrc.json`](.oxlintrc.json) and
[`.oxfmtrc.json`](.oxfmtrc.json); `npm run test:pre-commit` exercises the hook
against staged-content fixtures.

A desktop workspace for one lead agent and many workers. The application uses
Codex app-server for model sessions, tools, and permissions. It adds durable
orchestration, agent messages, command monitors, and user replies.

This repository owns the application, command tools, tests, and runtime prompts.
Its project-local [codex-workspace skill](.agents/skills/codex-workspace/SKILL.md)
describes managed tools and application workflows. No external skill repository
is required. Standard Codex CLI delegation belongs to the independent native
agent skill, not this application skill.

In the left sidebar, open a project's menu and select **New team** to group
independent chats. Select at least two chats from that project. Each chat keeps
its own tasks, history, and subagents. Team members can discover each other and
send explicit private messages. Broadcasts and automatic results stay within
each chat's original agent tree. The team menu supports edits and dissolution.
Drag a chat onto a team to join or move between teams. Drop it onto the project
name to leave. A team dissolves when fewer than two chats remain.
Dissolution preserves the chats and saved messages but removes peer access.

Two Studio installations can also share an approved room over Tailscale. This
feature is off by default; see the [peer federation operator guide](docs/peer-federation.md).

Select **New shared chat** to create one conversation with two agents.
Choose a project and each participant's account and model before creation.
The sidebar shows one chat. No existing chats or peer team are required.
Both agents receive the shared messages and reply in sequence. Each agent keeps
its own account, model, and context. Independent work stays outside this chat.
Select **Both agents** for one reply each, or select one participant.
**Discuss (4 replies)** gives each agent two turns. **Reply next** selects the
next speaker. **Stop** ends the exchange and interrupts only its current reply.
A new message replaces the remaining reply order after the current reply ends.
The exchange waits for answers to agent questions. Personal chat instructions
must use the queue while that agent gives a shared reply.
Existing two-member teams can also open a shared chat from their menu.
That history stays with its original members when the team changes.

Managed workers can use a different provider from their parent. The default is
`gpt-6-luna` with `high` reasoning. The orchestrator selects each worker's model
and reasoning level through `orchestration_spawn`. Studio selects an available
account for that model, preferring the parent's account, then the application
default. `account_key` selects a specific account. Model and account validation
finish before any worker in the batch is created. A team transfer moves existing
workers whose provider matches the destination. Workers on another provider stay
on their current account. New workers use the team's destination account and
provider-compatible defaults.

## Setup

Requirements: Python 3.11 or later, Node.js 22.15 or later, and the signed-in Codex CLI.
The desktop package currently targets macOS on Apple silicon.

From this repository:

```sh
npm ci
git config --local core.hooksPath .githooks
python3 scripts/install-cli.py
npm --prefix web ci
npm --prefix web run build
npm --prefix desktop ci
(cd scripts/claude_bridge && npm --prefix . ci --ignore-scripts --omit=optional)
npm --prefix desktop start
```

The root `npm ci` installs Oxlint and Oxfmt. Set `core.hooksPath` once per clone
to activate the mandatory staged-content checks. The hook does not download
dependencies; install them before committing.
For full-repository audits, run `npm run lint:all` and
`npm run format:check:all`. Append paths after `--` to check selected files.

Claude Code is also supported through the installed CLI and its Claude subscription.
Run `claude auth login`, then select **Claude Code** in the account menu for a new
chat. Use **Find existing accounts** in Accounts if it does not appear. Studio
reads the model list from Claude Code and keeps credentials in its native store.
It uses the Claude Agent SDK with the installed executable, as in
[T3 Code](https://github.com/pingdotgg/t3code). No Anthropic API key is required.

Claude chats support live Steer, an explicit message queue, native tools,
background tasks, questions, action approvals, and Studio agent tools. Limits shows
five-hour, weekly, and model-specific subscription windows. The account menu shows
the remaining weekly allowance.

Chat settings includes Claude permission modes, extended thinking, automatic
compaction, native commands and skills, and context rollback. Model, effort, and
fast-mode controls use the native model catalog. The model picker lists one row
per model with its display name, a **(default)** tag for the catalog default, a
**(current)** tag for the model the chat uses now, and the catalog description
in a second column. Hidden catalog models stay hidden. A proposed plan waits for a
separate implementation request. Rollback preserves the original saved history.
Commands and rollback retain their request identities after a lost response.

Accounts includes Claude profiles with separate executable and configuration paths,
custom models, launch options, and compaction limits. Sign in with the native CLI
before adding a profile. Active sessions keep their current connection; profile
changes apply when that connection is idle.
For an expired Claude session, select **Sign in again** on its account in Accounts.
Studio opens the native sign-in flow for that profile. Open the sign-in link,
then paste the confirmation code in Studio. Use the displayed account email.
Studio checks the account identity before it reports success. It does not resend
failed chat messages.

For an expired Codex session, select **Sign in to Codex** in the chat notice
or **Sign in again** in Accounts. The notice also identifies failed workers
on a different account from their lead. Use the displayed account email.
Studio uses the same profile and waits for native sign-in completion. A saved
token alone does not confirm success. Existing chats and worker assignments
keep their account. Sign-in does not resend tasks or restart active agents.

Codex model settings include a separate **Daybreak** switch. Studio checks the
selected account's native model grants before it enables the mode. A change
during an active turn applies to the next turn. Subagent defaults have their
own switch. Account transfers preserve the mode only when the destination
supports it; a provider change clears it. Native `review/start` cannot select
Daybreak, so use a review task in the chat when this mode is required.

Subagent defaults also has a review model and reasoning setting. A native review
uses the call's model and effort first, then the team review default, then the
caller's model and effort. Studio sets the reviewer thread's `review_model`, so
an account `review_model` does not override the selected model.

Studio checks installed Codex executables every minute, including `CODEX_BIN`,
PATH, and the copy bundled with ChatGPT. It checks the protocol schema and runs
an isolated native smoke test without account credentials or model requests.
The newest compatible version becomes an immutable bundle under the state
directory. The bundle includes `codex` and `codex-code-mode-host`, with separate
file hashes and one bundle identity. Studio publishes it only after both
executables pass their checks. A failed check preserves the last approved version.
Studio replaces an account app-server only after local and native checks confirm
that no turns, commands, queues, approvals, or unresolved requests remain.
Active work continues on its existing process. Accounts shows versions, pending
updates, and rejection reasons. Studio does not download or install Codex packages.

Studio also compares Codex and Claude CLI versions with its
[tested reference versions](scripts/codex_provider_versions.py). Older versions
produce an advisory in Accounts and the chat's **Warnings** dialog: features may
work poorly or fail, but you can continue at your own risk. These references are
not a latest-release check. Version age alone does not block use; native protocol
and safety checks still apply.

Studio reads Codex model metadata through a short-lived process of the same
approved executable. The reader uses the selected account's native credentials
and starts no model tasks. Model grants remain cached for five minutes.

Claude Code uses its native permission rules for its tools. Studio command
monitors use the installed Codex command executor with the Studio sandbox policy.
This executor does not start model sessions or use account credentials. Monitors
support output, terminal input, resize, cancellation, and timeouts. Voice remains
unavailable for Claude. Files remain native attachments or file references.

Existing chats can move between Codex and Claude accounts. Selecting an account
for a lead starts a team transfer. The chat keeps its identity and displayed
history. Idle members rebind immediately, including worker defaults.
Their native history transfers automatically in the background after account
selection, without sending a message or starting a model reply. The transfer keeps its
original identity when the action is repeated or the page reloads. Active turns are
interrupted with the destination account named, then move as soon as they stop.
Workers on another provider stay on their current accounts with the reason shown
in transfer progress. Queued events keep their receipts and deliver once after
the native history move. A transfer involving Claude creates a destination session
with recent text and a path to the complete saved history. It preserves the
original native session and does not replay old commands. A provider change
selects the destination model and clears queued settings for the previous
provider. Codex account transfers continue to use native history copies.
Changing the Subagents account starts a subagent-only transfer and updates the
account used by future workers while the lead stays on its current account.
See [the parity checks](docs/verification/2026-09-22-claude-parity.md) for evidence
and the tested reference revision.

The command installer creates links in `~/.local/bin`. Add that directory to PATH
if your shell does not include it. Use `--bin-dir` for another directory.
It refuses to replace unrelated files or links. To update links from the old
checkout, pass `--replace-from /path/to/previous-checkout`.

See [the supervisor operator notes](docs/supervisor-operator.md) for verified
per-handle cleanup and recovery guidance for older supervisor generations.

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

For a shareable process snapshot, run `python3 scripts/codex-diagnostics` while
Studio runs. The command calls the read-only `/api/diagnostics` endpoint. It
reports the Studio process tree, resident memory and CPU by process kind,
loaded Codex threads per connected account, live Claude queries, queue sizes,
short runtime lock samples, and host memory. Account names and process command
lines stay out of the output. The snapshot reads process and native state only
when requested. Resident memory totals can count shared pages more than once.
Idle Codex subscriptions release after 15 minutes and native threads can take
about one more minute to unload. Idle Claude queries close after 15 minutes.
Native background tasks and unresolved input keep their sessions open. New work
resumes the saved thread.

Set `CODEX_RUNTIME_LOCK_METRICS=1` before you start Studio to include
`runtimeLockOperations` by source call site. Each row reports count and total wait and hold time.
Percentiles and maxima use the latest 512 acquisitions at that call site.
Without this flag, Studio uses its normal runtime lock and reports no per-call-site data.

SQLite scopes above one second retain their owner, start location, wait stack,
duration, and completion result. Read `sqliteContention.slowTransactions` in
the diagnostics response. The existing update worker saves this evidence in
`diagnostics/sqlite-transactions.json` under the state directory. A process
restart preserves the last nonempty snapshot in
`diagnostics/sqlite-transactions.previous.json`. These files contain no SQL
text or message contents. The coverage timestamp identifies when owner records
start. Earlier transaction totals cannot identify a past owner.

Closing the window preserves the server and its active work. Backend source
identity remains available to diagnostics without a persistent notice in chats.
Reviewed live patches apply in the background without a backend restart.
See [live updates](docs/live-updates.md) for publication and verification.

The team view measures worker worktree disk use in the background. It shows each
worker and the team total. The default warning limit is 100 GiB across all
worker worktrees. Set `CODEX_WORKTREE_DISK_LIMIT_BYTES` before Studio starts to
change the limit. Set it to `0` to disable the warning. On Apple File System
(APFS), the measure counts bytes that are private to each file. Other file
systems use allocated blocks. The UI shows the measure for each worktree and
the total. On other file systems, the sum can exceed physical disk use.

The packaged macOS application restores its backend after login or a process
failure. Saved input and verified interrupted work recover automatically.
See [restart recovery](docs/restart-recovery.md) for the exact behavior and limits.

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
histories through its existing API. One shared event stream tells visible windows
when entity state, drafts, or open transcripts need an update.
While Studio is visible, it prepares unarchived chats and the selected team's
agent chats in the background. It updates these saved histories before selection.
The current chat loads first. One background history loads at a time.
This leaves connections available for messages and other active requests.
A ready history appears immediately on selection, including its saved scroll position.
After a network change or a return to Studio, sync replaces the old connection.
A cached workspace cannot send drafts to a different workspace before verification.

Run `npm --prefix web run test:mobile` for the mobile regression suite.
Run `BROWSER=webkit npm --prefix web run test:mobile` for the WebKit suite, where supported.
The performance fixture emulates a 390-pixel phone. Chromium uses Fast 4G and
four-times CPU slowdown. WebKit throttles asset responses only. Its local fixture
API and CPU are not throttled.
These checks do not replace a test on a physical iPhone.

Run `npm --prefix web run test:responsiveness` for the draft and transcript checks.
The mobile fixture measures 1,500 agents, 2,700 entity rows, and 1,500-message
transcripts. See [the measurement report](docs/verification/2026-09-12-ui-responsiveness.md)
for earlier results and limits. See [the mobile performance report](docs/verification/2026-10-01-mobile-performance.md)
for current startup, sync, transcript, and memory measurements.

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

| Path                            | Contents                                                 |
| ------------------------------- | -------------------------------------------------------- |
| `web/`                          | React, TypeScript, Mantine, Vite                         |
| `desktop/`                      | Electron host, native bridge, package tools              |
| [`scripts/`](scripts/README.md) | Python server, command tools, and component-local checks |
| `prompts/`                      | Runtime worker instructions                              |
| `tests/`                        | Backend, protocol, browser, and process contracts        |

- [Orchestration](ORCHESTRATION.md): managed agents, messages, monitors, and state.
- [Command guide](CLI.md): waves, steering, and CLI usage.
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

Agents can exchange messages and read agent conversations within their agent tree.
An agent tree contains one lead and its workers, with the same `rootId`.
Independent chats in a sidebar team can also exchange private messages.
Each chat keeps its own work, subagents, and automatic results.
You can still read historical conversations between teams.

Use the subagent control in the chat header to set this chat's maximum parallel
subagents. Zero selects Single agent; a positive value selects Multi agent.
The main agent does not use a subagent slot. Excess work waits in the queue.
Lowering the limit preserves current executions; zero prevents further worker
executions until you raise it. Current executions can still report their results.
The limit remains saved after a reload or restart. The orchestrator receives the
limit and adjusts delegation to it. See [subagent parallelism](ORCHESTRATION.md#subagent-parallelism)
for counting, recovery, and compatibility.
The main agent's model selector includes every available model from the selected account's app-server catalog.

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
Requests for user action appear as messages. Reply in the same conversation.
Saved user tasks retain their original dates and replies as messages.
Each message shows its send date and time. There is no separate user task board.

The harness reads the repository's `codex-orchestrator` or `codex-subagent` skill
from the server's `isLead` identity. It adds that skill to the native thread
instructions. Existing threads receive it on their next turn. The versioned turn
context repeats it after a skill change or context compaction.
Workers ask their lead to contact the user. Workers cannot send spoken responses to the user.

**Team** shows subagent status and opens subagent chats.
**Back to main agent** returns to the main agent.
The sidebar and Team panel can collapse at any window width.

**Studio settings** is available without opening a chat. Its Accounts tab opens
the same account manager as the chat menu: add accounts, view saved accounts,
remove them, and choose the application default. Its Appearance and Hotkeys tabs group per-browser appearance,
text size, transcript width, message author icons, and sidebar shortcut preferences.
Author icons are hidden by default. Chat settings continues to select the account
for the current conversation and manage its project, model, and permissions.

Prompt history is in the chat header; open **Conversation tools** when space is
limited. Context usage, session cost, and account limits sit below the composer.
Agent progress stays visible above it. An orange **Warnings** icon appears in the
header when the current account or chat has notices. It opens their full details
in a dialog; errors that block work remain visible in the conversation.
On narrow screens, **Chat actions** also opens Chat settings and Team. The agent
mode switch is inside Chat settings on mobile. Mobile input text stays at least
16px; larger selected text sizes apply normally.
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
`CODEX_AGENTS_STATE_DIR` and `CODEX_HOME` retain their meanings.
Historical Canvas positions remain in each browser profile. Message drafts remain available.
Closing Electron leaves the backend active.

## Checks

Tests are separated into client and server suites. Keep unit tests beside the
source module in its feature folder. Browser scenarios that cross component
boundaries live under `tests/client/<feature>/`.

```sh
npm run test:client:unit
npm run test:client:browser
npm run test:server:list
npm run test:server
npm run test:mutation
npm run test:pre-commit
```

See the [testing guide](docs/testing.md) for discovery, focused runs, browser
setup, native checks, and mutation reports. Run checks for the changed area.
Fixtures use isolated state; they must not attach to a live user backend or
send paid model requests. Desktop checks use hidden Electron windows.

Inside a project, drag teams, folders, and loose chats to set their order.
Drop near a row edge to change the order. Alt + Up or Down also works.
Project order and item order stay saved in this browser.
Select **Compact project** from the project menu to show peer team chats,
pinned chats, active chats, unread chats, and chats active in the last 24 hours.
The rule uses the chat's last update time. **Show all N** restores all chats.
The compact setting stays saved per project. Search includes hidden chats.

Drop a peer team chat in the center of another lead chat to make it a subagent.
The confirmation names both chats and lists the effects. Both agent trees must
be idle. Active turns, queued input, permission requests, commands, monitors,
workspace operations, account transfers, and shared exchanges block the move.
Unresolved tool requests also block the move.
The chat keeps its native thread, history, account, model, execution settings,
and directory. Its workers keep their parent links and use the destination root.
The complete source task board moves, with its owners, dependencies, and results.
Plans and annotations keep their chat identities. Requests for the user move to
the destination lead. The destination's plan and worker defaults stay in use.
The destination's concurrency, agent limit, and token limit apply to the moved tree.
The converted chat pauses. Its watches pause and require an explicit resume.
Worker watches and saved command and monitor records keep their agent identities.
Progress files keep their paths and contents. The next turn receives the new role.

The source leaves its peer team. A team with fewer than two members dissolves.
Saved peer rooms remain available to the user. Former peers lose agent access.
The old broadcast becomes a private room for the original tree and keeps its
messages. New broadcasts use the destination's broadcast room.
The action saves one receipt with the exact request body. A retry with the same
request ID returns that receipt. A different body with that ID is refused.
