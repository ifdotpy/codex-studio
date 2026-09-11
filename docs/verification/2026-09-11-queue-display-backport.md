# Queue display backport, 2026-09-11

The installed renderer shows each pending message in the transcript and a
separate queue panel. Commit `f8ac981` moves edit, reorder, and cancel controls
into the existing transcript message. It preserves the older server API.

`npm --prefix web run build` passes. The isolated
`tests/installed-queue-display-ui.mjs` check passes for one visible message,
edit, reorder, cancel, and the 390px layout. The installed candidate also loads
through a hidden Electron window with a temporary profile and the existing
backend. The large Attar chat loads in a fresh headless browser.

The native observation tool returned blank windows and timeouts after reload.
A renderer process exceeded 2 GB. The candidate was temporarily rolled back.
The owner then confirmed that the chat was visible. These observations do not
establish that the candidate caused a freeze. A browser with copies of the
existing Local Storage and IndexedDB also loaded the application.

The queue fix is installed again. Backend PID 8077 remains active. Only the
Electron window was restarted. No user agents or terminal commands were stopped.
Generated Electron caches were moved to
`~/.local/state/codex-agents/app-backups/electron-cache-20260911`.
Drafts, outbox, local storage, and dictation data were not cleared.

The reviewed source is in `.worktrees/installed-queue-display`. The full
development renderer already has inline queue controls, but requires newer
backend delivery behavior. This backport preserves the older delivery API.
