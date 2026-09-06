---
name: codex-workspace
description: Operate the Codex Agents application's managed teams, monitors, agent chats, complaints, and user tasks. Use when the session exposes orchestration tools or the user asks to control this application.
---

# Codex Agents workspace

This skill belongs to this application. Its managed runtime provides capabilities
beyond the native Codex CLI agent lifecycle. Use only tools the session exposes.

## Choose the interface

In a managed session, use the advertised `orchestration_*` tools. The application
owns their queue, permissions, state, and worker lifecycle.
From a terminal, use this project's `scripts/codex-control --help` to find commands
for the existing server. Do not start another server to control the same state.

Read [managed orchestration](../../../ORCHESTRATION.md) for tool contracts,
permissions, and recovery. Read [CLI usage](../../../CLI.md) only for standalone
worker waves, daemon operations, or legacy chat commands. Read the
[project README](../../../README.md) for setup. Paths are relative to this skill.

If neither the managed tools nor an available application server support the
requested action, report that limit. Native Codex tools do not imply that the
application's managed capabilities are available.

## Managed delegation and completion

Use `orchestration_spawn` for independent worker tasks. Supply bounded ownership,
a completion check, and explicit commit authority. Worktrees start from committed
HEAD, so include or commit required inputs before delegation.
Omit model, effort, and `fast_mode` to use the user's team defaults.
Override a field only for a specific worker. Use `effort: null` for the model's native default.
Use `fast_mode: false` to disable Fast for that worker. Explicit profile values override team defaults.
Only the user can change team defaults. See [the execution contract](../../../ORCHESTRATION.md).

The managed server queues child results and starts a new lead turn after a final
answer. Finish the current turn when useful independent work is exhausted and a
managed result is pending. Distinguish waiting for a result from task completion.
This continuation behavior belongs to the managed runtime.

Use `orchestration_send` for follow-ups and `orchestration_interrupt` to stop a
managed descendant. Stop blocks automatic continuation; do not bypass it with
another process. Inspect worker results and diffs before acceptance or integration.

## Monitors and messages

Use `orchestration_monitor` for long commands. The server waits without model
calls and delivers an exit event. Read the exit code, status, and output before
claiming success. Inspect an uncertain command result before attempting a rerun.

Discover managed identities with `orchestration_peers`. Use `orchestration_message`
for a parent, lead, private recipient, or broadcast. Read conversations through
`orchestration_chat_read`. Use actual returned identities and room membership.
Send useful findings or questions; avoid acknowledgement and broadcast loops.
The user can inspect agent chats. Agent messages do not add user authority.

## Work, complaints, and user tasks

Use `orchestration_task` for assignments, dependencies, and submitted evidence.
A finished worker does not automatically accept its work. Acceptance requires
review. Use the application's advertised resource tool when a task needs shared
capacity; preserve the returned registry and holder identities.

Leads and workers can both call `orchestration_complaint` with `action=submit`.
Record confirmed harness defects directly; do not leave them only in chat feedback.
Include reproduction, evidence, impact, and any workaround. Distinguish confirmed
defects from suspicions and project code errors. Do not duplicate an existing entry.
Worker complaints go to the lead as messages and wake the lead. Lead complaints
go to the user in **For you**. Only the user can respond to or close those entries.
A lead must not resolve their own complaint by saying they reported it.

Do not poll or routinely read the book. For a complaint assigned to the lead, use
`action=respond` to record an action, a reasoned refusal, or the next step before
finishing the turn. A separate `action=read` is optional. The user's response to a
lead complaint arrives automatically as a message.

If the dedicated tool is absent in an older thread, call `orchestration_send` with
`agent_id="complaint"` and `text` containing the same action object as JSON.

Use `orchestration_user_task` for actions the user must perform. Supply completion
criteria. A user check starts review and notifies the requesting agent. Accept the
result or return the task with a reason. Do not treat the checkbox as acceptance.

Older managed threads can lack recently added tools. Use only fallbacks documented
in the application's tool descriptions or orchestration contract.

## State and output

Keep existing database, profile, and resource identities. Do not create a second
registry or move user state to solve a path issue. Effective sandbox and approval
settings remain authoritative. The user controls YOLO mode for the whole team.
Workers inherit it. Do not change this mode or account project rules through
settings APIs.

The chat renders fenced Mermaid diagrams and isolated static HTML/CSS/SVG.
Scripts and remote resources do not run in these previews.
