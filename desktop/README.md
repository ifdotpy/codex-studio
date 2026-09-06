# Codex Agents desktop

The Electron app opens the existing React workspace. The Python backend owns agents, terminals, monitors, and the SQLite database. Closing the app leaves that backend active.

## Start

Requirements: macOS on Apple silicon, Node.js 22 or later, Python 3.11 or later, and the installed Codex CLI with its existing sign-in.

From `codex-agents/desktop`:

```sh
npm ci
npm run dev
```

`dev` builds the web app and starts Electron. Use `npm start` after a web build. Use `npm run start:hidden` for a check without a visible window.

The app attaches to a compatible backend at `http://127.0.0.1:4620`. Otherwise, it starts the bundled Python backend as a detached process. A backend identity endpoint checks the protocol and canonical state directory before the app loads the page. The runtime file lock prevents two servers from owning the same database. An incompatible service causes a visible error. The app does not stop that service or choose another database.

The default state remains `~/.local/state/codex-agents`. Existing chats remain there. Browser local storage, including canvas positions, belongs to each browser profile. Electron uses its own profile; it does not copy Chrome local storage.

The launcher checks `PATH`, `/opt/homebrew/bin`, `/usr/local/bin`, and `/usr/bin`. It skips Python versions below 3.11. Set `CODEX_AGENTS_PYTHON` or `CODEX_BIN` to an absolute executable path when needed. It does not run a login shell. `CODEX_AGENTS_STATE_DIR`, `XDG_STATE_HOME`, and `CODEX_HOME` retain the backend meanings. `CODEX_DESKTOP_PORT` and `CODEX_DESKTOP_PROFILE` support isolated tests.

Backend logs and its PID remain in `canvas.log` and `canvas.pid` under the state directory. Quitting Electron does not stop agents. Use the workspace controls to stop a specific agent or terminal.

## Package

```sh
npm run package
```

The command creates `dist/Codex Agents-darwin-arm64/Codex Agents.app`. The package includes Electron, Python source files, and the compiled web assets. Python and Codex remain installed prerequisites. The package uses no checkout paths at runtime. It is unsigned and not notarized; this build is for local use.

## Native bridge

`bridge.d.ts` defines `window.codexDesktop`. The bridge is available only in the workspace main frame.

| Method                      | Result                                                                         |
| --------------------------- | ------------------------------------------------------------------------------ |
| `pickDirectory()`           | A folder path, or `null`                                                       |
| `pickFiles()`               | Up to 20 files, with name, path, MIME type, and base64 data; 20 MB total limit |
| `revealPath(path)`          | Reveal an existing absolute path in Finder                                     |
| `openExternal(url)`         | Open an HTTP or HTTPS URL in the default browser                               |
| `setNotifications(enabled)` | Set notification permission for this app session                               |
| `notify({title, body})`     | Send a silent notification if permission is enabled                            |

Call the first five methods directly from a user click or keyboard handler, before an `await`. The isolated preload accepts a trusted input event for 1.2 seconds and consumes it once. Native notifications require explicit permission first. Other Chromium permissions are denied.

The main process checks the sender, frame, page URL, method, and arguments. The renderer has no Node.js access. Context isolation and the Chromium sandbox stay enabled. Embedded previews receive no bridge. Navigation, redirects, new windows, and webviews are blocked. The UI opens external links through the validated bridge.

The [Electron security guide](https://www.electronjs.org/docs/latest/tutorial/security) owns the upstream security guidance.

## Verify

```sh
npm test
npm run test:package
```

`test:package` requires a current package. It starts that exact `.app`, verifies its bundled backend path, and checks backend survival after app exit.

The source suite starts the real Python backend with an isolated database. It races two launchers, checks attachment and state ownership, runs hidden Electron windows, checks native access and navigation boundaries, and confirms that the backend survives app exit. It does not start a model request. Test windows remain hidden; input uses the renderer protocol, not operating-system input.
