# Live handover

A new server build takes the listening port from the running one. The port never
closes, so no connection is refused. Owner: `workspaces/runtime/apps/runtime-server/src/handover.rs`.

## Sequence

1. The running server offers `handover.sock` in its state directory.
2. The new build starts with `--takeover`. It sends `GO` and receives the listening
   socket (SCM_RIGHTS). The old server stops accepting, drains HTTP, ends SSE streams
   (clients reconnect with their cursor), flushes the SQLite WAL and closes the database.
3. The old server starts a standby copy of its own binary (`handover-standby` in the
   state directory), gives it the listening socket, and exits. The standby never opens
   the database.
4. The new build connects to the standby. After the old process ended, the standby sends
   `DRAINED`. The new build opens SQLite, serves, and sends `READY`. The standby exits.
5. If the new build dies before `READY`, the standby starts the old binary again with
   `--listener-from-stdin`. The old build serves again on the same port.

Only one process has the database open at any time. Agents live in
`codex-process-supervisor` and reattach as for any restart.

## Delivery

`install_update.py` (`stop_backend`) uses a handover when `handover.sock` exists, and
SIGTERM as before when it does not (an old build without this feature). The recovery
loop then sees a healthy backend and attaches.

## Limits

- A long migration in the new build delays its first answer. Requests wait in the accept queue.
- A lost answer during the switch stays uncertain, as for any restart.
- The old binary is copied to the state directory for the standby (one copy per handover).
