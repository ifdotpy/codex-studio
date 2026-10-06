# Supervised native executable recovery

## Confirmed cause

The backend selected a newer executable while the supervisor still owned the older live executable.
The supervisor correctly rejected the different launch signature.

Read-only checks identified the exact live child:

- Backend PID: 75638.
- Supervisor PID: 14945.
- Account handle: `account:profile-1a70871ee4be5b7a22dc`.
- Native PID: 15150.
- Native generation: 4.
- Saved PID and process start identity matched before and after the checks.
- All six command arguments matched except the executable path.
- The actual command and environment produced the saved launch signature.
- The selected executable and existing environment produced a different signature.

Strict environment fields matched. Only these ignored ambient fields differed:

- `LANG`: removed.
- `PATH`: changed.
- `__PYVENV_LAUNCHER__`: added.

The checks did not print environment values or credentials.

## Recovery contract

`Runtime.connect` supplies the exact account and its resolved home to `executable_for`.
An unchanged launch keeps its existing connection path.
A different executable requires the exact saved child identity, command arguments, environment, and launch signature.

The existing executable must remain inside the content-addressed native bundle store.
The executable and all required companions must match the bundle digest.
Verification does not run an approval probe or change the globally selected update.

The retained proxy uses the existing supervisor `status` action.
It never sends `open`.
It checks the saved generation, initialization result, and acknowledgement cursor before reattachment.
A changed or exited child causes a visible failure.
This path cannot start a replacement process.
A child that already exited before selection uses the newest selected executable for a normal new generation.

## Verification

The private supervisor fixture reproduces the reported error on the previous implementation.
The failing case completes in 0.766 seconds.

After the fix:

- `native-retained-executable-contract.py`: 20 tests pass in 14.579 seconds.
- `native-runtime-updates-contract.py`: 35 tests pass in 0.914 seconds.
- Existing environment, credentials, account, and PID reattachment checks: 3 tests pass.
- Python syntax and `git diff --check`: pass.

The new tests check retained receipts, cursor identity, missing initialization, changed generation, changed account, changed arguments, and modified companions.
They also check exit races, an absent supervisor owner, unchanged launches, and fresh launches.

Two broader restart fixtures observe completed work when they expect partial or running work.
Those fixtures replace executable selection with a mock and do not execute the new retained selection path.
Their failure does not establish a regression in this change.

No installed source, live database, agent, monitor, or native process changed during the fixture checks.

The final test also reproduces an open log after a rejected attach.
`AppServer.__init__` now closes that log before it raises the error.
The test fails before the change and passes after it.

## Live application

The reviewed functions were applied to the existing backend through the supported live update mechanism.
The installed methods retain their other existing behavior.

- Backend process: 75638, unchanged.
- Supervisor process: 14945, unchanged.
- Retained native process: 15150, unchanged.
- Retained generation: 4, unchanged.
- Live update: `native-retained-recovery-20261006`.
- Receipt: `applied`.
- Four saved inputs were restored through `/api/connection-recovery`.
- Their recovery calls returned in 0.14 to 0.80 seconds.

The applied manifest and temporary patch were moved into the state directory backup.
The installed application was signed again.

## Verified close and restart

Successful attachment exposed a separate unhealthy native process.
PID 15150 did not answer `thread/read`.
The user authorized interruptions and required restoration afterward.

The operator close caller had a two-second socket deadline.
The owner can wait three seconds for TERM and two seconds for KILL.
The real close timed out, but its durable record proved the process stopped.
The recovery read that record before any further action.
The caller now waits ten seconds.

An exact operator close also removed the in-memory owner without a degraded recovery record.
A subsequent launch with a changed executable then failed as an orphan.
The supervisor now accepts the exact durable operator-close reason and saved child PID and start identity.
The start identity must have a valid numeric or legacy process start format.
The previous identity must no longer match a live process.
Missing, malformed, unknown, or still-live proof remains blocked.

Both failures reproduce on the previous implementation.
Four operator close cases pass after the change.
The independent review also checks the malformed identity rejection before `Popen`.

The installed helper initially followed the daemon entry point and did not load in the supervisor process.
The installation now defines the helper before that entry point.
Static function identity and entry point order checks confirm the installed artifact.

The final supervisor PID is 92109.
The final backend PID is 92215.
The backend uses the original state directory and supervisor mode.
It does not use fallback mode.
The existing recovery configuration was restored exactly.
No second backend was started against the state directory.

The cutover saved pending inputs, active agent scopes, 23 operations, and 12 monitors.
The existing backend stopped gracefully before exact native generation closes.
All native handles were closed and read back before the supervisor restarted.
The final supervisor has no degraded or unknown ownership record.
Recovery instructions require agents to check results before they repeat any operation.
