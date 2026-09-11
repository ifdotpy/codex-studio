# User guidance and mobile team access

## Scope and sources

Read the existing Studio SQLite database in read-only mode on 2026-09-08.
Select orchestrators by persisted `isLead`, including deleted records.
The snapshot contains 127 `user` events for these orchestrators:
101 direct chat messages, one native question answer, and 25 maintenance messages
from the Studio implementation agent. The latter are not user preferences.
Cross-check the user message table and lead transcript items. Additional transcript
text consists of orchestration events, complaint reminders, or copies of these messages.
No full conversation export is added to the repository. No agent receives a message.

## Extracted rules

| Source message ID | User instruction | Destination |
|---|---|---|
| `ab70f007-daf1-40fd-8d28-631cabe92492`, `e4e2c8b6-62be-4c71-b2ee-64e6f80b5eec` | Review subagent commits and own the result. | Orchestrator responsibility and review. |
| `97c7e568-6967-4dd3-aa7c-4f5e2a20330c`, `67e9a158-409d-4a14-b1a1-ff092f849621` | Delegate implementation; own design, quality, and speed. | Orchestrator work allocation. |
| `61113f45-a204-421e-9be8-c9370fed7d1b`, `39359001-4ea7-44ee-9bfb-315c751e16d6`, `f7d197e0-7d7f-41da-9cbe-e848e3d5ac1c` | Keep independent work active during builds and other waits. | Orchestrator work allocation. |
| `967ef90a-5e49-4ba7-85e0-93a5047be068` | Resume subagents; report recovery that loses context. | Orchestrator recovery; subagent continuation evidence. |
| `e8c59984-1bf3-4037-8262-fa278a4e38da` | Improve pipelines and work methods for speed and quality. | Orchestrator decisions. |
| `92f584e8-3ecd-4da7-99c5-e61f481b0284`, `35affb27-015b-4dd7-8839-283db3d503c9`, `b81e95ed-ed0b-4b8d-918b-963fc2f0f43e` | Show each workstream and the current stage visually; avoid tabs. | Orchestrator progress and shared panel guide. |
| `2921f136-d9cb-4ce4-a0bb-9d82a69da9b3`, `187c1dec-6f04-49e4-a4b4-1526f2164783`, `e677ee7b-57e3-4325-b852-42c590f49926` | Provide a usable local application before the requested merge. | Orchestrator acceptance; subagent checks through the actual caller. |
| `ee62f927-e5c8-4a56-a334-4fb662521791`, `6f11877a-2faa-486d-a9ca-48958c979b9a` | Inspect existing code before building a replacement. | Both role skills. Chromium-specific architecture stays project-specific. |
| `8d6f6e6f-c670-4bab-82fb-e956ee14ddcc` | Preserve accepted design decisions when saving the document. | Orchestrator decisions. |

Task-specific instructions stay scoped. The Attar request for at least five workers
is not a global minimum. The showroom request to investigate without subagents
still applies until its later permission. Device erasure permission, cloud instance
choices, empty login fields, and LHS hardware requirements are not shared skill rules.
A confirmed-defect repair instruction applies only when the user authorizes repairs.

## Behavioral review

- An investigation without subagents stays an investigation.
- An implementation with independent tasks uses workers while its build waits.
- A small task does not create five artificial assignments.
- An uncertain worker command uses receipt recovery before any retry.
- A worker sends requests to the orchestrator, which decides whether to contact the user.
- A successful fixture does not establish that the requested local application works.
- Primary workstream states remain visible together; secondary detail may use tabs.

## Mobile behavior

The Team button opens the existing worker list at mobile widths. Search, status
groups, and completed workers use the desktop implementation. Selecting a worker
opens its chat and closes the drawer. Back to lead returns to the orchestrator.
Worker selection and separate drafts survive a page reload. Worker account selection
is disabled. The drawer follows the mobile viewport height and safe-area insets.
The conversation sidebar still lists orchestrators on mobile.

## Checks

The web build and both skill validators pass. The role skill contract passes all
eight tests. Team navigation passes desktop, tablet, 320px, and 390px checks.
The mobile client passes 320px, 390px, and 760px checks, including chat request
recovery and the desktop transition.

Visual inspection found a clipped Back to lead button in the mobile worker header.
The header now has space for that button, the title, and the status. The final
browser check asserts that the button stays inside the header. The final 320px
worker screenshot confirms that correction.

Logs: `/tmp/studio-mobile-team-build.log`, `/tmp/studio-mobile-team-ui.log`,
`/tmp/studio-mobile-client-ui.log`, `/tmp/studio-user-skill-role-check.log`.
Team screenshots: `/var/folders/29/8pytxrvn6qlcm4384bmy9n6m0000gn/T/codex-team-navigation-RBr5ki`.
The team fixture deliberately returns HTTP 503 for sync to control agent states.
Its screenshots therefore show that expected error; the real sync mobile test passes.
Two obsolete mobile assertions now match the existing Messages and subagent-defaults controls.
No live backend restart, active-agent interruption, or model request occurs.
