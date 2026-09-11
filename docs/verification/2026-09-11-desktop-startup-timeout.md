# Desktop startup timeout, 2026-09-11

The installed Studio showed `Cannot verify the local backend: The operation was
aborted due to timeout`. Its identity request had a one-second deadline and no
retry. The existing backend later answered in 0.002 seconds. The cause of the
original response delay is not established.

The launcher now allows five seconds per identity request and fifteen seconds
across retries. The deadline includes the response body. A timeout cannot
authorize a replacement backend, even if a subsequent connection is refused.
Invalid JSON, an incompatible protocol, and a different state directory still
block attachment.

Verification:

- `node --test desktop/backend-test.mjs`: seven tests passed. Cases include a
  response after 1.2 seconds, header and body timeouts, a server with no response,
  a refusal after a timeout, incompatible identity, and attachment without
  desktop assets or a process launch.
- The installed application started with `--hidden` and a temporary desktop
  profile. Its renderer loaded `http://127.0.0.1:4620/` and exposed the native
  desktop bridge. The window remained hidden.
- Backend PID 8077 remained the same before launch, after attachment, and after
  desktop exit. The verification did not stop agents or commands.

Only `backend.cjs` changed inside the installed `app.asar`. Every other archived
file matched the previous archive. The same fix is in the prepared application
under `desktop/dist`. The server workspace was not replaced.

The previous installed archive is saved at
`~/.local/state/codex-agents/app-backups/startup-timeout-20260911/app.asar`.
