# Scheduler stopped after disk exhaustion, 2026-09-09

## Confirmed failure

The live server (PID 35212) still answered HTTP requests. Its scheduler thread
was dead. `canvas.log` records SQLite `disk I/O error`, followed by
`OSError: [Errno 28] No space left on device` while writing `runtime-errors.log`.
The scheduler caught the operation error but did not catch the diagnostic write error.
That second exception ended the scheduler thread.

At diagnosis, the volume already had 125 GiB available. A database write transaction
and rollback succeeded. This investigation did not establish what filled the disk
or what freed that space. No user files were deleted for recovery.

## Fix and recovery

The scheduler retains its last error in memory. An `OSError` from its diagnostic
file cannot end the scheduler. A successful cycle clears the transient error.
Tests cover failures when opening and writing the log, followed by recovery.

The dead scheduler was replaced inside the existing process. All four native
account connections stayed in place. The live legacy Runtime has no separate
capacity tick; the installation preserves its original rules and dispatch cycle.

Lumina and spec-runner-results resumed from their pending queues. Attar also had
a stranded pre-submission reservation, c3a0b549-9345-4d5c-ae83-4fec885cc612. Its
native thread was idle, preparation was complete, no start frame was active, and
all seven original inputs remained reserved. The durable submitted flag was false.
The same reservation resumed once through the existing start method. No uncertain
input, tool mutation, or terminal command was replayed.

Readback showed active turns:

- Attar: 01a0866f-674c-71e1-935d-160c6dad0161
- Lumina: 01a0866e-2d02-7433-ab1d-19401273d77c
- spec-runner-results: 01a0866e-2d04-7572-a1d0-b11d04165dd7

Paused and completed agents were left unchanged. The guarded one-time reservation
recovery is operational recovery, not an automatic retry of unknown submissions.

## Checks

`python3 tests/scheduler-disk-full-contract.py`: three tests pass.
Live evidence: `/tmp/studio-scheduler-inspection.json`,
`/tmp/studio-scheduler-activation.json`, `/tmp/studio-scheduler-compat.json`,
and `/tmp/studio-attar-reserved-start-result.json`.
