# Claude worker dialog, 2026-10-06

The bridge can request a user answer during Claude history resume.
Studio permits these questions only for lead agents.
The old bridge rejected the callback from the Claude software development kit (SDK) when Studio refused a worker question.
The callback did not return a dialog result.
This path can leave Claude initialization incomplete.

## Change

The permission callback handles only the exact worker-role refusal from `Runtime.request`.
It returns a native permission denial with the original explanation.
The existing `resume_return` callback then returns `cancelled`.
The bridge does not choose `continue` or `compact` for the user.
Other errors still propagate.
The account check still controls input admission.
Bridge version 18 uses the existing checks for idle process replacement.

## Checks

- The worker resume case fails against `9540e6dc` before this change.
- Five dialog cases pass: worker refusal, lead choice, wrong account, tool denial, and unknown error.
- All 21 controls cases and 9 account admission cases pass.
- Existing question, interrupt, version, and exact-repeat cases pass.
- An independent review finds no defects in this change.
- The real SDK 0.3.285 passes four cases with a private transport fixture.

| SDK transport case              | Dialog response      | Initialization             | Native inputs |
| ------------------------------- | -------------------- | -------------------------- | ------------- |
| Rejected callback               | Error                | Pending                    | 0             |
| Worker refusal, correct account | Success, `cancelled` | Complete                   | 1             |
| Worker refusal, wrong account   | Success, `cancelled` | Complete, account rejected | 0             |
| Unknown request error           | Error                | Pending                    | 0             |

The SDK check executes the maintained bridge functions for permissions, account validation, initialization, and input admission.
Its SDK hash is `437c1e85b41958c04b3ce38096ed5e9e6d1b7ade54eba6df7f7d6362c9c073f8`.
Its bridge hash is `f5b8d1e8b2b3eb823ce40cd3412524bdbadd0916a40ec8a939ac98a4a82975b9`.
The private fixture replaces Claude CLI with a transport in memory.
It does not prove the native default for `resume_return` or the duration of a real startup.

## Live installation

The backend applies `claude-worker-dialog-20261006` without a restart.
Its receipt records PID 6414 and manifest hash `f157c88352e1310a211d43270fd438b97eb10184127981b8db9c04960a743289`.
The three existing Claude bridges receive only the permission callback change.
Their active generators and processes remain in place.
The exact source readbacks have hash `2420fe103daf8a8d5eb8fe779e3e9082c9c98a39a2262ad08a7aa446f5fa3ef5`.
The first response is lost; readback confirms application without a second mutation.

CATIA completes another automatic turn after the earlier recovery.
`CATIA drawing notation` and the peer lead are active with no error.
This check proves their current activity, not execution of a real worker resume dialog.
