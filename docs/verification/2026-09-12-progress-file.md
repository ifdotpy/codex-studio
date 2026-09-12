# PROGRESS.md display, 2026-09-12

## Behavior

Each managed agent uses `<stateDir>/progress/<agentId>/PROGRESS.md`.
The file remains outside the project repository. Agents use ordinary file tools.
Agents that share a working directory have separate files.
The guide and normal turn context supply the exact path.
No extra model turn delivers these instructions.

The selected chat reads the file while visible. A successful read schedules the
next read after one second. Hidden views pause reads and resume immediately.
Reads have an eight-second deadline and cannot overlap within one display scope.
An old chat or workspace response cannot replace the current display.
The display has a maximum height of 150px and scrolls internally.
Empty or deleted files hide it. Read errors preserve the last valid text.

The server reads at most 128 KiB outside the runtime lock.
It rejects invalid UTF-8, symlinks, special files, and unstable reads.
An atomic replacement becomes visible on the next read.
Provisioning never overwrites an existing file. A provisioning error does not
prevent thread setup, turn context, or a command permission check.

Workspace-write agents receive only their own progress directory as an extra
writable root. Read-only agents retain their policy. Full-access agents retain
full access. Native approvals and network policy remain unchanged.
The thread configuration uses the documented
[`sandbox_workspace_write.writable_roots`](https://learn.chatgpt.com/docs/config-file/config-reference) setting.

New tool catalogs omit `orchestration_panel` and `orchestration_panel_feed`.
New calls through an older catalog return the exact file instructions.
Known rejected calls receive `not_applied`; an existing committed operation keeps
its evidence and uncertainty. Cached tool responses retain their original result.
Legacy panels, callback receipts, and active feed records remain intact.
The new display has no agent callback or feed tool.

## Checks

- `progress-file-contract.py`: file lifecycle, limits, identity, unsafe paths, and retained legacy state.
- `progress-runtime-contract.py`: per-agent roots, read-only policy, versioned context, and optional-file failures.
- `panel-contract.py`: real HTTP route, tool retirement, old receipts, and legacy internal validation.
- `progress-markdown-panel-browser.mjs`: Chromium and WebKit, read failures, deadlines, scope changes, resume, and composer preservation.
- `yolo-contract.py`, `native-safety-contract.py`, and `token-efficiency-contract.py`: existing permission, retry, and context behavior.
- Production TypeScript check and web build.

The browser checks use fixtures. They do not prove physical iPhone input latency
or operation through every mobile network.

## Live application

The packaged application is installed at `/Users/igor/Applications/Codex Studio.app`.
The reviewed function update applied to backend PID 76342 without a restart.
All three native connection IDs and process IDs stayed unchanged.
The desktop reopened as PID 78504. Its startup log confirms the same backend.
The startup backend build identity remains unchanged; the update receipt records
which implementations changed inside that process.

The installed server and application assets passed a file lifecycle check on an
idle test chat. Ordinary writes, atomic replacement, automatic display refresh,
and deletion all passed. The test removed only its own temporary file content.
No chat message or command was submitted. The file read took 40 ms; the open
chat displayed the later file update in 1288 ms. This includes its polling interval.

Local and remote indexes have the same SHA-256:
`7c9058f206641e4f562598fc3292b4429c7dd68de83a4f9f1cf1874519456699`.
The remote URL is `https://macbook-pro-lumina.tailf00fa0.ts.net`.

Evidence, including source hashes, update receipt, tests, and the live display:
`~/.local/state/codex-agents/evidence/progress-file-20260912T145550/`.
The previous application is retained in
`~/.local/state/codex-agents/evidence/progress-ui-20260912T145931/`.
