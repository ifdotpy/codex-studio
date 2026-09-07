# Work and Codex conversation UX

## Evidence boundary

Inspected the installed `openai-codex-electron` package, version `26.818.41509`,
in `/Applications/ChatGPT.app/Contents/Resources/app.asar`.
This is a deeper source review after the [earlier audit](2026-09-07-desktop-ux.md).
Temporary extracts and formatted inspection copies are in
`/tmp/studio-work-ux-audit`. No product source or assets were copied into Studio.

Computer Use denied access to the ChatGPT window. No live interaction with the
reference application was verified. The supplied 11:15 screenshot shows a social
post, not the application's interface. It cannot verify UI behavior.

Official documentation distinguishes ChatGPT Work from Codex in the desktop
product selector. Shared bundled code does not prove that every branch is active
in both modes or available to this account.
Sources: [Use ChatGPT](https://learn.chatgpt.com/docs/use-chatgpt),
[Subagents](https://learn.chatgpt.com/docs/agent-configuration/subagents).

Archive paths below are source evidence. Recommendations are Studio design
decisions, not claims that the reference has the same controls.

## Findings

### 1. Separate the answer from the work log

`webview/assets/local-conversation-turn-Bhd6WQLo.js` assembles separate units for
agent activity, the assistant answer, tool outputs, and artifacts. Its activity
component receives `hasFinalAssistantStarted` independently of the final answer.

Studio puts some historical answers inside a turn disclosure. The reader must
sometimes expand the turn and then expand its work log. Prefer a readable answer
with one disclosure for the preceding work. Do not put routine success labels
above every answer.

### 2. Collapse with explicit rules

`webview/assets/subagent-activity-chip-group-fTxFK4Q1.js` contains `uj`.
It allows activity collapse when the final answer starts, the turn is not
cancelled, and renderable activity exists. It respects persisted collapse choice,
forced expansion, and `preventAutoCollapse`. Some embedded app results can remain
visible as persistent units while surrounding activity collapses.

Studio should preserve the reader's manual expansion and position. Automatic
collapse must never hide an unresolved question, failed command, or useful
interactive result. This requires a behavior change and scroll regressions,
not only CSS.

### 3. Use one activity summary with useful information

The same archive entry defines summaries for file reads, file edits, commands,
web searches, and connected integrations. Its completed activity divider can
show elapsed work time, with a previous-message count as a fallback.

Studio often repeats a generic tool count across multiple small groups. A useful
summary would describe the work, for example, `Read 4 files · Ran 2 checks`.
Derive that from structured tool records. Do not spend model tokens to write it.
Keep running and failed operations individually accessible.

### 4. Make status answer a current question

`local-conversation-turn-Bhd6WQLo.js` selects a waiting-for-answer state when an
elicitation is pending. It can use a reasoning heading for current activity and
has a Thinking fallback. The reference also contains completed activity labels;
it does not eliminate all completion information.

Studio should answer: Is the agent working? Does it need me? Did something fail?
Routine completion is already apparent from the answer and enabled composer.
Repeated `Complete`, `Turn complete`, and check icons add little information.

### 5. Distinguish workers from their conversations

`subagent-activity-chip-group-fTxFK4Q1.js` derives group summaries from member
states. It distinguishes active, updated, interrupted, and finished members.
`local-conversation-turn-Bhd6WQLo.js` supports compact inline groups and an
overflow count for additional subagents. The separate subagent panel provides
another level of detail, as recorded in the earlier audit.

Before this change, Studio's left `Agents` tab reads `runtime.rooms`, not the worker registry.
Its count is a room count. The right Team panel contains actual workers.
The current label creates two different meanings for Agents. Name the room
surface `Agent chats`, and connect team-specific conversations to their Team.
Private cross-team conversations belong to each participant's lead chat.
Global broadcasts do not belong in a selected chat's room list.

### 6. Treat scroll behavior as interaction state

`webview/assets/thread-scroll-layout-Bu5PFUH3.js` tracks distance from the bottom,
wheel direction, keyboard input, pointer dragging, content height, and recent
user intent. It separately reserves space for the composer footer. It disables
native overflow anchoring while its controller owns the behavior.

Studio already has a scroll controller. Extend its checks around turn collapse,
file previews, inline questions, panel updates, and composer resizing. Preserve
the visible paragraph while the user reads earlier content. Content changes
alone must not re-enable automatic following.

### 7. Keep secondary actions conditional

`local-conversation-turn-Bhd6WQLo.js` conditionally enables the assistant action
row and selected-text actions. `composer-utility-bar-Ddref8hX.js` branches on
execution context and available controls. Source branches are not evidence of
the complete toolbar visible in any particular Work session.

Studio currently shows many parallel navigation choices above the conversation.
A proposed Studio layout has the chat and Team as primary surfaces. Show files,
changes, pending questions, and background operations when their state warrants
attention. Put configuration and infrequent commands in the existing menu.
This is a proposal, not an implemented redesign or an exact copy of Work.

## This change

- Rename Agents to Agent chats. Scope rooms and their count to the selected lead
  conversation, including private rooms without root metadata.
- Keep the selected lead context when opening a private room. Show a return link.

- Remove the Question history component and its frontend fetch path.
- Preserve unanswered and deferred questions, answer receipts, and backend history.
- Hide the routine completed phase below the conversation. Reconnection and failure
  states remain visible.
- Remove the visible Turn complete label and success icon from completed turn
  disclosures. Preserve disclosure behavior, result links, and failure labels.

Browser checks cover answering, defer and restore, retained receipts, reload,
chat isolation, turn disclosure, result previews, and narrow layout.
These are Studio fixture checks, not live checks of the reference application.

## Recommended order after this change

1. Keep answers readable and consolidate preceding activity into one disclosure.
2. Simplify navigation around Chat and Team.
3. Show structured work summaries instead of repeated generic tool counters.
4. Verify scroll and composer behavior across every expanding surface.
5. Consolidate file and preview entry points around the resulting artifact.

The desired outcome is fewer competing controls, not fewer agent capabilities.
