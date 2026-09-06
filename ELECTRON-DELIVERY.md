# Desktop delivery, 2026-09-06

The application uses Electron 44.2.0 with the existing React interface and Python
backend. The installed application is `~/Applications/Codex Agents.app`.
The local server remains at `http://127.0.0.1:4620`.

## Changes

- The terminal panel keeps user shells and agent commands accessible across chats.
  Its session list supports search and virtual rows without a fixed count limit.
- File and skill reads have distinct tool cards, paths, output, and exit status.
- Agents create monitors. The manual monitor form, header action, slash command,
  and creation endpoint are removed. Existing monitor controls remain.
- Native dialogs select projects and attachments. Finder, external links, and
  optional notifications use an isolated main-frame bridge.
- [Time awareness](TIME-AWARENESS.md) adds time at user and tool boundaries without
  changing previous request input. Cache hits and billing were not measured.
- Account limits show Codex before Spark, compact quota rows, reset countdowns,
  and exact local dates. Local costs show today and the last 30 days.
  CodexBar supplies estimates, not subscription charges. Unknown coverage remains
  visible. Earned reset credits require explicit confirmation before use.

## Verification

- All 13 web browser suites pass, including 250 terminal sessions and a real PTY
  through the HTTP backend. Tests check terminal input order, uncertain delivery,
  shell output after reload, and no model wake from user shells.
- Nine terminal contracts pass. Real processes test foreground and background
  cleanup, independent sessions, restart, output cursors, and HTTP access checks.
- Eleven reset-credit contracts pass, including HTTP origin and token checks,
  account changes, duplicate requests, timeout, and restart. The limits UI passes
  23 cases. Tests use fake reset RPC results and consume no real credits.
- Eight cost contracts, six time contracts, 42 runtime contracts, 17 canvas
  contracts, 27 workspace contracts, and five workspace race tests pass.
- Six local Responses requests with installed Codex 0.153.4 preserve earlier
  input prefixes, schemas, and instructions across tool failures and restart.
  This test makes no external model requests.
- Twenty hidden Electron cases pass. They include the actual React project
  picker, exact attachment upload bytes, alerts, and external-link handler.
  OS dialogs and external applications are mocked; Electron IPC and SQLite are real.
- The packaged application starts its bundled backend in isolated state. The
  backend remains alive after application exit.
- The installed application attaches to the existing live backend. Both existing
  agent records remain. Verification does not activate or raise a window.

## Limits

Python 3.11 or later and the signed-in Codex CLI remain installed prerequisites.
The arm64 package is unsigned and intended for local use. Chrome and Electron
have separate local storage. Existing chats stay in SQLite, but canvas positions
are not copied between browser profiles. A backend restart ends user shells and
retains their saved output.

See the [desktop instructions](desktop/README.md) for startup, packaging, and
native access boundaries. See [orchestration](ORCHESTRATION.md) for agent tools.
