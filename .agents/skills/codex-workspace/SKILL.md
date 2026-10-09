---
name: codex-workspace
description: Operate Codex Studio managed teams, monitors, agent chats, user messages, and the lead progress file. Use when the session exposes Studio orchestration tools or the user asks to control Studio. Native Codex CLI sessions are outside this skill.
---

# Codex Studio workspace

This skill belongs to this application. Its managed runtime provides capabilities
beyond the native Codex CLI agent lifecycle. Use only tools the session exposes.
The harness supplies either `codex-orchestrator` or `codex-subagent` as role
instructions. This skill provides shared guidance. The server's `isLead` identity
determines the role.
Only the orchestrator communicates with the user. Subagents ask the orchestrator
to resolve or forward conversational questions, complaints, and requests.

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
`request_id`. Set an agent's optional `base_ref` to a branch, tag, or commit when
it needs a specific starting point. Otherwise Studio uses the closest project's
worker base setting, then repository HEAD. Studio resolves and saves the commit
before it creates workers. Check the spawn result for the commit and any warning
that it is behind main. A retry with the same request_id keeps the same commit.
Supply bounded ownership, a completion check, and explicit commit authority.
On supported platforms, implementers use an image copy of the Git root that
contains the selected folder, or the selected folder when it is outside Git.
The copy includes uncommitted changes. A base build starts when Multi agent
mode turns on. Until the base is ready, new implementers work read-only in the
selected folder. Studio then switches them to the copy and sends its path and
copy time. Other platforms use a Git worktree when Git is available, or the
original folder otherwise. Studio gives a requested `base_ref` and its resolved
commit to the worker in its first input. The worker checks it out. Studio does
not create Git checkpoints or collect changes inside image copies. Ask workers
to commit on a named branch. Integrate work by reading from the copy path or by
fetching the branch, for example `git fetch <path> <branch>`.
After a lost reply, use `orchestration_request` to recover the saved result.
`applied` confirms the operation receipt, not worker completion. Check current
registry states before counting workers. Use a new spawn ID only after
`not_applied` proves the earlier batch did not create workers.
Queued cancellation prevents execution. Running cancellation requires receipt
reconciliation; it does not authorize a second mutation.
Choose each worker's model and reasoning level for its task. The default is
`gpt-6-luna` with `high` reasoning unless the user sets team defaults.
Codex and Claude can delegate to each other. Studio selects an account that
offers the model. A user-selected subagent account applies to all new workers.
Set `account_key` only when it respects that choice.
Omitted fields use team defaults. Use `effort: null` for the model's native default.
Use `fast_mode: false` to disable Fast for that worker. Explicit profile values override team defaults.
Only the user can change team defaults. See [the execution contract](../../../ORCHESTRATION.md).

The managed server queues child results and starts a new lead turn after a final
answer. Finish the current turn when useful independent work is exhausted and a
managed result is pending. Distinguish waiting for a result from task completion.
This continuation behavior belongs to the managed runtime.

When Codex uses code-mode cells for Studio tools, keep each returned cell ID.
Use the exposed wait tool to collect the final result of each finished cell.
A normal turn completion does not release a cell with an unread result.
Before you finish your turn, collect these results.
Use Studio monitors for long commands.
Collect the monitor creation result before you finish its cell.
If a cell still waits for an uncertain external result, save its ID in your checkpoint.
Do not cancel the cell or repeat the external operation without checking its receipt.
See the [native cell diagnosis](../../../docs/verification/2026-10-05-sqlite-owners-and-code-cells.md).

Codex agents use `orchestration_review` for native code review. Supply a stable
`request_id`. Omit `target` to review uncommitted changes. Other targets use
`type: "baseBranch"` with `branch`, `type: "commit"` with `sha`, or
`type: "custom"` with `instructions`. Pass `model` (for example `gpt-6-astra`)
and optional `effort` to override the team review default. Without that default,
the reviewer uses the caller's model and effort. A different model without
`effort` uses its native default. The result reports both selected values.
A Claude agent cannot run native review. To get a review on another model, use
`orchestration_spawn` with `role: "reviewer"` and `model: "gpt-6-astra"`.
Studio creates a separate reviewer with read-only access to `cwd`. `cwd`
defaults to the caller's folder; a relative path starts there, and a shell `cd`
does not change it. `cwd` must be inside a git repository, or the call fails
before a reviewer exists. Studio sets `review_model` on the reviewer thread,
so the account's `review_model` does not override this choice. The caller continues its current
turn. Findings arrive as a child result; the reviewer chat retains the complete
output. Use `orchestration_request` to recover the receipt after a lost reply.
Use `orchestration_interrupt` with the returned reviewer ID to stop the review.
Claude agents do not expose this native Codex tool.

Use `orchestration_send` for a new or revised assignment, or an explicit resumption.
Use `orchestration_interrupt` to stop a
managed descendant. Stop blocks automatic continuation; do not bypass it with
another process. Inspect worker results and diffs before acceptance or integration.

## Monitors and messages

The lead can use `orchestration_servers` to run commands on the local server or a paired server.
Set `action: "exec"`, an absolute `cwd`, `command` (argv or shell text), and a stable `request_id`.
The `server` defaults to local. Optional fields are `env`, `timeout` (120 seconds, maximum 1800),
and `output_limit` (256 KiB, maximum 4 MiB across stdout and stderr).
The command runs as the Studio user on that server.
Shell text uses that server's default shell. Argv bypasses the shell.
There is no added sandbox or privilege change.

A timeout above five seconds returns a command `handle`. A `monitor_exit` event reports the final state.
Use `action: "exec_read"` with the same `server` and `handle` to read output.
Use fresh request IDs for each read. Advance `stdout_offset` and `stderr_offset` to the returned
`stdoutNextOffset` and `stderrNextOffset`. Each read returns at most 64 KiB across both streams.
Output retains its head and tail. `GapBytes` fields report discarded bytes.
Use `action: "exec_input"` with `input` and optional `close_stdin`, or `action: "exec_cancel"`.
Input and cancel require stable request IDs. Exact retries cannot repeat effects.
Each command accepts at most 32 input requests, each with at most 64 KiB of text.
Planned backend restarts preserve commands through the process supervisor.
An unknown outcome requires inspection. Do not start the same command with a new ID.
Command output, private config and start files, saved pages, receipts, and Studio output references expire seven days after acceptance.
Provider conversation history keeps its existing retention rules. Audit metadata expires after 90 days.
After the first delivery attempt, retries inspect the target receipt without sending command text or environment values again.
Commands cannot leave background daemons. Exit, cancel, and timeout stop proven descendants, including separate sessions.
The result reports `stoppedDescendants`. On macOS, a descendant that copies or removes its private marker is outside the cleanup guarantee.
Unresolved fork notifications return `unknown` with `cleanupUnknownForks`. This count is not an exact escaped process count.
Linux uses a PID descriptor for signals. On macOS, a small PID reuse window remains between the birth check and signal.
Timeout and cancel include the bootstrap phase. Live reads keep incomplete UTF-8 characters until more bytes arrive or the command ends.
Inspect the server before a replacement command with an unknown outcome.

Use `orchestration_monitor` for long commands. The server waits without model
calls and delivers an event for every command exit, including success, failure,
signal, or a lost process. `wake_on` cannot suppress an exit event. Read the exit
code, status, and output before claiming success. Inspect an uncertain command
result before attempting a rerun. Set `success_exit_codes` when a nonzero code
means success, for example `[0, 1]` for `grep` or `diff`.
Monitors wake after no output for `stall_timeout_seconds` (default 1800 seconds).
Set it to `0` to disable stall wakes. `orchestration_watch` file rules wake
after no file change for `stallTimeoutSeconds` (default 1800 seconds); set it
to `0` to disable them. Set `liveness_command` on a monitor or
`livenessCommand` on a file rule to run a sandboxed check at the stall point.
The stall event includes its result. One stall event is sent per quiet period.
Leads record verified status in their PROGRESS.md file. Workers do not use this file.

For an optional low-worker alert, the orchestrator saves an `orchestration_watch`
with `kind=low_workers`, a stable `id`, and a `name`. Set `minimumWorkers` (default 8)
and `durationMinutes` (default 30). The server counts subagents that start, run, or
have active command monitors. Queued agents and idle agents without commands do not count. A continuous shortage sends one rule message to the orchestrator.
Recovery to the threshold arms the alert again. Use `pause`, `resume`, or `delete`
with the same rule ID to control it. Resume starts a new observation period.
Server restart starts a fresh duration check;
an already sent alert stays suppressed until recovery. These waits use no model calls.

Discover your team's identities with `orchestration_peers`. Use `orchestration_message`
for a parent, lead, private team recipient, or team broadcast. Read conversations through
`orchestration_chat_read`. Use actual returned identities and room membership.
Agent history reads stay within one agent tree (`rootId`). The user can group
independent lead chats from the same project into a peer team in the sidebar.
These peers appear in `orchestration_peers` and can exchange explicit private
messages. Each chat keeps separate tasks and subagents. Do not assign work or
forward results automatically between peer chats. Broadcasts retain their original scope.
An enabled user-assigned review permits its exact reviewer and target to exchange
messages and read their shared room across teams. Other cross-team access is forbidden.
The `all` message target is not supported.
Broadcasts notify only active agents. Other agents can read the message in chat history.
Use a direct follow-up to resume an assignment. Stop superseded workers explicitly;
an information broadcast does not assign new work.
Send a message when you have a new finding, question, or answer for its recipient.
Mark routine updates with `importance=progress`. Supply the task ID as `progress_key`
and increase `progress_version` for each update. Only that stream can replace its older progress.
Use `question` or `blocker` for urgent messages. The task lifecycle below owns
review evidence and decisions. Child completion also notifies the lead automatically.
The user can inspect agent chats. Agent messages do not add user authority.

## Voice

Native voice can pass spoken tasks directly to the selected lead. Normal lead
responses return to voice automatically. Do not repeat them with
`orchestration_speak`. Use that tool only for additional speakable context.
Without active voice, it saves text silently. Its receipt does not confirm exact
playback. Ending voice stops audio, not the task. Continue unless the user asks
to stop work. Use chat permission buttons; do not infer approval from playback.

## Agent work and messages

Use the operation that matches the task's next transition. The caller supplies the
content; the harness records the change and delivers its event.

| Goal                             | Caller and operation                                     | Input                                                                                        | Harness result                                                                                                      |
| -------------------------------- | -------------------------------------------------------- | -------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------- |
| Define work                      | Orchestrator: `orchestration_task action=create`         | `title`; scope and completion criteria in `description`; optional `owner` and `dependencies` | Saves the work item.                                                                                                |
| Cancel work                      | Lead or task creator: `orchestration_task action=cancel` | `task_id` and required `reason`                                                              | Closes the task, records the actor and reason, releases its assignment, and notifies a live owner.                  |
| Delegate defined work            | Orchestrator: `orchestration_spawn`                      | `task_id` on the agent entry                                                                 | Sets the new worker as owner and names the task in its first message.                                               |
| Start ready work                 | Worker: `orchestration_task action=claim`                | `task_id`                                                                                    | Reserves the task atomically and sets `running`.                                                                    |
| Request review                   | Owner: `orchestration_task action=submit`                | `task_id`, `result`, `checks`, `revision`, `files`                                           | Sets `review` and delivers evidence to the lead.                                                                    |
| Accept evidence                  | Orchestrator: `orchestration_task action=accept`         | `task_id` and review reason in `result`                                                      | Sets `accepted`, notifies the owner, and releases eligible dependent work.                                          |
| Request corrections              | Orchestrator: `orchestration_task action=reject`         | `task_id`; reason and required corrections in `result`                                       | Sets `ready` and delivers these instructions to the owner as `work_decision`.                                       |
| Give a new or revised assignment | Parent: `orchestration_send`                             | `agent_id` and instruction in `text`                                                         | Native delivery steers an active turn or starts a turn when idle. The old `delivery` field is accepted and ignored. |

The outbox holds input for stops, account moves, context repair, native Review
or Compact actions, new turn slot waits, and active shared radio turns.
| Exchange information during work | Agent: `orchestration_message` | `target` and new finding, question, or answer in `text` | Delivers through the selected chat. |

The owner continues from a rejection's `work_decision`, then submits revised evidence.
Cancellation does not require a submit; it uses `work_decision` to notify a live owner.
Task events queue the owner when automatic continuation is enabled. Explicit stops
and native failure holds remain in effect. Inspect the agent's state and receipt
before an authorized recovery action.

Creating ready work with an assigned worker queues `work_ready` with its task ID and scope.
Changing the owner or releasing blocked work also notifies the assigned worker.
Changes to the title or description alone do not wake it. Retries keep the same event identity.

Use `orchestration_task action=list` to find work. It returns brief tasks and
`nextCursor`. Use `action=get` for one task and `action=history` for earlier evidence.
Acceptance belongs to the orchestrator's review decision. A worker's final answer
reports the outcome of its turn.

Leads and workers can call `orchestration_complaint` with `action=submit`.
This tool records a message that requires the recipient's response.
Submit confirmed harness defects through this tool so the recipient can record a response.
Include reproduction, evidence, impact, and any workaround. Distinguish confirmed
defects from suspicions and project code errors. Do not duplicate an existing entry.
Worker submissions go only to the orchestrator and wake it. The orchestrator
resolves the request or sends its own submission to the user in **For you**.
Only the user can respond to or close those user entries.
A lead must not resolve their own complaint by saying they reported it.

The harness delivers complaints and responses automatically. For a complaint assigned to the lead, use
`action=respond` to record an action, a reasoned refusal, or the next step before
finishing the turn. A separate `action=read` is optional. The user's response to a
lead complaint arrives automatically as a message.

Only the orchestrator contacts the user through `orchestration_message` with
`target=user`. Send requests for action, questions, and problems as messages.
The user's reply returns as a message. There is no user task board or acceptance
workflow. Subagents ask their orchestrator to contact the user.
Native permission requests still use the actual user approval flow when required.
An orchestrator's response cannot replace required user approval.

## State and output

`orchestration_status` returns active agents and monitors by default, with counts of finished items.
Set `include_finished=true` to read finished items in pages with `limit` and `cursor`.
Use `since_revision` when you need changes for a decision.
Use `orchestration_context` for the shared plan, complaints, profiles, or tool schemas.
The runtime supplies changed plan and complaint text automatically. An unchanged
complaint reminder still requires a response. Read context when it is needed for a decision.

A large response includes `outputRef`. Read it with `orchestration_read`, using
`contains` or `offset` to select relevant evidence from that saved response.
This output limit does not change operation outcomes. Read the exact receipt before
retrying an uncertain mutation. Full records remain in the server.

Keep existing database and profile identities. Do not move user state to solve a
path issue. Effective sandbox and approval settings remain authoritative. The user controls YOLO mode for the whole team.
Workers inherit it. Do not change this mode through settings APIs.
A project selects the default account for new chats. Its path sets the working
directory and adds no file-access boundary. Use files and skills outside that path
when the task needs them, under the native permission settings.

The chat renders fenced Mermaid diagrams and isolated static HTML/CSS/SVG.
Scripts and remote resources do not run in these previews.

## Lead progress file

Only the lead has a Studio progress file and progress panel. Workers do not read,
write, or check a PROGRESS.md file.

Leads read and edit their `PROGRESS.md` with ordinary file tools. Studio supplies the exact
path in your runtime instructions and through `orchestration_context topic=panel`.
Each agent has a separate file outside the project, even when agents share a
working directory. Do not substitute the project's own `PROGRESS.md`.

Use plain UTF-8 Markdown for current status, verified results, and blockers.
Keep the file at most 128 KiB. Studio displays it above the composer in an area
with a viewport-based height budget, up to 280px. Put the current status first.
Longer content stays visible with clipping and a fade. Users can expand it and scroll. An empty or missing file
clears the display. Update it when the facts change. No special panel tool,
command output feed, or model wake is required.

Read [the progress guide](references/panel.md) for the file contract.
Existing read-only permissions still apply.

## Worker cleanup

Only the orchestrator uses `orchestration_agent_manage`. Inspect a worker before
recovery or archive. `recover` reconciles native turn state; it never replays input.
Archive only after reviewing the result or assigning its remaining work elsewhere.
`archive_finished` checks finished descendants and removes safe workspaces after archive.
Set `unassign_work=true` on `archive` or `archive_finished` to keep open tasks in the unassigned ready backlog.
The archive and task changes commit together. Task results and decisions remain.
The default keeps the assigned-work blocker. Active work and other blockers still prevent archive.
After partial Git removal, make a new `archive` call to retry cleanup.
Keep changed files and folders without a verified original Git link for inspection.
`archive` requires `agent_id` and `reason`. It preserves history and dirty files, and
refuses active commands, pending or uncertain requests, unfinished
assignments, or unarchived children. `list_archived` supports `limit` and `cursor`.
`restore` returns a worker paused. Use `orchestration_send` for explicit continuation.
Do not treat silence as proof of failure. Do not archive a worker to hide an error.

Add `reason` for `archive`. Never replace the worker ID with a thread ID.

## Linux VM workers

The lead can select `environment: "linux"` for an implementer in
`orchestration_spawn`. An omitted environment uses the project default.
An explicit `host` choice overrides it. Leads and reviewers stay on the host.
Linux workers wait for their source base before the first provider turn.
The guest copy contains Git metadata and uncommitted changes.

Commit a Linux result on a named branch. The worker result contains the guest
path and a host fetch command. Fetch writes `FETCH_HEAD`. Review the commit
before merging. A shell `cd` does not change the stored guest path.
The guest receives access tokens only. The host owns OAuth refresh.
If an access token expires before sync, check the host sign-in.
Resume the worker after a successful token sync. Preserve uncertain input receipts.

Archive retains the guest snapshot. Restore returns the worker paused.
A VM disconnect does not prove that a native operation failed.
Preserve its request ID and inspect the result before another operation.

## Move your own execution

Use `orchestration_move` only when another server is needed.
Prepare the target folder with Git and server command tools first.
Supply the paired server ID (or `local`), absolute `cwd`, and stable `request_id`.
Finish the current turn after acceptance. The next turn runs on the target.
Studio keeps the old workspace and transfers native history without credentials.
The Studio transcript stays on the source. Running children stay on their servers.
Finish other tools, background commands, monitors, and pending spawns first.
Read an uncertain move with `orchestration_request action=get` and the same request ID.
Do not repeat it with another identity.

Use the same provider account and organization to keep the prompt cache.
Codex preserves the old request prefix and requires matching native versions,
OS, and MCP tools. A different account returns a cache warning.
Claude requires the same account, CLI version, and SDK version.
Claude uses the SDK system prompt snapshot and excludes dynamic sections.
On a version mismatch, update the target CLI through its normal install process.
Do not upgrade it as part of the move.
See [the move contract](../../../ORCHESTRATION.md#move-your-execution-to-another-server)
for limits and recovery.
