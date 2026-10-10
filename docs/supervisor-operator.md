# Process supervisor operator notes

## Closing a test or unknown handle

The operator CLI can close one in-memory child on a supervisor generation that
contains the `--admin-close-handle` option. Use this only after confirming from
the incident or test log that the handle belongs to a test or unknown client.
Read the current supervisor identity first:

```sh
python3 workspaces/runtime/apps/server/src/codex_process_supervisor.py --state "$STATE_DIR" --status-json
```

From the matching `handles` entry, copy `id`, `pid`, `startTime`, and
`signature`. Check that the handle is the one to remove and that its PID belongs
to its recorded start time. Then close that exact identity:

```sh
python3 workspaces/runtime/apps/server/src/codex_process_supervisor.py --state "$STATE_DIR" \
  --admin-close-handle terminals \
  --expected-pid "$PID" \
  --expected-start-time "$START_TIME" \
  --expected-signature "$SIGNATURE"
```

The supervisor refuses the operation if any recorded identity changed. The CLI
sends the operator-only request over the state directory's protected Unix
socket. Do not use this path for an account handle with active work.

## Incident on an older supervisor

The older supervisor generation has no per-handle close operation and retains
exited children in its in-memory handle map. Its `terminals` handle cannot be
removed while preserving the account children. Replacing that supervisor makes
the old generation's recovery path stop all of its verified child processes;
there is no safe old-generation command that targets only the stray terminal.

For the reported `terminals` incident, leave the live supervisor and its
children alone until an announced maintenance window. Pick a moment when all
account model work, native commands, monitors, and user terminal sessions are
idle and users can tolerate reconnecting. Stop/restart the backend and
supervisor as one planned recovery, then start the updated generation and
confirm its health and handle list. The next generation can close a verified
test/unknown handle individually if such a handle is created again.
