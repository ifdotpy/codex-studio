# Live launchd policy repair, 2026-10-06

The saved Interactive policy did not change the registered recovery or supervisor jobs.
Both jobs reported `spawn type = daemon (3)`.
This investigation starts from `7b7a8193`.

## Applied operation

Only the recovery job was removed and registered again.
The operation used its existing Interactive plist and state directory.

Before removal, the checks confirmed:

- Recovery PID 13874 owned the registered label.
- The loaded job had `AbandonProcessGroup`.
- Backend PID 6414 had its own process group.
- Supervisor PID 14945 had parent PID 1 and its own process group.
- Supervisor recovery had no blocked, degraded, or fallback state.
- The backend reported supervisor mode without fallback.
- All seven native handles had recorded process IDs, start times, and signatures.
- The plist content and process identities still matched immediately before removal.

The first registration attempt returned error 5.
Readback confirmed that the job was absent.
A second registration attempt succeeded without another removal.

Recovery PID 54317 then reported `spawn type = interactive (4)`.
Backend PID 6414, supervisor PID 14945, and all seven native process identities stayed unchanged.
Recovery accepted the existing backend.

The private operation receipt is `/private/tmp/studio-recovery-interactive-receipt-20261006.json`.
No backend or native process was stopped by this operation.

## Later live readback

The backend and recovery subsequently restarted outside this operation.
The repository also advanced to merge commit `bde97b6d`.

- Recovery PID 75113 reported Interactive and scheduler priority 31.
- Backend PID 75638 had recovery PID 75113 as its parent and scheduler priority 31.
- The backend retained `/Users/igor/.local/state/codex-agents` and supervisor mode without fallback.
- Supervisor PID 14945 remained at scheduler priority 20.
- All seven original native process IDs and exact start times remained unchanged.
- Supervisor recovery still had no blocked, degraded, or fallback state.
- CATIA Review had status `waiting` and no error.
- Its drawing worker had status `completed` and no error.
- The state request took 0.521 seconds and reported 13 agents with status `running`.

This readback proves process preservation and current state.
It does not prove a performance improvement for every API or agent command.

## Remaining native delay

A no-input query in the existing Claude bridge completed account, model, and flag preparation after 13.986 seconds.
It used the CATIA profile, resume history, instructions, and 19 Studio tools.
The account matched, the SDK returned 12 models, and the flag control succeeded.
No Studio input was supplied.
This duration differs from the earlier 33.268-second measurement; it is not a controlled before-and-after comparison.

A new diagnostic child of the existing bridge completed the CPU loop in 0.767221 seconds, with 0.082173 seconds of CPU time.
An Interactive child with an explicit Utility clamp completed it in 0.547515 seconds, with 0.082664 seconds of CPU time.
Neither result establishes a safe removal of the inherited delay.

The diagnostic returned zero values for the readable task suppression fields.
The private policy-state read returned `KERN_PROTECTION_FAILURE`.
Apple's published kernel source restricts that read to privileged tasks.
The exact inherited kernel limit remains unverified.
[Apple task policy source](https://github.com/apple-oss-distributions/xnu/blob/main/osfmk/kern/task_policy.c).

No supported live `launchctl` setter for `ProcessType` was found in the local command help or manual.
The existing supervisor owns the real input and output pipes of its children.
Studio has no transfer mechanism for those pipe descriptors.
A replacement supervisor terminates verified orphaned children during recovery.
Removing that supervisor would therefore interrupt their native calls, with possible unknown outcomes.
The supervisor was preserved.

## Rejected source optimization

An experiment removed the separate metadata query before a cold turn.
It applied thinking flags after native initialization but before the queued Studio input.
The fixture and SDK transport checks passed.
Independent review found a supported boundary that those checks did not cover.

Claude SessionStart hooks can supply `initialUserMessage` before that flag control.
Such an input could use inherited thinking settings.
The requested flags would take effect later.
The installed SDK 0.3.285 declares the difference between constructor settings and the later flag overlay.

The retained-query variant also removed a fresh SDK subscription and provider check.
Studio's separate CLI account check detects login changes, but it does not fully replace that subscription check.

All five experimental source and test files were restored to their existing committed content.
No experimental bridge patch or version 19 was installed.
The installed bridge and controls still match version 18 source.

The applied result is Interactive recovery.
The later backend starts from that recovery job.
The existing supervisor and native processes retain their old policy.
