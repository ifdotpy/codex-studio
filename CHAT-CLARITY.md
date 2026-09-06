# Chat clarity

The account limit summary must show remaining capacity without opening a menu.
Details group each limit into labelled percentage bars and reset times. Missing
or expired values must not appear as current capacity.

Common workspace sections and agent actions have direct labelled controls. The
monitor control opens the existing monitor form without changing a message draft.
An idle agent must not show a loading symbol. Real active and reconnect states
retain their status indicators.

The user can quote any selected excerpt from a message and repeat that action.
Each excerpt appends to the current draft in order. Existing comments remain.
Selection controls must not quote adjacent tool buttons or stale chat content.

Verification uses browser fixtures for limits, selections, status transitions,
and direct navigation at desktop and narrow widths. Existing message, task,
monitor, and preview paths retain their regression checks.

## Verified changes

The footer shows remaining percentages. The account panel uses grouped cards,
percentage bars, reset times, credits, and an update timestamp. Unknown values
remain unavailable. Expired windows request a refresh. A model-specific pool is
used only when its reported name matches the current model.

Work, Your tasks, Inbox, Changes, Search, Plan, Rules, Background, and Canvas have
visible buttons. Monitor, Compact, Review, and Stop team no longer require the
conversation menu. Labels remain visible at narrow widths.

The Ready row no longer appears below an idle conversation. Active, stopped,
failed, waiting, and reconnect states use their respective status icons.

Quote selection appends an exact excerpt. Alt+Shift+Q performs the same action.
Repeated quotes retain earlier excerpts and the user's comments. Selection is
limited to one message's prose. Embedded preview controls and other messages
cannot enter the quote. Chat changes clear the selection action.

The production build, format check, and all eleven browser suites pass. The
new cases cover account variants, direct navigation, idle status, exact quotes,
paragraph boundaries, keyboard input, touch activation, and 320/390 px layouts.
The parent reviewed source changes and desktop/mobile screenshots. Tests use
isolated runtime state. Native mobile long-press behavior is not measured.
