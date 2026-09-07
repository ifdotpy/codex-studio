# Stale turn status after storage errors

## Confirmed state

The live runtime, PID 35212, continued to answer HTTP requests in about 0.09 seconds.
Its event callbacks recorded `disk I/O error` between 22:02 and 22:13 UTC on September 7.
The callback dispatcher logs an exception and discards that callback.
There was no later native status check to repair a missed terminal notification.

Native inspection confirmed two mismatches:

- Attar: turn `01a07de3-b300-79c3-aa2a-b99054d1a9cb` was interrupted.
  Its thread was not loaded in the replacement account process.
  Studio still recorded `running` and `inFlight=true`.
  A steer request received `thread not found`.
- FieldView: turn `01a07ded-94a9-7372-9dd9-72b108bd7d61` completed.
  The native thread was idle. Studio retained a partial message and `Writing`.
  Steer requests received `no active turn to steer`.

A failed disconnect transaction can also leave the old process's preparation cache intact.
The replacement connection must not reuse that cache or its unresolved preparation future.

## Source correction

`codex_turn_recovery.py` checks turns after 120 seconds without recorded activity.
It checks each eligible agent at most once per minute and submits one probe at a time.
Native reads use the existing recovery executor, outside the runtime lock.
They do not call a model.

An active native thread needs only a small status read.
For an inactive thread, the check reads the latest ten turns and verifies native status again.
Only an exact terminal turn can repair the local state.
An unavailable read, an unknown status, or a missing turn leaves the outcome unconfirmed.

The application applies earlier native callbacks before it checks account, connection,
thread, turn, epoch, and start-attempt identities again.
It restores missing final text and uses the existing terminal-notification handler.
Existing completion IDs prevent duplicate completion events.
An unloaded thread loses its cached preparation before the next turn.
Preparation also checks connection identity independently.

The correction does not replay tool calls or rejected messages.
It does not stop monitors, command processes, or account servers.
It does not establish recovery of all other records lost during the storage errors.

## Live verification

The two lead states were reconciled at 22:38:16 UTC.
Their existing pending user messages became delivered in new turns:

- Attar: message `4e705c93-2b4c-435c-92b0-10e6154d48c7`,
  turn `01a07e05-dd1f-7eb0-a952-4d2947707c9e`.
- FieldView: message `e12df8ba-7d2d-4676-af75-ba23fb0439a1`,
  turn `01a07e05-c30a-7e13-8a84-383c1ef3954f`.

Both produced new responses. The three earlier rejected steer receipts remain failed.
No new identity was used to repeat those rejected attempts.

The guarded installer activated the permanent correction at 22:49:44 UTC.
The backend PID, account connections, and executor objects remained unchanged.
The watchdog then reconciled ten stale worker turns from native evidence:
nine interrupted turns and one completed turn.
These labels describe existing native outcomes, not interruptions by the recovery code.

Private evidence is under
`~/.local/state/codex-agents/diagnostics/turn-recovery-20260907T224944Z/`.
The installed application's source matches the reviewed correction.

## Checks

All 81 targeted tests pass:

- `python3.14 -B tests/turn-recovery-contract.py`: 17.
- `python3.14 -B tests/turn-start-contract.py`: 16.
- `python3.14 -B tests/runtime-contract.py`: 48.

Coverage includes lost completion, partial final text, failed and interrupted outcomes,
active turns, read timeouts, identity races, storage failure, pending-message delivery,
monitor preservation, bounded scheduling, and the guarded live installer.
