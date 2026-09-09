---
name: codex-workspace
description: Operate Codex Studio managed teams, monitors, agent chats, complaints, user tasks, and visual panels. Use when the session exposes Studio orchestration tools or the user asks to control Studio. Native Codex CLI sessions are outside this skill.
---

# Codex Studio workspace

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

Use `orchestration_spawn` for independent worker tasks. Give each batch a stable
`request_id`. Supply bounded ownership,
a completion check, and explicit commit authority. Worktrees start from committed
HEAD, so include or commit required inputs before delegation.
After a lost reply, use `orchestration_request` to recover the saved result.
`applied` confirms the operation receipt, not worker completion. Check current
registry states before counting workers. Use a new spawn ID only after
`not_applied` proves the earlier batch did not create workers.
Queued cancellation prevents execution. Running cancellation requires receipt
reconciliation; it does not authorize a second mutation.
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
Use `wake_on=failure` only when a successful exit needs no agent follow-up.
For display-only counters, use a panel feed instead of status or log polling.

Discover managed identities with `orchestration_peers`. Use `orchestration_message`
for a parent, lead, private recipient, or broadcast. Read conversations through
`orchestration_chat_read`. Use actual returned identities and room membership.
Broadcasts notify only active agents. Other agents can read the message in chat history.
Use a direct follow-up to resume an assignment. Stop superseded workers explicitly;
an information broadcast does not assign new work.
Send useful findings or questions; avoid acknowledgement and broadcast loops.
Mark routine updates with `importance=progress`. Supply the task ID as `progress_key`
and increase `progress_version` for each update. Only that stream can replace its older progress.
Use `question` or `blocker` for urgent messages. Do not repeat a submitted result
in a separate message. Submission and child completion already notify the lead.
The user can inspect agent chats. Agent messages do not add user authority.

## Voice

Native voice can pass spoken tasks directly to the selected lead. Normal lead
responses return to voice automatically. Do not repeat them with
`orchestration_speak`. Use that tool only for additional speakable context.
Without active voice, it saves text silently. Its receipt does not confirm exact
playback. Ending voice stops audio, not the task. Continue unless the user asks
to stop work. Use chat permission buttons; do not infer approval from playback.
Older threads can use the workspace fallback with
`{"tool":"orchestration_speak","arguments":{"text":"..."}}`.

## Work, complaints, and user tasks

Use `orchestration_task` for assignments, dependencies, and submitted evidence.
List returns brief tasks with `nextCursor`. Use `action=get` for one task and
`action=history` for earlier evidence. Do not load every task's history to find work.
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

Use `since_revision` with `orchestration_status` when you need changes for a decision.
Use `orchestration_context` for the shared plan, complaints, profiles, or tool schemas.
The runtime supplies changed plan and complaint text automatically. An unchanged
complaint reminder still requires a response. Do not poll for context updates.

A large response includes `outputRef`. Read it with `orchestration_read`, using
`contains` or `offset` to select relevant evidence. Do not rerun the original tool.
This output limit does not change operation outcomes. Read the exact receipt before
retrying an uncertain mutation. Full records remain in the server.

Keep existing database, profile, and resource identities. Do not create a second
registry or move user state to solve a path issue. Effective sandbox and approval
settings remain authoritative. The user controls YOLO mode for the whole team.
Workers inherit it. Do not change this mode or account project rules through
settings APIs.

The chat renders fenced Mermaid diagrams and isolated static HTML/CSS/SVG.
Scripts and remote resources do not run in these previews.

## Studio panel

For a persistent visual display or interactive control, read
[the panel guide](references/panel.md), or use `orchestration_context topic=panel`.
It defines the 150px content viewport,
component composition, local state, strict measurement, and callback behavior.
Use the json-render `spec` mode by default. Read the live catalog with
`orchestration_panel` and `action=catalog` before composing an unfamiliar panel.
Studio controls component styles. Use HTML only when the catalog cannot express
the required visual or interaction. These rules apply only inside Codex Studio.

Use [the progress example](assets/panel-progress.json) for a complete starting
structured composition. Its counts and states are sample data. Replace them with verified
values and adapt the composition to the task. The example is not a required layout.

For data that changes without agent work, use [a background panel feed](references/panel-feed.md).
A script supplies state directly to the panel without model calls. Use the EC2
example for explicit instance IDs and separate machine state from resource claims.

## Worker cleanup

Only the orchestrator uses `orchestration_agent_manage`. Inspect a worker before
recovery or archive. `recover` reconciles native turn state; it never replays input.
Archive only after reviewing the result or assigning its remaining work elsewhere.
`archive` requires `agent_id` and `reason`. It preserves history and files, and
refuses active commands, pending or uncertain requests, resource claims, unfinished
assignments, or unarchived children. `list_archived` supports `limit` and `cursor`.
`restore` returns a worker paused. Use `orchestration_send` for explicit continuation.
Do not treat silence as proof of failure. Do not archive a worker to hide an error.

For an existing thread without this native tool, use the workspace compatibility
call. Its permissions and archive checks are identical:

```javascript
await tools.orchestration_send({
  agent_id: "workspace",
  text: JSON.stringify({
    tool: "orchestration_context",
    arguments: { topic: "agent_manage", action: "inspect", agent_id: "WORKER_ID" }
  })
})
```

Replace `action` with `recover`, `archive`, `restore`, or `list_archived` as needed.
Add `reason` for `archive`. Never replace the worker ID with a thread ID.
