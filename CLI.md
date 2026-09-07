# Codex command guide

Use the host's native agent tools for bounded work within the current session.
Use `codex app-server` for a separate process, durable workers, or a host without native delegation.
Delegate only when the user or applicable instructions authorize delegation.
This skill does not itself require extra workers for every task.

Run examples from this project root, or use the installed commands without the `scripts/` prefix.
The protocol uses JSON Lines on stdio, without Content-Length framing.
Keep runtime state outside the project checkout.
The scripts require Node.js and Python 3.11 or later on macOS or Linux.

## Managed teams

For a lead that must resume after worker or command completion, use the managed
canvas runtime. Run `scripts/codex-canvas`, open the local page, and create a lead.
The lead uses `orchestration_spawn` and `orchestration_monitor`. The server owns
queues, concurrency limits, command waits and automatic parent continuation.
Use `scripts/codex-control` for terminal access to the same runtime.
Recover an uncertain tool result with `codex-control requests AGENT_ID REQUEST_ID`.
Omit `REQUEST_ID` to list recent requests. This command only reads receipts.
An older local server can also use this command without a restart. If the route
returns 404, the CLI reads saved results from that server's exact SQLite database
in read-only mode. Missing results and failed legacy receipts remain `unknown`.
This fallback is unavailable for remote servers and does not bypass HTTP errors.
An operation-only receipt exposes `operationApplied` and `operationResult` while
the enclosing tool outcome remains unknown. It does not prove that a subsequent
statement in the caller's script executed.
The equivalent route is `GET /api/tool-requests?agent=AGENT_ID&request_id=REQUEST_ID`.
Authenticated `POST /api/tool-requests/cancel` accepts `agent` and `request_id`.
See [Managed Codex teams](ORCHESTRATION.md) for limits, permissions and recovery.

## Mode

| Mode | Use |
|---|---|
| One worker | One bounded bug, module, review, or measurement |
| Implementer + reviewer | One change needing independent review |
| Wave | Independent tasks, separate files and worktrees |

- Do not split sequential reasoning across workers; do not assign one file to two workers.
- The orchestrator owns boundaries, steering, review, merges.
- The top-level orchestrator owns app-server waves and their lifecycle.
- Native agents follow the host's actual delegation and completion capabilities.

The remaining sections describe app-server mode unless they explicitly mention native agents.

## Paths and protocol

- State dir: `$CODEX_AGENTS_STATE_DIR`, else `$XDG_STATE_HOME/codex-agents`, else `~/.local/state/codex-agents`. Scripts create it.
- Board dir: `$CODEX_BOARD_STATE_DIR`, else the state directory's `board/` subdirectory. Use the same directory for every client of a shared resource.
- `CODEX_HOME` selects the Codex login (default `~/.codex`). Install commands with `python3 scripts/install-cli.py`.
- Codex state, board, and profile directories must resolve outside `.claude`. Scripts reject these paths and do not discover legacy Claude job directories.
- Move historical state only with explicit authorization. Preserve claims and messages. Never create a second board for an occupied resource during migration.
- Schemas move; before depending on a protocol field: `codex --version; codex app-server generate-json-schema --experimental --out /tmp/codex-schema` (drop `--experimental` if it fails; read `v2`). Goal methods need `capabilities.experimentalApi: true` at `initialize`.

## Model

Honor an explicit model request. Native agents inherit the parent model unless the task requires an authorized override.
`scripts/codex-models` lists available app-server models and efforts.
The app-server wave default is `CODEX_MODEL=gpt-5.6-luna`, `CODEX_EFFORT=max`.
Check availability instead of guessing from a local config file.

## Worktrees and briefs

The app-server launcher requires a separate worktree and branch per worker, with absolute paths.
It validates paths and claims worktrees across active waves.
Native read-only reviewers can inspect an existing checkout without a new worktree.
Native implementers need isolation only where their edits or outputs can collide.

Give each worker a bounded outcome, owned files, relevant context, and a completion check.
State the role: implementers can edit their scope, while reviewers remain read-only.
State commit, merge, and publication authority explicitly. A review task grants no write authority.
Supply shared-resource paths and peer exclusions when the worker needs them.
Treat a colleague's diagnosis as a hypothesis to check, not an implementation command.
Ask for concrete evidence and remaining limitations, not a mandatory report template.

Use the host's real completion notifications or a bounded watcher for long work.
Keep the orchestrator available for user updates. Do not assume that every host needs manual polling.
If progress depends on an external condition, record the resume condition and give monitoring to an available owner.
Do not report an unchanged monitored condition as a new failure.

## Task file and launch

Write `codex-tasks.<wave>.json` in the state dir (or set `CODEX_TASKS`). Unique names and paths; default role `implementer`.

```json
[{"name": "parser-fix", "role": "implementer",
  "cwd": "/abs/worktree", "branch": "codex/parser-fix",
  "objective": "Correct the parser error and prove it.",
  "prompt": "Correct one parser error. Add a regression test. Run the parser gate. Commit."}]
```

```bash
export CODEX_MODEL=gpt-5.6-luna CODEX_EFFORT=max CODEX_BUDGET=1000000
scripts/codex-daemon start --wave parser    # detached, portable (macOS/Linux), logs to codex-daemon.<wave>.log
scripts/codex-daemon status --wave parser
```

- The daemon inherits the caller's environment; it refuses to start without the task file (no fallback) and surfaces an immediate launcher death with its cause.
- The app-server process owns its threads: launcher down = steering down. Check the launcher PID file, not `pgrep -f`. An OS service manager (see `wave.plist.template`) is optional for reboot survival; give services an explicit `PATH`.
- Approval and sandbox are inherited; verify the effective policy first — a worker that cannot approve a needed command stalls. Optional: `CODEX_APPROVAL_POLICY=never`, `CODEX_SANDBOX=read-only|workspace-write|danger-full-access`. Reviewers always `read-only`. `CODEX_BUDGET` is a hard per-worker token stop.
- One era of scripts per wave: launcher, watcher, report, steer from the same checkout. Mixed copies fail silently or refuse valid state.

## Monitor

```bash
scripts/codex-watch --wave parser
scripts/codex-report --wave parser [worker --answers]
```

- The watcher pins the first run id, waits for the launch-complete marker, and exits nonzero on failed, blocked, interrupted, abandoned, paused, stalled, or replaced runs.
- Reports are bounded; never pull raw event logs or rollouts into orchestrator context.
- Hand-rolled sentinel (when `codex-watch` is unavailable): copy terminal values from one real record first. `goalStatus` terminal values are `complete`, `budgetLimited`, `blocked` — not `completed`. Safe condition: goal present and not `active`, or turn `failed`/`interrupted`.
- Arm the replacement watcher before stopping the old one; a gap is a missed event.

## Canvas, chat nodes, and creator connections

Build the interface with `npm ci && npm run build` in `web/`.
Run `scripts/codex-canvas` and open `http://127.0.0.1:4620`.
Use `--port PORT` to select another port. The server uses Python's standard library and listens only on the local machine.
Keep its terminal session alive while the canvas is in use.

Agents and chats are separate nodes. Do not use a wave or a visual container as a chat.
An agent can connect to several chats. Chat connections define membership.
Creator connections record who started each subagent. They do not grant chat membership.
Do not infer a creator from a shared wave, directory, model, or resource claim.

The default view is a lead conversation. Select **New chat** to create it without a form.
Only managed records with `isLead=true` appear in the chat list. Leads use Astra or Sol.
Select **Canvas** for all teams and registered sessions. Click a node to read its transcript.
Drag a node to move it. Drag the background to pan. Scroll to zoom. **Fit** shows the team.
Canvas positions stay in browser storage on this device.
Use **Other sessions** for legacy agents and shared chats. Use the chat commands below to change membership.

### App-server creators

The launcher records `orchestratorId` and `orchestratorName` with each worker.
`CODEX_ORCHESTRATOR_ID` takes precedence over the host's `CODEX_THREAD_ID`.
A task can supply its own `orchestratorId` when a different agent owns that task.
Set `CODEX_ORCHESTRATOR_NAME` for a readable name.
Before a wave launch from a host that has no `CODEX_THREAD_ID`, supply the actual stable creator identity with `CODEX_ORCHESTRATOR_ID`.
No creator identity means no creator connection. Historical records remain unchanged.
A creator reference with no status record displays an unknown status, not the launcher's status.

### Native host agents

Native delegation needs an explicit graph record because the canvas does not receive the host's native tool events.
Before the first native spawn, register the orchestrator with its stable host identity.
After a successful spawn, register the returned child identity and the actual parent.
Do not register a child when its spawn fails.
Update the same record after host notifications change its status.
Only attach `--thread` when the host provides a real local Codex thread identity.

```bash
scripts/codex-graph agent --id HOST_PARENT_ID --name "Lead" --status running
scripts/codex-graph agent --id HOST_CHILD_ID --name "Parser" --parent HOST_PARENT_ID --status running
scripts/codex-graph agent --id HOST_CHILD_ID --name "Parser" --parent HOST_PARENT_ID --status completed
scripts/codex-graph list
```

Reuse the same parent and thread values on updates. They are part of the identity.
Register each actual parent before its children. A subagent that creates children also appears as an orchestrator.
These records report host observations. They do not create a process or prove process liveness.
Use the native host tools to send instructions to native agents. The web mailbox controls only app-server workers.

### Chat operations for the orchestrator and agents

Chats, connections, and messages persist in `canvas.sqlite3` inside the state directory.
The schema migration preserves existing chat records and history.
The same commands work while the web server is closed.
Use the exact agent IDs from `codex-graph list` for connections.

```bash
scripts/codex-chat create "Runtime discussion"
scripts/codex-chat connect CHAT_ID --agent AGENT_ID
scripts/codex-chat disconnect CHAT_ID --agent AGENT_ID
scripts/codex-chat list
scripts/codex-chat read CHAT_ID
scripts/codex-chat post CHAT_ID "Result or question" --owner "$CODEX_BOARD_OWNER"
scripts/codex-chat post CHAT_ID "Native agent reply" --agent HOST_CHILD_ID
```

Use `--id UUID` on create or post to reuse an operation identity after a lost response.
Only a connected agent can post as a member. Local identity labels are not remote authentication.
A user message to a chat uses `codex-steer` for its connected app-server workers.
Connections stay tied to exact run identities. Replacement workers do not inherit them.
`queued` proves mailbox receipt, not agent acceptance or completion.
Failed or uncertain deliveries stay visible. The canvas does not resend them automatically.
Peer posts do not start new turns. Read the shared chat before coordination decisions.

The transcript panel shows recent messages, tool calls, and results with explicit size limits.
It does not expose internal reasoning records.
The canvas creates managed leads and their workers. Legacy waves still use the launcher lifecycle commands.

## Steer a worker

```bash
scripts/codex-steer --wave parser parser-fix "Limit the change to the parser module."
```

- One message to the run inbox; `turn/steer` on an active turn, `turn/start` on a terminal one; deleted only after the RPC succeeds; failures back off into the dead-letter directory.
- Never start a second app-server to steer a thread it does not own.
- A steer makes the wave live again: re-read the status file immediately before any launcher stop; never stop on a report older than your last steer.

## Shared resources (board)

```bash
"$CODEX_BOARD" claim|renew|release product-build "$CODEX_BOARD_OWNER" ["reason"]
"$CODEX_BOARD" claim product-build "$CODEX_BOARD_OWNER" --wait [--timeout SECONDS]
"$CODEX_BOARD" show
"$CODEX_BOARD" takeover product-build "$CODEX_BOARD_OWNER" "old-wave:run-id:worker" "reason"
```

Implementers claim; reviewers only read. Claims never expire — `takeover` only after verifying the holder is gone. The board stores facts and claims, not design decisions. Set `CODEX_AGENTS_STATE_DIR` when running it outside a worker.

`claim --wait` queues the caller when the resource is held, instead of failing with `HELD`, and blocks in the same process until it is granted in FIFO order. `--timeout SECONDS` gives up after the wait, exit code 3, printing the current holder and the caller's queue position; without `--timeout` it waits indefinitely. A plain `claim` (no `--wait`) on a free resource still defers to a live queue: only the queue head may take it, everyone else gets `QUEUED resource n waiting, head=owner` and exit 1 (same code as `HELD`), so an existing plain-claim poll loop keeps working without a code change. `show` lists `QUEUE resource position owner pid waiting Ns` after the claims. A queued process that dies is dropped by the next claim/renew/takeover/release/show on that resource (liveness check: `kill(pid, 0)`, same as `luna`'s launcher check). Do not add a manual `release` for a dead waiter, it was never holding the claim. Poll interval: `CODEX_BOARD_POLL_MS` (default 500).

## Capacity and completion

- A capacity error rejects one turn; the launcher retries the same thread with backoff. Do not switch models mid-retry.
- Budget exhaustion is a hard stop — hence "commit each finished part."
- A terminal turn with an active goal is not a completed worker. Success = completed goal + completed turn. Read `turn.error` on every `turn/completed`.

## Review, merge, finish

- Reviewer gets the spec and the diff, never the implementer's reasoning trace. Blocking findings need file:line. `INSUFFICIENT_CONTEXT` is a valid result.
- Per branch: read the diff, read the test evidence, run the affected gate if it changes the merge decision, merge only a coherent change. A worker's success claim is not merge evidence.
- Before merging, personally inspect the diff and evidence when the user requests personal review.
- Finish after every worker is terminal and every accepted change has a commit and evidence.
- Release only the wave's own resource claims. Preserve worktrees until their work is integrated or handed off.
- Keep useful logs and evidence. Remove only disposable, verified wave-owned scratch files.
- Recheck status before `codex-daemon stop --wave <name>`: stopping interrupts active workers.

## Commands

| Command | Function |
|---|---|
| `codex-models` | Model ids and efforts |
| `codex-swarm.mjs` | The wave launcher |
| `codex-daemon` | Start/stop/status of a detached launcher |
| `codex-watch` | Exit-on-terminal watcher |
| `codex-report` | Bounded status and answers |
| `codex-steer` | Mailbox message to one worker |
| `codex-board` | File-locked resource claims |
| `codex-stop` | Verified launcher stop |
| `luna` | Explorer and mailbox: dashboard, `ls --all`, `show NAME`, `tail NAME`, `say NAME "text"`, `board`, `waves`, `watch` |
| `codex-canvas` | Lead conversations, global canvas, agent chats, approvals, and command monitors |
| `codex-chat` | Create chat nodes, connect members, read messages, or post replies |
| `codex-graph` | Register native agents and actual creator relationships |

`luna say` writes through `codex-steer`; other commands only read existing state.
For `show`, `tail`, and `say`, use `--wave NAME` when worker names repeat.
Ambiguous names fail instead of selecting the newest wave.
Each worker uses its own launcher PID and run log.
An unfinished worker whose launcher ended is `abandoned`.
The board marks a known owner's claim `STALE` when its launcher is gone.
Unknown owners remain claimed; missing local history is not evidence that a resource is free.
`LUNA_HOME` remains an explicit legacy alias for the state directory, below `CODEX_AGENTS_STATE_DIR` in precedence.

## Script verification

Run from the project root:

```bash
node tests/portable-smoke.mjs
node tests/state-contract-smoke.mjs
python3 -B tests/daemon-contract.py
node tests/sandbox-smoke.mjs
python3 -B tests/canvas-contract.py
```

The first three checks use fixtures and mocks; they do not call a model.
The sandbox check needs a local Codex binary and tests real sandboxed commands, without model inference.
Run these checks after changes to state schemas, paths, lifecycle, or message delivery.
Mock protocol tests do not prove compatibility with every app-server version.

For canvas client changes, run `npm --prefix . ci` and `npm --prefix . test` from `web/`.
Build the React client before starting the canvas server. The built client does not require a Node.js server.
The client checks use headless Chrome and a local fixture server.
They verify rendered behavior without model inference.
