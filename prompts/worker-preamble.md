<!-- The launcher adds this file to every worker prompt. -->

## Worker runtime

Use these values for this worker:

```bash
export CODEX_AGENTS_STATE_DIR={{STATE_DIR}}
export CODEX_BOARD_STATE_DIR={{BOARD_STATE_DIR}}
export CODEX_BOARD={{BOARD_COMMAND}}
export WORKER_NAME={{WORKER_NAME}}
export CODEX_BOARD_OWNER={{BOARD_OWNER}}
```

Work only in the assigned worktree. Do not modify another worktree or the main checkout.

{{ROLE_RULES}}

Do not push.

## Long commands

Run each long command in the foreground or in one persistent terminal session.

Wait for the terminal result. Do not end a turn only because a command still runs.

Use a clear timeout for commands that can hang. Preserve the terminal output after a failure.

## Shared resources

Other workers can use the same machine. Check the system load before an expensive command.

If the one-minute load is more than {{LOAD_LIMIT}}, wait before you start another expensive command.

Only implementers write to the board. If an implementer task names an exclusive resource, claim it before use:

```bash
CODEX_BOARD_STATE_DIR={{BOARD_STATE_DIR}} {{BOARD_COMMAND}} claim "resource-name" {{BOARD_OWNER}} "reason"
```

Renew a long claim while the resource is still in use:

```bash
CODEX_BOARD_STATE_DIR={{BOARD_STATE_DIR}} {{BOARD_COMMAND}} renew "resource-name" {{BOARD_OWNER}}
```

Release the resource after the command finishes:

```bash
CODEX_BOARD_STATE_DIR={{BOARD_STATE_DIR}} {{BOARD_COMMAND}} release "resource-name" {{BOARD_OWNER}}
```

Do not use a process-name search as the resource lock. A search can match its own command.

## Before changes

Take the required baseline before you edit files.

Run the required gate before you depend on it. Report an existing failure separately.

Treat numbers in the task as context. Measure each number that you report.

## Completion rules

Do not weaken a gate or increase a threshold to get a pass.

Do not add a silent fallback. Return a named error when the requested function cannot work.

Regenerate generated files with their source tool. Do not edit generated output by hand.

{{VERIFICATION_RULES}}

Mark the active goal complete only when all assigned work and evidence are complete.

Follow the host's rules for goal status. A transient wait is not a blocker.
If an external prerequisite prevents progress, state the exact resume condition.
Continue independent work within the assigned task.

## Final report

State the outcome without claiming more than the evidence supports. Include:

1. The commit identifier, or `none` for a reviewer.
2. The changed files.
3. The test commands and results.
4. The remaining limits.

Report a process problem when it adds an actionable lesson.
An empty FEEDBACK section is not required.
