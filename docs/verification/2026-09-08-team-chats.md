# Team conversations in the chat toolbar

## Change

Agent chats moves from the left sidebar to a button beside Background.
The reader retains the selected lead conversation and its draft.
It shows the selected team's broadcast and private rooms whose participants belong to that team.
Global broadcasts and conversations with other teams stay outside this view.
Existing room records and agent communication tools remain unchanged.

## Verified

- Production TypeScript and Vite builds pass.
- `node tests/agent-chat-scope-ui.mjs` passes against an isolated runtime.
- Scope coverage: two leads in one folder, private rooms without root metadata,
  excluded cross-team/global rooms, bounded room list, older messages,
  retained main conversation, desktop/mobile navigation, and Background access.
- `node tests/mobile-client-ui.mjs` passes at 320, 390, and 760 pixels.
- Desktop and narrow room screenshots were inspected.
- No live model calls or user messages were made by these tests.

## Remaining test failures

The full UI suite is not certified by this change.
`product-ui.mjs` stops at its existing tool-output assertion before the room checks.
`background-ui.mjs` times out when the task detail becomes hidden after a mobile viewport change.
`project-tree-ui.mjs` expects five rows; observed counts vary after reload, including six.
These failures remain open. Their root causes have not been established in this task.
The focused room checks do not establish that these other behaviors are correct.

## Delivery

Only renderer assets need an update. The live backend and agent processes need no restart.
Existing windows load the new renderer after a page reload.
