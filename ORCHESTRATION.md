# Managed Codex teams

Start `scripts/codex-canvas` and open <http://127.0.0.1:4620>.
Select **New chat**. The server creates an empty lead conversation immediately.
Write the task in the conversation. The lead generates its title with `orchestration_title`.
Creation itself does not call the model. A repeated creation request returns the same chat.
Leads carry an explicit `isLead` marker in SQLite. Only Astra and Sol can be leads.
Standalone workers and registered sessions stay in **Other sessions**.
The project defaults to the previous lead's directory, `CODEX_CANVAS_CWD`, or the server's current directory.
Before the first message, select the folder below the composer to change the project.
Choose Astra or Sol in the chat header. Model changes require an idle turn.

The conversation shows Markdown, tool results, queued messages and agent questions.
Enter sends a message. Shift + Enter inserts a new line.
Workers appear on the right. Select a worker to read its conversation, then select **Lead** to return.
On a narrow screen, **Team** opens the worker panel.
Select **Canvas** for the selected team's graph. Drag nodes to move them, drag the background to pan, and scroll to zoom.
The canvas has one control, **Fit**. Parent links come from the runtime.
The browser keeps existing canvas positions and message drafts when views change.
Manual graph selection modes, chat wiring, the minimap, and duplicate zoom controls were removed.
Existing shared chats and sessions remain accessible from **Other sessions**; their command-line tools remain available.

The conversation menu provides context compaction, review, command monitoring, and team stop.
`/monitor <command>` submits a command directly. Results appear below the worker list.
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
| `orchestration_title` | Set the conversation title from the task. Only a lead can call this tool. |
| `orchestration_interrupt` | Stop a descendant and its descendants. A follow-up can resume them. |
| `orchestration_spawn` | Create up to 64 workers in one request. Each worker has a task, role, optional model and effort. |
| `orchestration_send` | Queue a follow-up for a descendant. An explicit follow-up can resume a stopped descendant. |
| `orchestration_status` | Read team status and command watches for a decision. |
| `orchestration_monitor` | Start a command watch. Deliver one result when the command exits. |
| `orchestration_cancel_monitor` | Cancel a command watch. |

After delegation, the lead can finish its turn. The runtime queues each child
result and starts the next lead turn, including after a final answer.
Events that arrive during a turn wait for that turn to finish. Up to 32 events
are combined in one input. Repeated completion notifications share an event id.
A worker with pending children or command watches stays in the waiting state.
Its parent receives a result after that work settles. A reported result still needs review.

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

The inherited `never` approval policy does not add an approval step. Other policies
require approval for a model-created Monitor command in the conversation.
A command submitted directly through the user interface is an explicit user action.
The same sandbox still applies. This runtime does not bypass sandbox restrictions.

## Capacity and work ownership

Each team defaults to 8 concurrent agents and a maximum of 64 total agents,
including its lead. `codex-control configure` supports 1 to 64 concurrent agents and 1 to 256
agents in the team. `CODEX_CANVAS_CONCURRENCY` sets the server-wide cap, default 16,
maximum 64. Lead turns have priority when a slot becomes free.
Lowering a limit does not interrupt existing turns.

Implementers receive separate Git worktrees under the parent's repository:
`.worktrees/codex-agents/<agent-id>`, on branch `codex-agent/<agent-id>`.
They start from committed HEAD. Parent changes that are not committed are absent.
Reviewers use the parent's directory with a read-only sandbox.
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
python3 tests/runtime-contract.py
python3 tests/canvas-contract.py
node tests/portable-smoke.mjs
cd web
npm --prefix . ci
npm --prefix . test
npm --prefix . run format:check
```

`runtime-contract.py` exercises a 40-worker team, bounded concurrency, parent wakeup,
Monitor exit delivery without model polling, stop races, duplicate events, budgets,
worktree isolation and restart behavior against a protocol fixture.
The DOM test covers lead creation, a lost creation response, command monitoring and
team stop through the HTTP interface. It is not a browser-render test.

The optional live check uses the configured account and model:

```bash
python3 tests/runtime-live.py --run
```

It requires one lead, two reviewers, one successful command watch and all three
completion events delivered to the lead. The test prints its evidence directory.

See [UI and tool evidence](UI-AND-TOOLS.md) for the interface decisions and Codex tool checks.
