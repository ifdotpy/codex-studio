---
name: codex-agents
description: Coordinate authorized Codex delegation with native agent tools or durable app-server workers. Use for parallel agent work, reviewer workers, separate Codex processes, worker waves, goal budgets, monitoring, and steering.
---

# Codex worker orchestration

Use the host's native agent tools for bounded work within the current session.
Use `codex app-server` for a separate process, durable workers, or a host without native delegation.
Delegate only when the user or applicable instructions authorize delegation.
This skill does not itself require extra workers for every task.

For app-server mode, use the bundled scripts. Do not copy their logic into a project.
The protocol uses JSON Lines on stdio, without Content-Length framing.
Keep runtime state outside the skill checkout.
The scripts require Node.js and Python 3.9 or later on macOS or Linux.

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
- `CODEX_HOME` selects the Codex login (default `~/.codex`). Canonical install: `~/.agents/skills/codex-agents`.
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

## Local canvas and group chat

Run `scripts/codex-canvas` and open `http://127.0.0.1:4620`.
Use `--port PORT` to select another port. No package install is necessary.
The server uses Python's standard library and listens only on the local machine.
Keep its terminal session alive while the canvas is in use.

The canvas reads app-server waves from the same state directory as `luna`.
Native agents in a host conversation are not part of this feed.
Drag cards to move agents. Use Shift + click to select members for a group.
The panel shows recent messages, tool calls, results, run identities, and resource claims.
The transcript view has explicit size limits. It does not expose internal reasoning records.

Group chats persist in `canvas.sqlite3` inside the state directory.
Canvas positions stay in browser storage on this device.
A user message to a group uses `codex-steer` for each member.
Members stay tied to their exact run and thread. Replacement agents do not inherit old messages.
`queued` proves mailbox receipt, not agent acceptance or completion.
Failed deliveries remain visible. The canvas does not resend them automatically.

Agents can read and post shared replies with:

```bash
scripts/codex-chat list
scripts/codex-chat read GROUP_ID
scripts/codex-chat post GROUP_ID "Result or question" --owner "$CODEX_BOARD_OWNER"
```

Only a current group member can post as an agent.
The owner identifies a local worker, not an authenticated remote user.
Peer posts do not start new turns. Read the shared chat before coordination decisions.
The canvas does not create agents or replace the wave launcher's lifecycle commands.

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
| `codex-canvas` | Local web canvas, live transcripts, group chat, and resource board |
| `codex-chat` | List group chats, read messages, or post a member reply |

`luna say` writes through `codex-steer`; other commands only read existing state.
For `show`, `tail`, and `say`, use `--wave NAME` when worker names repeat.
Ambiguous names fail instead of selecting the newest wave.
Each worker uses its own launcher PID and run log.
An unfinished worker whose launcher ended is `abandoned`.
The board marks a known owner's claim `STALE` when its launcher is gone.
Unknown owners remain claimed; missing local history is not evidence that a resource is free.
`LUNA_HOME` remains an explicit legacy alias for the state directory, below `CODEX_AGENTS_STATE_DIR` in precedence.

## Script verification

Run from the skill directory:

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
These development dependencies are not necessary to use the canvas.
The client check uses a synthetic Document Object Model (DOM) and a local fixture server.
It does not prove browser appearance.
