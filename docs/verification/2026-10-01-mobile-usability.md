# iPhone usability audit

Screenshots, geometry and logs are stored outside the repository in `/Users/igor/.local/share/codex-studio-evidence/2026-10-01-mobile-usability` (19 MB). Run `tests/mobile-usability-ui.mjs` with `STUDIO_EVIDENCE_DIR` to regenerate them.

Date: 2026-10-01. Baseline: `e98b69a`. Source fixes: `a1ee301` and `5a4279a` (branch `6c2e152` and `71c9996`), rebased on `af5aad2`.

The browser uses Playwright WebKit 2359 with iPhone 13 emulation. Phone sizes: 375 × 812, 390 × 844, 430 × 932. Desktop: 1440 × 960.

All browsers are hidden and headless. All server state uses temporary directories. This audit does not access the live server.

## Ranked findings and fixes

P1 blocks an action or hides its state. P2 increases input errors or makes content harder to read.

| Rank | Impact and reproduction | Change | Before | After |
| --- | --- | --- | --- | --- |
| P1.1 | Open the keyboard with a long draft. Eight textarea rows put Send below the visible area. | Limit the textarea to 80 px while the keyboard is open. Keep the draft scrollable. | [375 px](/Users/igor/.local/share/codex-studio-evidence/2026-10-01-mobile-usability/before/375-keyboard-long-draft.png) | [375 px](/Users/igor/.local/share/codex-studio-evidence/2026-10-01-mobile-usability/after/375-keyboard-long-draft.png) |
| P1.2 | Open Messages in standalone mode. The drawer header and footer ignore the top and bottom safe areas. | Apply the safe areas to portal content. Keep the drawer body scrollable inside the visible viewport. | [Standalone](/Users/igor/.local/share/codex-studio-evidence/2026-10-01-mobile-usability/before/375-messages-standalone.png), [keyboard](/Users/igor/.local/share/codex-studio-evidence/2026-10-01-mobile-usability/before/375-messages-keyboard.png) | [Standalone](/Users/igor/.local/share/codex-studio-evidence/2026-10-01-mobile-usability/after/375-messages-standalone.png), [keyboard](/Users/igor/.local/share/codex-studio-evidence/2026-10-01-mobile-usability/after/375-messages-keyboard.png) |
| P1.3 | Open a chat at 375 px. The project name and mode switch leave the status as “M.” | Put status before project. Move the phone mode switch to Chat settings. Keep the desktop switch in the header. | [Header](/Users/igor/.local/share/codex-studio-evidence/2026-10-01-mobile-usability/before/375-chat.png) | [Header](/Users/igor/.local/share/codex-studio-evidence/2026-10-01-mobile-usability/after/375-chat.png), [settings](/Users/igor/.local/share/codex-studio-evidence/2026-10-01-mobile-usability/after/375-settings.png) |
| P2.1 | Chat actions are 22 px. Dictation is 30 px. Tool summaries and prompt navigation are 28 px and 22 px. | Set phone buttons, tabs, menu options, disclosures, and form fields to a 44 px minimum target. | [Composer](/Users/igor/.local/share/codex-studio-evidence/2026-10-01-mobile-usability/before/375-composer.png), [history](/Users/igor/.local/share/codex-studio-evidence/2026-10-01-mobile-usability/before/375-tool-history.png) | [Composer](/Users/igor/.local/share/codex-studio-evidence/2026-10-01-mobile-usability/after/375-composer.png), [history](/Users/igor/.local/share/codex-studio-evidence/2026-10-01-mobile-usability/after/375-tool-history.png) |
| P2.2 | Project actions are 40 px. Larger targets can cover the project text. | Reserve space for visible project and chat menus. Keep the actions available without hover. | [Project tree](/Users/igor/.local/share/codex-studio-evidence/2026-10-01-mobile-usability/before/430-chat-list-project-tree.png) | [Project tree](/Users/igor/.local/share/codex-studio-evidence/2026-10-01-mobile-usability/after/430-chat-list-project-tree.png) |
| P2.3 | Team filters are 25.5 px. Back to main agent is 28 px. A fixed 84 px header cannot contain larger controls. | Enlarge Team controls. Let the worker header grow. Keep Back to main agent inside it. | [Team](/Users/igor/.local/share/codex-studio-evidence/2026-10-01-mobile-usability/before/375-team-workers.png), [worker](/Users/igor/.local/share/codex-studio-evidence/2026-10-01-mobile-usability/before/375-worker-chat.png) | [Team](/Users/igor/.local/share/codex-studio-evidence/2026-10-01-mobile-usability/after/375-team-workers.png), [worker](/Users/igor/.local/share/codex-studio-evidence/2026-10-01-mobile-usability/after/375-worker-chat.png) |
| P2.4 | Settings fields are 36 px. Model descriptions compete with names in two narrow columns. | Enlarge fields. Put each description below its model name. Bound menus and dialogs to the visible viewport. | [Account](/Users/igor/.local/share/codex-studio-evidence/2026-10-01-mobile-usability/before/375-account-picker.png), [model](/Users/igor/.local/share/codex-studio-evidence/2026-10-01-mobile-usability/before/375-model-picker.png) | [Account](/Users/igor/.local/share/codex-studio-evidence/2026-10-01-mobile-usability/after/375-account-picker.png), [model](/Users/igor/.local/share/codex-studio-evidence/2026-10-01-mobile-usability/after/375-model-picker.png) |
| P2.5 | Message actions, task actions, and analytics tabs use desktop control sizes. | Apply the same phone targets and scroll limits to these panels. | [Messages](/Users/igor/.local/share/codex-studio-evidence/2026-10-01-mobile-usability/before/375-messages-for-you.png), [task](/Users/igor/.local/share/codex-studio-evidence/2026-10-01-mobile-usability/before/375-background-task-details.png), [analytics](/Users/igor/.local/share/codex-studio-evidence/2026-10-01-mobile-usability/before/375-analytics.png) | [Messages](/Users/igor/.local/share/codex-studio-evidence/2026-10-01-mobile-usability/after/375-messages-for-you.png), [task](/Users/igor/.local/share/codex-studio-evidence/2026-10-01-mobile-usability/after/375-background-task-details.png), [analytics](/Users/igor/.local/share/codex-studio-evidence/2026-10-01-mobile-usability/after/375-analytics.png) |

## Measured result

The complete matrix contains 106 screenshots per version. The JSON files record target sizes, hit tests, page width, header bounds, and drawer bounds.

- [Before measurements](/Users/igor/.local/share/codex-studio-evidence/2026-10-01-mobile-usability/before/measurements.json)
- [After measurements](/Users/igor/.local/share/codex-studio-evidence/2026-10-01-mobile-usability/after/measurements.json)
- [Screenshot index](/Users/igor/.local/share/codex-studio-evidence/2026-10-01-mobile-usability/index.md)

At 375 px, the open chat changes from 14 targets below 44 px to zero. The project tree changes from 28 to zero. The model picker changes from 26 to zero.

All phone scenarios have zero targets below 44 px after the fix. The page width does not exceed the viewport at any phone width. Wide code remains inside its own horizontal scroll area.

The keyboard test gives the visual viewport a 54 px offset and 390 px height. Send remains above its 444 px bottom with short and long drafts. Messages and Activity drawers fit these bounds.

The standalone test supplies 47 px top and 34 px bottom safe areas. It checks the same portal layout that receives the native safe-area values.

Desktop screenshots cover every affected panel. Main also removes transcript avatars and updates progress and Team rooms, as requested separately. Common desktop control widths and heights are unchanged between the two versions. The desktop mode switch remains in the header.

## Navigation and state

Counts start from an open lead chat. They exclude text input and scrolling.

| Action | Taps | Result |
| --- | --- | --- |
| Open a worker | 2 | Team, worker card |
| Return to the lead | 1 | Back to main agent |
| Switch chats | 2 | Conversations, chat row |
| Open Chat settings | 1 | Header settings button |
| Change phone agent mode | 2 | Settings, mode switch |
| Change desktop agent mode | 1 | Header mode switch |
| Open a model picker | 3 | Settings, model settings, model field |
| Open Messages | 1 | Header Messages button |
| Read a background command | 2 | Activity, task row |

Tap counts for worker and chat navigation do not increase. The phone mode switch takes one additional tap to preserve the chat status.

The audit checks failed tool output, worker errors, pending send state, and offline state. The offline screen shows that saved chats and drafts remain available. The audit does not send a model request.

The activity bar preserves `e98b69a`: only monitors and background commands. Worker navigation remains in Team. In-turn commands remain in turn history.

## Scope and limits

The fixture extends the existing isolated `simple-ui-fixture.py` data. It uses the visual viewport simulation contract from `mobile-keyboard-ui.mjs`.

Keyboard and standalone screenshots simulate geometry. They do not prove physical iPhone keyboard behavior or an installed home-screen app.

The existing phone UI excludes the terminal dock. Activity exposes background commands. The desktop terminal screenshot uses a completed shell record and recorded output, without a real shell.

Sync, initial load, bundles, and data flow belong to `mobile-perf`. This audit does not use screenshot delay as a performance measurement.

## Checks

Pass:

- WebKit audit: 106 before screenshots and 106 after screenshots.
- Web build: TypeScript and Vite.
- UI tests: `team-navigation-ui`, `team-panel-ui`, `worker-overview-ui`, `project-tree-ui`, `model-picker`, `analytics-ui`.
- UI tests: `messages-focused-ui`, `background-controls-ui`, `terminal-dock-ui --fixture-only`.
- UI tests: `turn-history-ui`, `prompt-navigation-ui`, `composer-stability-ui`, `team-room-bounds-ui`.
- Source: `git diff --check`. Prettier passes for the added audit and changed CSS and viewport hook.

The additional mobile tests pass: request deadlines, send data, loading, and startup. Startup: 6873 ms cold, 710 ms cached draft.

The required `npm --prefix web run test:mobile` fails at the sync fixture. Its earlier stages pass: nine state tests, phone client, keyboard, message delivery, and both prefetch tests.

The lead permits submission with these existing main failures on 2026-10-01. Sync fixtures and data flow remain with `mobile-perf`.

| Test and failure line | UX branch | Clean main `1357db8` | Evidence |
| --- | --- | --- | --- |
| `chat-settings-autosave-ui.mjs`, assertion in `accounts-ui-smoke.mjs:651` | 508.2 ms open, fails 150 ms limit | 326.9 ms open, same assertion | [UX](/Users/igor/.local/share/codex-studio-evidence/2026-10-01-mobile-usability/checks/rebased-settings.log), [main](/Users/igor/.local/share/codex-studio-evidence/2026-10-01-mobile-usability/checks/main-settings.log) |
| `mobile-sync-browser.mjs:115` | Projection callback timeout, 30 seconds | Same callback timeout and line | [UX](/Users/igor/.local/share/codex-studio-evidence/2026-10-01-mobile-usability/checks/sync.log), [main](/Users/igor/.local/share/codex-studio-evidence/2026-10-01-mobile-usability/checks/main-sync.log) |
| `mobile-sync-cached-browser.mjs:117` | Cached state timeout, 30 seconds | Same cached state timeout and line | [UX](/Users/igor/.local/share/codex-studio-evidence/2026-10-01-mobile-usability/checks/sync-cached.log), [main](/Users/igor/.local/share/codex-studio-evidence/2026-10-01-mobile-usability/checks/main-sync-cached.log) |
| `mobile-send-reliability-ui.mjs:373` | Failed-draft restore timeout, 15 seconds | Same draft timeout and line | [UX](/Users/igor/.local/share/codex-studio-evidence/2026-10-01-mobile-usability/checks/send-ui.log), [main](/Users/igor/.local/share/codex-studio-evidence/2026-10-01-mobile-usability/checks/main-send-ui.log) |

The comparison uses a clean Git archive with the same dependencies. Tests run serially. The assertions and limits remain unchanged.

[Final WebKit log](/Users/igor/.local/share/codex-studio-evidence/2026-10-01-mobile-usability/checks/after.log). [Room bounds log](/Users/igor/.local/share/codex-studio-evidence/2026-10-01-mobile-usability/checks/room-bounds.log). [Final Messages log](/Users/igor/.local/share/codex-studio-evidence/2026-10-01-mobile-usability/checks/messages-final.log). [Build log](/Users/igor/.local/share/codex-studio-evidence/2026-10-01-mobile-usability/checks/build.log).

[Mobile suite log](/Users/igor/.local/share/codex-studio-evidence/2026-10-01-mobile-usability/checks/mobile-suite.log). [Startup log](/Users/igor/.local/share/codex-studio-evidence/2026-10-01-mobile-usability/checks/startup.log).

The initial mobile run fails because the prefetch fixture returns 100 entities for a request for 500. This also fails on clean main. `mobile-perf` supplies the pagination correction in `72609fd` (cherry-picked as `c50b49d`).

The repeated mobile suite passes prefetch: first switch 135.7 ms, revisits 64 ms and 126.8 ms. It fails the next sync fixture at `mobile-sync-browser.mjs:115`, before layout imports. The table records the clean-main comparison.

Reproduce after screenshots with:

```sh
npm --prefix web run build
node tests/mobile-usability-ui.mjs --phase=after
```

Use `MOBILE_UI_DIST` with built baseline assets to reproduce the before screenshots.
