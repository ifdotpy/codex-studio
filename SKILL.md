---
name: codex-agents
description: Orchestrate one or more Codex worker threads through codex app-server JSON-RPC. Use for Codex delegation, parallel worktrees, Luna worker waves, goal budgets, monitoring, steering, reviewer workers, or durable multi-agent runs from any agent harness.
---

# Codex worker orchestration

Run Codex workers from any agent harness. Use `codex app-server` as the transport.

Keep one source for this skill. Store runtime state outside the skill directory.

Use the bundled scripts. Do not copy their logic into a project or prompt.

## Select the mode

| Mode | Use |
|---|---|
| One worker | One bounded bug, module, review, or measurement |
| Implementer and reviewer | One change that needs an independent review |
| Worker wave | Independent tasks with separate files and worktrees |

Do not split sequential reasoning across workers. Do not assign one file to two workers.

The orchestrator owns task boundaries, steering, review decisions, and merges. Workers do not decide these items by consensus.

Run the launcher from the top-level orchestrator. A nested subagent cannot hold a wave: it cannot wait, steer, or survive its own turn ending.

## Use portable paths

The scripts use this state directory:

1. `$CODEX_AGENTS_STATE_DIR`, if set.
2. `$XDG_STATE_HOME/codex-agents`, if `XDG_STATE_HOME` is set.
3. `~/.local/state/codex-agents`.

The scripts create the state directory. They do not write runtime files into the skill directory.

The board uses the `board` subdirectory. A workspace-write worker can write only its worktree and this board directory.

Use `CODEX_HOME` to select the Codex login and session directory. The default is `~/.codex`.

Install the canonical skill at `~/.agents/skills/codex-agents`. Add a harness-specific link only when that harness does not scan this path.

## Check the installed Codex protocol

App-server schemas can change. Check the installed version before you depend on a protocol field.

```bash
codex --version
codex app-server generate-json-schema --experimental --out /tmp/codex-schema
codex app-server generate-ts --experimental --out /tmp/codex-proto
```

If `--experimental` fails, run the commands without it. Read the `v2` schema.

Goal methods need `capabilities.experimentalApi: true` during `initialize`.

The bundled launcher uses JSON Lines over standard input and output. It does not use `Content-Length` framing.

## Select the model

Use Luna at effort `max` for worker waves.

```bash
scripts/codex-models
```

The command prints model identifiers and supported effort values. The default launcher values are:

```text
CODEX_MODEL=gpt-5.6-luna
CODEX_EFFORT=max
```

The launcher sets the model on `thread/start`. It sets the model and effort on `turn/start`.

The launcher checks each worker path, branch, and linked worktree before it starts Codex.

It claims each canonical worktree across all active waves. A second wave cannot use the same worktree.

Do not infer the model from a local configuration file.

## Prepare isolated worktrees

Create one worktree and one branch for each worker. Give each worker an absolute path.

Do not use the main checkout for worker edits. Do not use another worker's worktree.

Each worker prompt must include:

1. One deliverable.
2. The owned files or module.
3. A gate that proves completion.
4. A command to take the baseline.
5. A role: `implementer` or `reviewer`.

An implementer prompt must require a commit for each finished part.

A reviewer uses a separate read-only worktree. A reviewer must not edit files or create commits.

Use one deliverable per worker. A large open request usually consumes its budget without a coherent commit. Point at an existing file or test as the pattern to imitate; an open-ended implementation in an unfamiliar codebase burns the budget without a coherent result.

## Write the brief

The first paragraph of every prompt states the wait rule: nothing wakes the worker; every wait is an active loop in bounded slices (400 seconds or less), one tool call per slice; long runs are detached with the PID stored in a file. A worker that waits passively hangs forever.

Give every shared path as an absolute path: the board command, shared state, reference binaries. A worker cannot find harness-owned paths by searching.

Name the live peer processes and the worktrees the worker must not touch. A worker cannot know that another worker is building next door.

State the outcome, not the mechanism. Numbers in a brief are context, not measurements; a colleague's diagnosis is a hypothesis, and the brief must say so.

End every prompt with a required FEEDBACK section: wrong or incomplete task statement; tools that caused avoidable work; facts in the brief the worker disproved; what would make the next run shorter. The orchestrator files each complaint in the project's incident register and fixes the cause. A repeated complaint about a recorded item is an orchestrator defect.

## Create the task file

Set a wave name. Then write its task file in the state directory or set `CODEX_TASKS`.

The default file is `codex-tasks.<wave>.json`.

```json
[
  {
    "name": "parser-fix",
    "role": "implementer",
    "cwd": "/absolute/path/to/worktree",
    "branch": "codex/parser-fix",
    "objective": "Correct the parser error and prove the correction.",
    "prompt": "Correct one parser error. Add a regression test. Run the parser gate. Commit the result."
  }
]
```

The default role is `implementer`. Use unique worker names and worktree paths.

## Start a wave

Run the launcher from the skill directory or use its absolute path.

```bash
export CODEX_WAVE=parser
export CODEX_MODEL=gpt-5.6-luna
export CODEX_EFFORT=max
export CODEX_BUDGET=1000000
scripts/codex-swarm.mjs
```

`CODEX_BUDGET` is optional. When set, it is a hard token limit for each worker.

The launcher inherits the Codex approval and sandbox settings by default. Verify the effective approval and sandbox policy before the wave starts: a worker that cannot approve a needed command stalls on its first privileged step.

Set these optional values only when the wave needs an explicit policy:

```text
CODEX_APPROVAL_POLICY=never
CODEX_SANDBOX=workspace-write
```

Allowed sandbox values are `read-only`, `workspace-write`, and `danger-full-access`.

A reviewer always uses `read-only`. The launcher refuses to replace a live launcher for the same wave.

## Monitor the wave

Start the watcher when you start the wave.

```bash
scripts/codex-watch --wave parser
scripts/codex-report --wave parser
scripts/codex-report --wave parser parser-fix --answers
```

The watcher waits for the launch-complete marker. It does not treat a growing worker list as a replacement wave.

The watcher pins the first run identifier. It returns nonzero if another run replaces an unfinished run.

It also returns nonzero for a failed, blocked, interrupted, abandoned, paused, or stalled worker.

The report command prints bounded output. Do not send raw event logs or rollout files into the orchestrator context.

When a harness cannot run `codex-watch` and you hand-roll a sentinel over the status file, copy the terminal values from one real terminal record first. The launcher writes `goalStatus` values such as `complete`, `budgetLimited`, and `blocked` — not `completed`. A sentinel that enumerates success spellings from memory sleeps through every success. The safe condition is: terminal when `goalStatus` is present and not `active`, or `turnStatus` is `failed` or `interrupted`.

Arm the replacement watcher before you stop the old one. A gap between watchers is a missed event.

## Steer a worker

Use the mailbox that belongs to the active launcher.

```bash
scripts/codex-steer --wave parser parser-fix "Limit the change to the parser module."
```

The command writes one message to the run inbox.

The launcher uses `turn/steer` for an active turn. It uses `turn/start` for a terminal turn.

The launcher deletes the message only after the Remote Procedure Call (RPC) succeeds.

Each message has one stable identifier. Failed delivery uses backoff. The launcher preserves the last failed copy in the dead-letter directory.

Do not start a second app-server process to steer an active thread. The second process does not own that thread.

A steer makes the wave live again. A watcher report older than your last steer is stale: the steered worker may be mid-turn on your instruction. Re-read the status file immediately before any launcher stop, and never stop a launcher on a report that predates your last steer.

## Coordinate shared resources

The launcher adds the board command and state path to each worker prompt.

Use the board for exclusive resources such as one shared build directory:

```bash
"$CODEX_BOARD" claim product-build "$CODEX_BOARD_OWNER" "build reason"
"$CODEX_BOARD" renew product-build "$CODEX_BOARD_OWNER"
"$CODEX_BOARD" release product-build "$CODEX_BOARD_OWNER"
"$CODEX_BOARD" show
```

Only implementers write board claims. Reviewers use read-only checks and do not claim resources.

Claims do not expire. Use `takeover` only after you verify that the old owner no longer uses the resource.

```bash
"$CODEX_BOARD" takeover product-build "$CODEX_BOARD_OWNER" "old-wave:run-id:worker" "reason"
```

Set `CODEX_AGENTS_STATE_DIR` when you run the board outside a worker.

The board stores facts and resource claims. It does not store design decisions.

## Handle capacity and budgets

A capacity error rejects one turn. It does not delete the thread or its history.

The launcher retries that turn on the same thread. It uses a longer delay after each refusal.

Do not switch models during these retries. A model switch changes the measured worker configuration.

A token budget is a hard stop. Tell every implementer to commit each finished part before the final report.

The launcher tracks every `turn/started` notification. A terminal turn is not a completed worker while its goal stays active.

Success needs a completed goal and a completed turn. Goal limits and blocked goals cannot become successful completed turns.

Read `turn.error` from every `turn/completed` notification. A completed notification can contain a failed turn.

## Keep the launcher alive

The app-server process owns its active threads. If the launcher stops, mailbox steering stops.

Start the launcher with the portable daemon. It works the same on macOS and Linux and does not depend on any harness:

```bash
export CODEX_WAVE=parser  # informational; start sets it from --wave
scripts/codex-daemon start --wave parser
scripts/codex-daemon status --wave parser
```

`codex-daemon start` detaches the launcher from the calling terminal and harness, writes its output to `codex-daemon.<wave>.log` in the state directory, and fails loudly when the launcher dies in the first seconds. It refuses to start without `codex-tasks.<wave>.json`; there is no fallback task file.

The launcher inherits the caller's environment. Export `CODEX_MODEL`, `CODEX_EFFORT`, and the optional values before `start`.

An OS service manager (launchd, systemd) is an optional alternative for launchers that must survive a reboot. `scripts/wave.plist.template` is a launchd example. A service process needs an explicit `PATH`; graphical services usually have a minimal environment.

Check the launcher PID file instead of `pgrep -f`. A process search can match its own shell command.

Print the final report. Then stop a completed launcher:

```bash
scripts/codex-report --wave parser
scripts/codex-daemon stop --wave parser
```

## One era of scripts per wave

Run the launcher, the watcher, the report, and the steer commands of one wave from the same skill checkout. Never mix an old copy of one script with a new copy of another: file names and status formats move together, and a mixed pair fails in silence or refuses valid state.

Do not copy scripts into a project or a job directory. The canonical scripts live in this skill; only state lives in the state directory.

## Review and merge worker output

For reviewer work, keep the implementer and reviewer in separate threads.

Give the reviewer the specification and the diff. Do not give the implementer's reasoning trace.

Require file and line references for blocking findings. Accept `INSUFFICIENT_CONTEXT` as an explicit result.

For each worker branch:

1. Read the diff.
2. Read the test evidence.
3. Run the affected gate when the result changes a merge decision.
4. Merge or cherry-pick only a coherent change.
5. Ignore an obsolete branch.

Do not merge a branch because the worker reports success.

## Finish the wave

Make sure that each worker has a terminal state.

Make sure that each accepted change has a commit and test evidence.

Remove only task-specific worktrees and runtime files that the wave owns.

Keep the final status and bounded reports when they are delivery evidence.

Stop the launcher after the final report. The stop command marks any active worker as interrupted.

## Bundled commands

| Command | Function |
|---|---|
| `codex-models` | List model identifiers and effort values |
| `codex-swarm.mjs` | Start and manage a worker wave |
| `codex-watch` | Report terminal states and stalls |
| `codex-report` | Print bounded status and final answers |
| `codex-steer` | Send a mailbox message to one worker |
| `codex-board` | Claim shared resources with a file lock |
| `codex-stop` | Stop one verified wave launcher |
| `codex-daemon` | Start, stop, or check a detached wave launcher (portable, no service manager) |
| `luna` | Explore threads: dashboard, one thread in full, events, mailbox |

Run each command from `scripts/`, or add that directory to `PATH` for the current shell.

### luna

`luna` reads only files that already exist: the status files, the event log, the claim board, the launcher PID file and Codex's own rollout files. It makes no API call, so it cannot disturb a worker.

```
luna                  dashboard: launcher, live threads, claims, load, disk
luna ls --all         every thread of every wave
luna show NAME        one thread in full, with its final answer
luna tail NAME        recent events, including capacity retries
luna say NAME "text"  queue a follow-up turn through the mailbox
luna board            slot holders, and which claims are stale
luna waves            threads grouped by wave, newest first
luna watch            the dashboard, refreshed
```

It reports two things a status file cannot state by itself:

- **Liveness belongs to a wave, not to a launcher.** Only the newest wave can act. A thread of an earlier wave belongs to a process that ended, so `luna` reports it as `abandoned` while a launcher is up. Without this rule, a thread frozen for days reads as slow.
- **A claim outlives its holder.** Nothing releases the board when a worker dies. `luna board` marks a claim as `STALE` when its holder is not live. Such a claim blocks every worker that waits for the slot.

`luna` finds the state directory from `CODEX_AGENTS_STATE_DIR`, then `LUNA_HOME`, then its own directory, then the newest state directory it can find.
