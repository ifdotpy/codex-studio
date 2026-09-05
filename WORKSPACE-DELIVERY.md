# Orchestration workspace delivery

Owner request: implement all recommendations in FEATURE-GAPS.md, 2026-09-06.
Baseline: 01c61fb. Preserve native Codex tools, managed wakes, room privacy, complaints, and permission enforcement.

## Acceptance scope

- Shared work board: dependencies, atomic claims, evidence submission, explicit acceptance, model tools.
- Current-turn steering and an editable, cancellable, ordered message queue.
- Worker changes, line comments, result files, image/PDF/HTML previews, and attachments.
- Global attention inbox with grouped notifications and source navigation.
- Conversation branches and previewed checkpoints for isolated worker worktrees.
- Full message search, project grouping, pins, archive, and restored conversations.
- Editable plans, actual capability inventory, and reusable worker profiles.
- Interactive command monitors, process input, cancellation, and log downloads.
- Durable schedules and event/file watches with script checks before model calls.
- Existing resource-board operations exposed through managed tools and the UI.

## Verification

Use isolated SQLite, repositories, process fixtures, and browser profiles.
Test cross-team access, duplicate claims, stale edits, stopped owners, retries, restart outcomes, and file boundaries.
Inspect desktop and narrow-screen renders. Retain existing runtime and browser regressions.
Verify native protocol operations separately where fixtures cannot prove compatibility.
Deploy only after checks pass and existing user work is idle.

Status: implemented, integrated into local main, and deployed at http://127.0.0.1:4620.

## Implementation and evidence

All acceptance scope items above are implemented. The web README documents the controls.
Native tools, permissions, worker wakes, complaints, and room privacy retain their existing paths.

Six model tools are added: orchestration_task, orchestration_result,
orchestration_search, orchestration_watch, orchestration_resource, and
orchestration_monitor_input. Existing send accepts queue or steer. Spawn accepts
profiles. Monitor accepts interactive terminals. Older threads use a workspace
route through orchestration_send because resume retains their original tool schema.

Verified in the task worktree:

- Runtime contract: 42 passing tests.
- Canvas contract: 17 passing tests.
- Workspace contract: 27 passing tests.
- Concurrent lifecycle regressions: 5 passing tests.
- Six browser suites: product, live chat, background history, chat controls,
  background controls, and workspace. Desktop and narrow screens pass.
- Installed codex-cli 0.153.4 accepts the tool schemas and capability discovery.
  Real command/exec supports terminal input, resize, output, and exit.
- Real active turn/steer and thread/fork pass with a local Responses fixture.
  The fork includes its selected turn and excludes a subsequent turn.
  The test observes three local provider requests and no external requests.
- Independent review found five lifecycle defects. Each has a regression and a
  verified correction. The parent inspected the implementation and rendered UI.

Browser checks use isolated state and deterministic providers. They do not claim
cloud-model quality. The background controls suite verifies HTTP requests. The
backend and native protocol checks provide execution evidence.

## Limits

- Direct terminal controls operate on managed command/exec monitors. Native Codex
  command input uses a labelled request to the agent.
- Forks use complete native turns. Restore requires an isolated idle worker
  worktree and a matching preview. Ignored files and external services are excluded.
- Codex selects native tools. The UI lists managed definitions, discovered skills
  and MCP tools, and observed native calls. A complete native inventory is unavailable.
- Restart marks interrupted commands unknown and does not repeat them.
- Desktop alerts require explicit browser permission. System notification delivery
  is unmeasured and depends on browser and operating-system settings.
- No remote Git push or external package publication is part of this delivery.

## Local deployment

Implementation commit: `e1a74fd`.
The server uses the production bundle from local main. A read-only headless browser
opened the workspace sections at desktop and 390 px, verified served script bytes
against the local bundle, and reported no page errors. Both existing agent ids and
statuses remained unchanged. No production model turn or monitor was started.

Deployment evidence: `/var/folders/29/8pytxrvn6qlcm4384bmy9n6m0000gn/T/codex-workspace-deployed-TkKq6z`.
Database backup: `~/.local/state/codex-agents/backups/canvas-before-workspace-20260906-010707.sqlite3`.
Process id at verification: `19718`.
