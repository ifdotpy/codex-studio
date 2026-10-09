# UI preference sync

Base: `70667188334e89940e46c9819a40e1f43fd30cab`.

Each row lists the previous storage and the chosen scope.
User preferences go to every reachable paired server.
Server preferences stay on their own server.

| Choice                                                                      | Previous storage                                                                                                                        | Scope                                                                         |
| --------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------- |
| Theme (`auto`, `light`, `dark`)                                             | `codex-studio-preferences-v1.theme`, Mantine cache `mantine-color-scheme-value`                                                         | User                                                                          |
| Original or custom typography                                               | `codex-studio-preferences-v1.typography`                                                                                                | User                                                                          |
| Sidebar text size                                                           | `codex-studio-preferences-v1.sidebarFontSize`                                                                                           | User                                                                          |
| Main text size                                                              | `codex-studio-preferences-v1.mainFontSize`                                                                                              | User                                                                          |
| Font family                                                                 | `codex-studio-preferences-v1.fontFamily`                                                                                                | User                                                                          |
| Original or custom content layout                                           | `codex-studio-preferences-v1.contentLayout`                                                                                             | User                                                                          |
| Chat content width ratio                                                    | `codex-studio-preferences-v1.contentWidth`                                                                                              | User (Appearance intent, distinct from panel geometry)                        |
| Sidebar shortcut                                                            | `codex-studio-preferences-v1.sidebarShortcut`                                                                                           | User (explicit Appearance choice)                                             |
| Message avatars                                                             | `codex-studio-preferences-v1.showMessageAvatars`                                                                                        | User                                                                          |
| Project, folder, team and chat order                                        | `/api/projects` (`reorder`), `runtime_sidebar_order`, `workspace.current.sidebarOrder`; legacy local order cache from `useSidebarOrder` | Server, existing sync entity                                                  |
| Project and folder collapse                                                 | `codex-project-tree:<stateDir>`                                                                                                         | Server, each entry has its own field version                                  |
| Project compact mode and show-more state                                    | `codex-project-compact:<stateDir>`                                                                                                      | Server                                                                        |
| Shell logical project collapse                                              | `studio-logical-project-collapsed`                                                                                                      | User                                                                          |
| Shell logical project compact mode                                          | `studio-logical-project-compact`                                                                                                        | User                                                                          |
| Legacy shell compact mode                                                   | `studio-server-project-compact-v1`                                                                                                      | User, migrated                                                                |
| Server alias                                                                | `studio-local-server-alias-v1`, `studio-paired-servers-v1[].alias`, server discovery aliases                                            | User, keyed by sync workspace identity                                        |
| Server label and registry order                                             | `studio-paired-servers-v1[].label`, array order                                                                                         | User, credentials and pairing stay local                                      |
| Progress visibility                                                         | `codex-progress-hidden:<stateDir>`                                                                                                      | Server, explicit choice overrides the phone width default                     |
| Worker excerpt disclosures                                                  | `codex-worker-disclosures`                                                                                                              | Server                                                                        |
| Transcript tool disclosures                                                 | `studio-turns:<stateDir>:<chat>:tools-v3`                                                                                               | Server                                                                        |
| Prompt bookmarks                                                            | `studio-prompt-bookmarks:<stateDir>:<chat>`                                                                                             | Server                                                                        |
| Project names, folder names and hierarchy                                   | `/api/projects`, `runtime_projects`, `project` entities                                                                                 | Server, already synced                                                        |
| Chat folder assignment, pin, archive, hide                                  | Organization APIs, agent and chat entities                                                                                              | Server, already synced                                                        |
| Project locations, aliases and project compact property                     | Project APIs, project entities                                                                                                          | Server, already synced                                                        |
| Sidebar width and other panel geometry                                      | Layout state, where present                                                                                                             | Device                                                                        |
| Terminal height                                                             | `codex.terminal.height`                                                                                                                 | Device                                                                        |
| Terminal visibility and selected terminal                                   | `codex.terminal.open`, `codex.terminal.selected`                                                                                        | Device                                                                        |
| Sidebar visibility                                                          | `codex-sidebar-collapsed`; shell component state                                                                                        | Device                                                                        |
| Team panel visibility                                                       | `codex-team-open`                                                                                                                       | Device                                                                        |
| Selected chat and server                                                    | `codex-mobile-opened`, other chat selection cache, `studio-selected-server`                                                             | Device                                                                        |
| Last server used for a project                                              | `project-last-server:<project>`                                                                                                         | Device, navigation history                                                    |
| Scroll position and history anchor                                          | `studio-chat-scroll:*`, transcript `:window-anchor`                                                                                     | Device                                                                        |
| Open dialogs, search, temporary expansions                                  | React state                                                                                                                             | Device                                                                        |
| Drafts, attachments and dictation text                                      | Draft journal, `studio-answer-draft:*`, form draft keys, dictation cache                                                                | Device for this preference feature; existing draft transport stays intact     |
| Removed pending message flags and saved copies                              | `studio-removed-messages:*`, `:copies`, `:restored`                                                                                     | Device, explicit removal and retry recovery; these are not chat hide settings |
| Read state and team message seen state                                      | Existing read-state API and `TeamChats` seen cache                                                                                      | Existing behavior, not a visual preference                                    |
| Pending command IDs, pairing attempts, operation receipts and outbox fences | `request-id:*`, operation-specific pending keys, pairing cache, upload fences                                                           | Device                                                                        |
| Runtime and cost caches                                                     | Sync database identity, transcript cache, progress cache, session cost cache                                                            | Device                                                                        |
| Account, model, permission and worker concurrency choices                   | Existing server APIs and their entities                                                                                                 | Server, already synced; not UI preference fields                              |

`web/src/servers/storage.ts` adds the server namespace to local keys in server views.
The offline cache retains the previous local keys for the first render.

## Storage and delivery

The `uiPreferences` entity collection has `user` and `server` rows.
Each field has a JSON value, a timestamp in milliseconds, and a writer identity.
A map entry is a separate field, so two project collapse edits do not replace each other.
Timestamp, writer identity, then canonical JSON decide equal versions.
The merge is idempotent, including after a lost response.

`POST /api/sync/preferences` uses the existing permission and schema checks.
The existing entity pull and state resource stream deliver changes.
The shell connects to each paired server and merges user fields from all servers.
A retry retains field versions. An unavailable server receives the cache after reconnection.
Credentials never enter the preference entity.

A migration marker prevents repeated imports from legacy local keys.
Legacy fields have timestamp zero. Explicit edits have a current timestamp.
The backend includes inert preference JSON in its HTML response.
A fresh browser reads that JSON before React renders.
An existing browser keeps its local cache for the first render.

## Check evidence

The backend tests cover validation, independent fields, stale writes, equal timestamps,
scope separation, duplicate requests, and SQLite close and reopen.
The client tests cover merge, legacy migration, independent first edits, stale controls,
two-server merge, reconnection, and recovery after a lost response.
The browser test uses isolated contexts and a fixture backend on a dynamic port.
It checks live theme, text size, project order, project collapse, and server alias.
A fresh phone context checks Appearance before React renders, then the project state.

Five existing browser failures also occur on the base commit.
They are the settings account picker, the main multi-server route test,
and three discovery recovery tests (uncertain identity and two chat-count checks).
The baseline checkout is `/tmp/prefs-sync-baseline-20261009`.
The baseline log is
`/Users/igor/.local/state/codex-agents/monitor-logs/d6daf17a-27f7-5b3c-9aa4-58cba3bc133e.log`.
The feature regression log is
`/Users/igor/.local/state/codex-agents/monitor-logs/b5a87616-6961-5b4e-b750-51571265a1b8.log`.

Final checks:

- Web build and generated contract check: pass.
- Client unit tests: 610 passed, one skipped, 110 files.
- Strict runtime mypy: pass, 299 files.
- Backend tests: 105 passed (`sync.test_preferences`, `sync.test_router`,
  `sync.test_contracts`, `tests.test_core`).
- Final preference, project tree, sidebar drag, sidebar status, and paired sidebar
  browser tests: five passed.
- Staged Oxlint and Oxfmt checks: pass.
- Pre-commit staged-content fixtures: pass.

Final browser evidence:
`/var/folders/29/8pytxrvn6qlcm4384bmy9n6m0000gn/T/studio-preferences-sync-44E9wz`.
The folder contains `context-b.png`, `context-c-phone.png`, `state.json`,
and `fixture.log`. The phone screenshot shows the shared order and collapse state.
The final browser log is
`/Users/igor/.local/state/codex-agents/monitor-logs/8c18ef98-6758-567b-9a1c-cb0d37df04b3.log`.
Build and unit logs: `/tmp/prefs-sync-final-build.log` and
`/tmp/prefs-sync-final-unit.log`.
Backend and runtime check log:
`/Users/igor/.local/state/codex-agents/monitor-logs/7d4821aa-e581-5380-9d4c-933d8636dcda.log`.
No installed app, live backend, or port 4620 was used.
