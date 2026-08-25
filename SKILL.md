---
name: codex-agents
description: Orchestrate one or more Codex worker threads through codex app-server JSON-RPC. Use for Codex delegation, parallel worktrees, Luna worker waves, goal budgets, monitoring, steering, reviewer workers, or durable multi-agent runs from any agent harness.
---

# Codex worker orchestration

Run Codex workers from any harness over `codex app-server` (JSON Lines on stdio, no Content-Length framing). Use the bundled scripts; do not copy their logic into a project. Keep one skill checkout; state lives outside it.

## Mode

| Mode | Use |
|---|---|
| One worker | One bounded bug, module, review, or measurement |
| Implementer + reviewer | One change needing independent review |
| Wave | Independent tasks, separate files and worktrees |

- Do not split sequential reasoning across workers; do not assign one file to two workers.
- The orchestrator owns boundaries, steering, review, merges.
- Only the top-level orchestrator holds a wave. A nested subagent cannot wait, steer, or outlive its turn.

## Paths and protocol

- State dir: `$CODEX_AGENTS_STATE_DIR`, else `$XDG_STATE_HOME/codex-agents`, else `~/.local/state/codex-agents`. Scripts create it. Board lives in its `board/` subdirectory.
- `CODEX_HOME` selects the Codex login (default `~/.codex`). Canonical install: `~/.agents/skills/codex-agents`.
- Schemas move; before depending on a protocol field: `codex --version; codex app-server generate-json-schema --experimental --out /tmp/codex-schema` (drop `--experimental` if it fails; read `v2`). Goal methods need `capabilities.experimentalApi: true` at `initialize`.

## Model

`scripts/codex-models` lists ids and efforts. Wave default: `CODEX_MODEL=gpt-5.6-luna`, `CODEX_EFFORT=max`. Do not infer the model from a local config file.

## Worktrees and briefs

One worktree and branch per worker, absolute paths. Never the main checkout, never another worker's worktree. The launcher validates paths and claims worktrees across active waves.

Every prompt contains:

1. One deliverable, with an existing file or test as the pattern to imitate. Open-ended implementation burns the budget.
2. Owned files or module.
3. A gate that proves completion, and a baseline command to run first.
4. Role: `implementer` (commit each finished part) or `reviewer` (read-only worktree, no edits, no commits).
5. First paragraph, the wait rule: nothing wakes the worker; every wait is an active loop in slices of 400 s or less, one tool call per slice; long runs detached with the PID in a file.
6. Absolute paths for every shared resource (board, state, reference binaries) — workers cannot find harness paths by searching.
7. Live peers and untouchable worktrees, named.
8. Outcome, not mechanism. Numbers in a brief are context; a colleague's diagnosis is a hypothesis.
9. Closing FEEDBACK section: wrong/incomplete task statement; tools causing avoidable work; brief facts disproved; what shortens the next run. File each complaint in the project's incident register; a repeated complaint is an orchestrator defect.

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

## Steer

```bash
scripts/codex-steer --wave parser parser-fix "Limit the change to the parser module."
```

- One message to the run inbox; `turn/steer` on an active turn, `turn/start` on a terminal one; deleted only after the RPC succeeds; failures back off into the dead-letter directory.
- Never start a second app-server to steer a thread it does not own.
- A steer makes the wave live again: re-read the status file immediately before any launcher stop; never stop on a report older than your last steer.

## Shared resources (board)

```bash
"$CODEX_BOARD" claim|renew|release product-build "$CODEX_BOARD_OWNER" ["reason"]
"$CODEX_BOARD" show
"$CODEX_BOARD" takeover product-build "$CODEX_BOARD_OWNER" "old-wave:run-id:worker" "reason"
```

Implementers claim; reviewers only read. Claims never expire — `takeover` only after verifying the holder is gone. The board stores facts and claims, not design decisions. Set `CODEX_AGENTS_STATE_DIR` when running it outside a worker.

## Capacity and completion

- A capacity error rejects one turn; the launcher retries the same thread with backoff. Do not switch models mid-retry.
- Budget exhaustion is a hard stop — hence "commit each finished part."
- A terminal turn with an active goal is not a completed worker. Success = completed goal + completed turn. Read `turn.error` on every `turn/completed`.

## Review, merge, finish

- Reviewer gets the spec and the diff, never the implementer's reasoning trace. Blocking findings need file:line. `INSUFFICIENT_CONTEXT` is a valid result.
- Per branch: read the diff, read the test evidence, run the affected gate if it changes the merge decision, merge only a coherent change. A worker's success claim is not merge evidence.
- Finish: every worker terminal, every accepted change committed with evidence, wave-owned worktrees and runtime files removed, the wave's board claims released (a dead holder's claim blocks every later wave), final report printed, then `codex-daemon stop --wave <name>` (stop marks active workers interrupted).

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
| `luna` | Read-only explorer: dashboard, `ls --all`, `show NAME`, `tail NAME`, `say NAME "text"`, `board`, `waves`, `watch` |

`luna` only reads existing files and cannot disturb a worker. It adds two judgments raw status lacks: threads of a wave whose launcher ended are `abandoned` (liveness belongs to the wave), and `luna board` marks claims whose holder is not live as `STALE` (claims outlive holders and block the slot).
