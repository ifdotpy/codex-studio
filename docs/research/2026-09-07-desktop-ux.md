# Desktop interface audit, 2026-09-07

This audit compares readable JavaScript in the installed desktop application with Codex Studio source.
It records source evidence. It does not prove live operation or account access.

## Inspected inputs

| Input                | Identity                                                           |
| -------------------- | ------------------------------------------------------------------ |
| Installed archive    | `/Applications/ChatGPT.app/Contents/Resources/app.asar`            |
| Archive package      | `openai-codex-electron`                                            |
| Package version      | `26.818.41509`                                                     |
| macOS bundle version | `6962`                                                             |
| Archive SHA-256      | `8eb91bd9efbf9a4dd04b9b0afdbfcb4e0bab5da18c1919ad74ca327c00c7e791` |
| Studio base commit   | `0638f04d4307bd48a2fcb930b496a05b37a13d17`                         |

The audit uses `@electron/asar` to inspect selected archive entries.
Temporary extracts remain outside the repository, under `/tmp/desktop-feature-audit` and `/tmp/chatgpt-studio-ux-20260907`.
The repository contains no copied application code, bundles, or assets from this inspection.
Archive paths below identify local entries, not public source links.

## Prompt navigation and bookmarks

Archive entry: `webview/assets/thread-user-message-navigation-rail-app-856jNa2B.js`.

- Each user message has a navigation button and a position label.
- The visible message receives `aria-current`.
- Pointer and focus events show message previews.
- A pointer drag can select successive messages and move the conversation immediately.
- `isBookmarked` controls the bookmark marker and accessible label.
- `onBookmarkChange` supports bookmark addition and removal from the preview.
- `onPreviewItem` and `onRevealItem` support content that requires a separate fetch or reveal step.

These mechanisms support a prompt index and bookmarks in Studio.
They do not establish the desktop bookmark storage policy.
The audit does not establish a desktop feature that pins the current prompt above the conversation.

## Agents, questions, and recaps

| Area               | Archive entries and identifiers                                                                                                       | Source behavior                                                                                                                                                      |
| ------------------ | ------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Subagent list      | `webview/assets/subagent-panel-D3Kd5g1l.js`; `webview/assets/chatgpt-thread-visibility-D72EgEdP.js`, `KC`, `qC`                       | Active and Done sections. Task or status previews. Details contain the delegated task and final response. Missing responses and branch changes have explicit states. |
| Background agents  | `webview/assets/app-initial-DwVrCWuo.js`, `composer.backgroundSubagents.*`                                                            | Collapsible agent summary, model labels, agent mentions, and an action to stop descendant agents. `isBackgroundSubagentsEnabled` affects display.                    |
| Questions          | `webview/assets/app-initial-DwVrCWuo.js`, `userInputResponse`, `requestUserInputAutoResolution`                                       | Options, freeform answers, answer history, a scheduled deadline, and a Snooze action. Snooze disables the timeout.                                                   |
| Conversation recap | `webview/assets/app-initial-DwVrCWuo.js`, `reasoning_recap`; `webview/assets/chatgpt-thread-visibility-D72EgEdP.js`, `reasoningRecap` | A supplied recap becomes an expandable summary. `hide_all` and `keep_inline` control display.                                                                        |

The renderer search did not establish a `request_user_input_async` implementation or continued execution during an unanswered question.
The question timeout code alone does not prove either behavior.
The recap code does not prove local summary generation.
`webview/assets/codex-home-announcements-C0pNDVJk.js` contains a separate Computer History weekly recap prompt.

## Native features and access limits

| Area         | Archive entry                                                                                   | Source behavior and access limits                                                                                                                                                                                   |
| ------------ | ----------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Computer use | `webview/assets/computer-use-settings-Bj7lp_m7.js`                                              | Plugin install/manage controls, browser extension state, permitted apps, permission removal, and click sounds. Host, platform, installation, and policy affect access through `computerUseAvailability`.            |
| Voice        | `webview/assets/voice-settings-DUICHekT.js`                                                     | Voice and microphone selection, global shortcut, and optional screen context. Account/workspace access checks have loading, error, and unavailable states.                                                          |
| Dictation    | `webview/assets/global-dictation-page-CBJ0SrX0.js`; `webview/assets/voice-settings-DUICHekT.js` | Hold/toggle shortcuts, transcription state, retry, and a dictionary. Local recording recovery supports transcript copy, retranscription, download, and deletion. Capability and configuration checks affect access. |

No live voice, dictation, or computer-use check occurred.
No corresponding interface appeared in the inspected Studio renderer and native source search.
These features require native capabilities and access checks. They are outside this implementation pass.

## Studio comparison and selected work

The comparison describes Studio before the concurrent changes in this task.

| Studio source                                                 | Existing behavior                                                                               | Selected improvement                                                                                                                      |
| ------------------------------------------------------------- | ----------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------- |
| [Conversation.tsx](../../web/src/components/Conversation.tsx) | Conversation content and controls                                                               | `PromptNavigator` with a prompt list, bookmarks, previous/next actions, and a pinned prompt. The pinned prompt is a Studio design choice. |
| [App.tsx](../../web/src/App.tsx)                              | Team rows show name, status, model, and error. Completed workers have a separate group.         | Clear Team groups and an explicit return to the lead conversation.                                                                        |
| [Requests.tsx](../../web/src/components/Requests.tsx)         | A generic request notice opens a modal with questions and options.                              | Inline controls for genuine Studio asynchronous questions. Preserve separate approval behavior.                                           |
| [codex_runtime.py](../../scripts/codex_runtime.py)            | Completed `agentMessage.questions` create `agent/asyncQuestion`. Answers enter the agent queue. | Reuse this existing asynchronous mechanism. Do not infer it from desktop timeout behavior.                                                |
| [Activity.tsx](../../web/src/components/Activity.tsx)         | Tool groups include read summaries and active/failure counts.                                   | Turn recap remains a future option. No recap implementation is part of this pass.                                                         |

Implementation status: **VERIFIED IN ISOLATED BROWSER FIXTURES**.
The selected changes are independent Studio implementations.

- `npm --prefix web run build`: TypeScript and production bundle pass.
- `node tests/prompt-navigation-ui.mjs`: prompt index, bookmarks after reload, search, previous/next navigation, scroll, chat changes, and narrow layout pass.
- `node tests/team-navigation-ui.mjs`: state groups, filters, completed search, selected worker, return to lead, and chat isolation pass.
- `node tests/questions-ux-ui.mjs`: inline asynchronous answers, explicit selection, freeform answers, retry, exact request identity, and late response isolation pass.
- `node tests/product-ui.mjs` and `node tests/chat-scope-ui.mjs`: affected product and chat scope checks pass.
- `node tests/live-chat-ui.mjs` and `node tests/selection-quote-ui.mjs`: stream behavior and exact text selection pass.
- Desktop and narrow viewport screenshots were inspected.

An initial prompt navigator accumulated DOM elements during scroll and chat changes.
A stable host container now owns that optional component.
The regression exercises 30 scroll changes and four round trips between populated and empty chats.
This evidence confirms the fix in the fixture, not a claim about the underlying React implementation.

The five principal opportunities are prompt navigation, bookmarks, clearer Team groups, direct question controls, and expandable turn recaps.
Worker task/result previews remain a further option.
