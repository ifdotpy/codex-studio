# Feature gaps for one lead and dozens of workers

Research date: 2026-09-06. Local source: `01c61fb`.
T3 Code source: `f8b4c464b4760d73e0ece7e68011c738803d8b69`.
These are recommendations based on source and documentation, not runtime certification of other products.

This document describes the baseline before implementation. See [workspace delivery](WORKSPACE-DELIVERY.md) for the delivered scope and checks.

## Baseline app

The runtime retains native Codex tools and adds 11 managed orchestration tools.
It already supports worker completion wakes, command monitors, agent rooms, complaints, worktrees, token budgets, and tool history.

Local evidence:

- [Runtime](scripts/codex_runtime.py): `TOOLS`, `send`, `start`, `record_task`, `native_action`, `prepare_locked`.
- [Conversation](web/src/components/Conversation.tsx): text composer, pending messages, copy, and approval controls.
- [Sidebar](web/src/components/Sidebar.tsx): search over names and recent previews, without full transcript search or archive.
- [Task panel](web/src/components/BackgroundTasks.tsx): process and tool history, monitor approval and cancellation.
- [Resource board](scripts/codex-board): existing command-line resource claims, renewal, release, and wait queues.
- [Native tool evidence](UI-AND-TOOLS.md#tool-evidence): tool parity and prior live inventory. Configuration controls actual availability.

`runtime_tasks` stores tool executions. It is not a work backlog with dependencies or atomic work assignment.
`send` always queues an event. The managed runtime does not call `turn/steer`.
`start` sends a text-only input array. There is no attachment upload path in the web composer.
The app stores plan and diff notifications as tool output. It lacks a separate plan editor and a diff review workspace.
The installed protocol has image inputs, thread forks, and turn steering. `thread/rollback` is marked deprecated in its generated schema.
Protocol rollback alone does not restore files or worker state.

## Priority UI additions

| Order | Addition | Local gap and proposed behavior | Reference |
| --- | --- | --- | --- |
| 1 | Shared work board | Ready, running, blocked, review, accepted. Each item has an owner, dependencies, and evidence links. | [Claude agent teams](https://code.claude.com/docs/en/agent-teams#assign-and-claim-tasks) |
| 2 | Steering and queue controls | Separate corrections for the current turn from follow-ups. Edit or cancel pending instructions. | [Pi message queue](https://github.com/earendil-works/pi/tree/main/packages/coding-agent#message-queue) |
| 3 | Changes and results workspace | Inspect each worker's file changes and reports. Comment on exact lines. Keep review state separate from a completed model turn. | [T3 diff comments](https://github.com/pingdotgg/t3code/blob/f8b4c464b4760d73e0ece7e68011c738803d8b69/apps/web/src/components/diffs/DiffCommentAnnotation.tsx) |
| 4 | Attention inbox | Collect pending questions, approvals, failures, and unresolved complaints across teams. Group routine notifications. | [OpenCode attention](https://opencode.ai/docs/tui/#attention) |
| 5 | Attachments and file previews | Paste screenshots and attach files. Open images, PDF, HTML, and reports beside the conversation. | [T3 Chat FAQ](https://t3.chat/faq), [T3 composer](https://github.com/pingdotgg/t3code/blob/f8b4c464b4760d73e0ece7e68011c738803d8b69/docs/user/composer.md) |
| 6 | Conversation branches and checkpoints | Branch from a message. Preview a checkpoint before restoring one owned worktree and its conversation. | [Pi branches](https://github.com/earendil-works/pi/tree/main/packages/coding-agent#branching), [T3 checkpoints](https://github.com/pingdotgg/t3code/blob/f8b4c464b4760d73e0ece7e68011c738803d8b69/docs/internals/overview.md) |
| 7 | Search and project organization | Search stored messages, pin current work, and archive finished conversations without deletion. | [T3 thread sidebar](https://github.com/pingdotgg/t3code/blob/f8b4c464b4760d73e0ece7e68011c738803d8b69/docs/user/thread-sidebar.md) |
| 8 | Plan and capability views | Render native plans as editable work documents. Show each worker's actual tools, skills, model, effort, and connection failures. | [Gemini plan mode](https://geminicli.com/docs/cli/plan-mode/#collaborative-plan-editing), [Hermes toolsets](https://hermes-agent.nousresearch.com/docs/user-guide/features/tools#using-toolsets) |

Interactive terminal controls are also a concrete gap. Native Codex command tools already accept input; our Tasks view does not.
Reuse the available process protocol where possible. Do not send native process IDs to the separate monitor termination endpoint.
References: [Hermes process management](https://hermes-agent.nousresearch.com/docs/user-guide/features/tools#background-process-management), [Gemini interactive shell](https://geminicli.com/docs/tools/shell/#interactive-commands).

## Model-callable additions

Names in this table are proposed names for our app, not claims about existing upstream APIs.

| Priority | Proposed tool surface | Purpose and boundary |
| --- | --- | --- |
| First | `orchestration_task` with create, list, claim, update, submit, accept | Persistent work items, atomic claims, dependency checks, and explicit acceptance. Worker completion alone must not mark a result accepted. |
| First | `orchestration_search` | Search team messages, tool output, complaints, and accepted decisions. Return exact record links and enforce room membership. |
| First | Extend `orchestration_send` with delivery mode | Support current-turn steering and next-turn follow-up through one existing API. Preserve idempotency and stopped-agent rules. |
| Next | `orchestration_watch` and `orchestration_schedule` | Persist event rules and schedules. A script decides whether an event needs a model call. Define deduplication and restart behavior. |
| Next | `orchestration_result` | Register a worker result with files, source revision, test evidence, and review state. Reuse the task record instead of a second completion system. |
| Later | Resource claim wrapper | Expose the existing `codex-board` operations to managed agents and the UI. Do not create a second resource registry. |

The shared task reference is Claude's experimental agent teams. It documents dependency blocks and file locks for claims.
The acceptance stage above is our proposal, not a claim that Claude verifies all submitted evidence.

Hermes exposes `session_search` over stored messages. Our missing feature is search over the managed team database, not native Codex context recall.
Reference: [Hermes session search](https://hermes-agent.nousresearch.com/docs/user-guide/features/memory#session-search).
A shared knowledge view can build on accepted task results and indexed decisions before adding another memory store.

Hermes exposes `cronjob` with lifecycle operations. Its pre-check script can return `wakeAgent: false` to skip inference.
Our monitor already waits outside the model. The gap is durable schedules and event rules beyond one command's exit.
Reference: [Hermes cron](https://hermes-agent.nousresearch.com/docs/user-guide/features/cron#skipping-the-agent-entirely-wakeagent).

## Product-specific findings and limits

- T3 Chat's FAQ confirms attachments, profiles, reasoning effort, and search controls. Its public feedback board marks message branching complete. The direct browser check met a Vercel checkpoint, so no signed-in workflow was tested.
- T3 Code provides a closer coding-workspace reference. Its composer documentation covers file input, quoted response context, skills, and rendered file previews.
- Claude agent teams are experimental and off by default. Task tools depend on the session. Current documentation says the former `TeamCreate` and `TeamDelete` tools were removed.
- Pi includes conversation branches and message queues. Its subagents, plan mode, and Git checkpoint examples are extensions, not built-in features.
- OpenCode supplies undo/redo, attention events, and an experimental LSP tool. LSP availability in our configured Codex tools requires a separate check.
- Gemini CLI supplies editable plans, optional checkpoints, and interactive shell support. A native plan tool is not absent from our app.
- Hermes supplies session search, process controls, toolset configuration, and schedules. Process recovery depends on the execution backend.

Additional primary sources:

- [T3 Chat branch request marked completed](https://feedback.t3.chat/p/branch-before-any-user-message-in-a-chat).
- [Claude checkpoints and exclusions](https://code.claude.com/docs/en/checkpointing).
- [Claude background hooks](https://code.claude.com/docs/en/hooks#run-hooks-in-the-background).
- [Gemini checkpoints](https://geminicli.com/docs/cli/checkpointing/).
- [OpenCode plugins](https://opencode.ai/docs/plugins/).
- [OpenCode LSP](https://opencode.ai/docs/tools/#lsp-experimental).

Ordinary Claude asynchronous hooks do not wake an idle session. Its `asyncRewake` mode wakes on exit code 2.
Claude checkpoint restoration excludes shell changes and most subagent edits. Do not infer that one rewind restores a whole team.

## Recommended sequence

1. Shared work board and atomic claims.
2. Current-turn steering and editable follow-up queue.
3. Worker changes, evidence, and attention inbox.
4. Attachments, previews, and indexed team search.
5. Durable event rules, checkpoints, and reusable worker profiles.

These are product priorities for this user's orchestration workflow. They are not a measured ranking of harness popularity.
