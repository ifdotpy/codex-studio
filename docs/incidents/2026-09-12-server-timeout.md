# Studio server timeout, 2026-09-12

The owner reports `The server did not respond in time.` during the UI optimization.
The optimized UI was not installed when the outage started.

## Observations

Backend PID 76342 stayed alive. The identity endpoint returned HTTP 200 in
26 milliseconds. The static index returned HTTP 200 in 42 milliseconds.
A full state read timed out after 20 seconds. A chat state read timed out after
eight seconds. The sync identity read took 97 milliseconds.

The callback log recorded one command-output notification with 151,622 milliseconds
of callback time, 788,095 milliseconds of queue delay, and 3,277 queued callbacks.
Callback time includes time that waits for the runtime lock.

The native process sample showed SQLite and JSON work in a dispatcher.
A later privileged Python stack capture occurred after that dispatcher became idle.
It does not identify the earlier lock holder. The logs also contain SQLite
`disk I/O error` failures during severe host load and low free disk space.
The precise cause of those I/O failures remains unproven.

The state directory contains 219,024 task records. Only 24 records had status
`running` at the bounded read-only check. The existing status index counted those
records in four milliseconds. Several guards instead decoded the complete history
under the runtime lock before filtering active tasks.

The application recovered before the task-query patch was applied. A local chat
read took 278 milliseconds. Remote sync took 694 milliseconds. No backend or
account connection was restarted to obtain that recovery.

## Change

The active-task helper applies the existing status index before JSON decoding.
Disconnect, restart, workspace, and permission guards preserve their original
status predicates. Disconnect still restricts changes to the affected account.
Archive checks preserve `running`, `starting`, `pending`, and `unknown` blockers.
The team lookup excludes work history that its response never uses.

The guarded live update verifies known function signatures and module namespaces.
It replaces code on the existing function objects under the runtime lock.
Captured callbacks therefore retain the correction. Unknown code stops the update
before any mutation. A replacement failure restores the prior functions and helper.
The update does not reload modules, replay requests, or replace connections.

These fixes remove confirmed history scans. They do not prove that those scans
caused the observed outage.

## Evidence

Private diagnostic files remain under the Studio state directory:
`evidence/server-timeout-20260912/`. They include the agent identities before the
update, unresolved receipt identities, and the privileged stack capture.

The UI optimization is recorded in
[the responsiveness report](../verification/2026-09-12-ui-responsiveness.md).

## Live application

The guarded update returned `applied` in backend PID 76342. All three native
connection IDs and process IDs stayed unchanged. The installed source also
contains the fix for subsequent starts. The update leaves the startup build
identity unchanged; its separate receipt identifies the applied methods.

After application, chat state returned HTTP 200 in 84 milliseconds. Remote sync
returned HTTP 200 in 128 milliseconds. No request was replayed by this update.
The receipt is `apply-result.json` in the incident evidence directory.
