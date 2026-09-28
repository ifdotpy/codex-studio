# Chat settings redesign measurements

Measured in headless Chromium against the production web build and isolated local API fixtures. The fixture mounted 261 chats and supplied a 301-turn Claude history. It used no live app, database, account, or model request.

| Interaction | Before | After | Target |
| --- | ---: | ---: | ---: |
| Click Chat settings to usable controls | 86.9 ms | 94.5 ms | < 150 ms |
| Change permission mode to saved confirmation | 53.5 ms | 54.6 ms | < 300 ms |
| Full state snapshot reads after a setting change | 1 in the old `run → load → refresh` path | 0 observed | 0 |

Both local timing runs are under the stated budgets. The save timing is similar because the fixture returns its snapshot quickly; it does not model production network latency. Before, each Save click ran `load()` and one full `/api/state` refresh. Now, a setting change sends only the Claude settings mutation, updates the local value, and shows inline confirmation. The regression asserts zero snapshot reads after the change.

The new browser check also makes the next setting request fail, verifies that the control reverts and displays the error, then repeats the same setting and verifies that the original `request_id` is reused. It checks layout at 320 px and saves the 390 px and 1440 px screenshots below.

## Screenshots

Before:

- [390 px](before-390.png)
- [1440 px](before-1440.png)

After:

- [390 px](after-390.png)
- [1440 px](after-1440.png)

Reproduce the measurement with `node tests/chat-settings-autosave-ui.mjs` after `npm --prefix web run build`.
