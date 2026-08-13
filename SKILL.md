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

Use one deliverable per worker. A large open request usually consumes its budget without a coherent commit.

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

The launcher inherits the Codex approval and sandbox settings by default.

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

Use the persistent-process function of the current harness or operating system. Keep standard input and output connected to the launcher.

On macOS, use `scripts/wave.plist.template` as a launchd template.

Replace every placeholder. Create `__STATE_DIR__` before you load the job:

```bash
mkdir -p __STATE_DIR__
plutil -lint /absolute/path/to/wave.plist
```

Set an explicit `PATH` for a service process. Graphical services usually have a minimal environment.

Check the launcher PID file instead of `pgrep -f`. A process search can match its own shell command.

Print the final report. Then stop a completed launcher:

```bash
scripts/codex-report --wave parser
scripts/codex-stop --wave parser
```

For launchd, unload the job before you delete its plist file:

```bash
launchctl bootout "gui/$(id -u)" /absolute/path/to/wave.plist
```

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

Run each command from `scripts/`, or add that directory to `PATH` for the current shell.
