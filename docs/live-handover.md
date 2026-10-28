# Live handover

A new server build takes the listening port from the running one. The port never
closes, so no connection is refused. Owner: `workspaces/runtime/apps/runtime-server/src/handover.rs`
(the module comment there is the protocol).

## Sequence

1. The running server offers `handover.sock` in its state directory.
2. The new build starts with `--takeover`. It sends `GO` and receives the listening
   socket (SCM_RIGHTS). The old server stops accepting, drains HTTP, ends SSE streams
   (clients reconnect with their cursor), flushes the SQLite WAL and closes the database.
3. The old server starts a standby: its own executable file (found by its open file, so
   it is the old build even after the install swapped the app) with `--handover-standby`.
   It gives the standby the listening socket and exits.
4. When the old process ends, the new server connects to the standby and reads `DRAINED`.
   It sends `OPEN` just before it opens the state, and `READY` when it serves.
5. Rollback: if the new server never connects, or its line ends or fails before `OPEN`
   (5 s wait), the standby starts the old executable again with the socket as standard
   input. The old build serves again on the same port.
6. Fail forward: if the line ends after `OPEN`, the state may have migrated. The standby
   exits and the recovery loop starts a backend. The old build does not run on it.
7. Old server without standby (drain past its grace, error, kill): the new server finds no
   standby, waits for `runtime.lock` and serves from the socket it holds.

Only one process has the database open at any time. Agents live in
`codex-process-supervisor` and reattach as for any restart.

## Delivery

`install_update.py` (`stop_backend`) uses a handover when `handover.sock` exists, and
SIGTERM as before when it does not (an old build without this feature). The recovery loop
(`recover_backend.py tick`) waits while any process listens on the port, so it never
starts a third backend between two owners.

## Limits

- A long migration in the new build delays its first answer. Requests wait in the accept queue.
- A lost answer during the switch stays uncertain, as for any restart.
- A resumed old build runs from the old executable path (`.previous.app` after an install).
  The next install replaces that folder.
- On macOS, when a new server dies, closing its copy of the socket can reset connections
  that wait in the queue at that moment. Clients retry.
- A stop without a handover (SIGTERM) drops the offer copy of the socket at once, so the
  port closes with the server as before.
