# User actions and rich chat previews

Requested behavior: render Mermaid and HTML in conversations. Agents manage a
separate list of things the user must do. A user check sends one durable event to
the requesting agent. The agent can accept the result or return it with a reason.

Keep native tools, paragraph streaming, agent wake behavior, permission boundaries,
existing work items, and stored conversations unchanged.

User-task transitions: open -> review -> accepted, or review/accepted -> open.
Only the user can mark completion. Only the requesting agent or its lead can
change the task, accept, return, or cancel it. Versions reject stale decisions.
Retries retain their operation identity. Stop prevents automatic agent resumption;
review remains pending and is supplied when that agent resumes.

Mermaid and HTML previews retain source access. Incomplete streamed fences wait
until complete. HTML runs in a static isolated frame with no application access.
Invalid diagrams show an error and their source.

Verification: isolated runtime and HTTP tests, native tool schema check, browser
render and isolation tests, and desktop/narrow-screen inspection.

Status: implemented and verified in the task worktree. Local deployment follows.

## Verified behavior

- User-task contract: 11 tests pass. These include real HTTP token/origin checks,
  owner/lead permissions, stale versions, concurrent retries, return and acceptance,
  wake after a final answer, restart persistence, and explicit Stop/resume.
- Existing runtime, canvas, workspace, and concurrency contracts: 91 tests pass.
- Eight browser suites pass against the production build. New suites cover task
  states and previews, including 320/390 px task layouts and narrow previews.
- Installed Codex accepts all tool schemas. Native steering, forks, and terminal
  controls pass their existing isolated checks with no external model request.
- Independent review found SVG animation navigation and lost document attributes.
  Both are corrected and covered by browser regressions. The parent inspected
  the source changes and desktop/mobile screenshots.

The requesting agent uses `orchestration_user_task`. Existing native threads use
its documented `orchestration_send` workspace route. The user completion endpoint
cannot accept or create tasks. The inbox contains open user tasks.

Mermaid 11.17.2 loads on demand. HTML preserves document classes, inline styles,
and SVG references inside the document. Scripts, remote resources, forms,
frame navigation, and SVG animation are disabled. Source downloads retain the
original text. These are static previews, without JavaScript execution.

Browser task checks use deterministic HTTP responses; backend checks exercise
real SQLite and HTTP delivery. Preview checks exercise real runtime events and
HTTP streams. No production model turn is part of these checks.

Latest browser evidence:

- `/var/folders/29/8pytxrvn6qlcm4384bmy9n6m0000gn/T/codex-rich-preview-ui-FDUJ73`
- `/var/folders/29/8pytxrvn6qlcm4384bmy9n6m0000gn/T/codex-user-tasks-ui-3B5Jha`

