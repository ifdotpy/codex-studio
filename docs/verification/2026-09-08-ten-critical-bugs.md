# Ten server reliability fixes

Scope: ten confirmed defects in Codex Studio. Tests use temporary databases,
local Git repositories, fake native responses, and a real operating-system pipe.
They do not prove that every earlier live incident has the same cause.

| # | Defect and trigger | Corrected behavior | Regression file |
| --- | --- | --- | --- |
| 1 | A full native input pipe blocks a write without a deadline. Callers can hold the runtime lock. | Bound lock admission and pipe writes. Preserve uncertainty after a partial write. | `tests/critical-runtime-contract.py` |
| 2 | An explicit turn rejection leaves its submitted input uncertain. A local failure after success can instead label it failed. | Record a known rejection as failed. Keep successful native responses with failed local processing uncertain. | `tests/critical-runtime-contract.py` |
| 3 | A failed SQLite preparation commit leaves the agent in the loaded cache. | Commit the native thread identity before updating the cache. | `tests/critical-runtime-contract.py` |
| 4 | A lost approval reply leaves the request pending and permits a duplicate or conflicting answer. | Save the answer before transmission. Preserve unknown delivery. A typed unsent rejection remains answerable. | `tests/critical-runtime-contract.py` |
| 5 | Git creates a worker worktree, but its metadata write fails. Every later preparation then fails on the existing branch. | Adopt only the exact registered worktree path and branch. Preserve its files. | `tests/critical-runtime-contract.py` |
| 6 | A checkpoint reset changes files, then a local failure leaves the old conversation active. | Keep a durable recovery record. Block new work until file and conversation states agree. | `tests/critical-workspace-contract.py` |
| 7 | A native conversation fork succeeds, but local persistence fails. Retrying creates another native fork. | Retain the native result and finish the same local branch on an exact retry. Never repeat an unknown native mutation. | `tests/critical-workspace-contract.py` |
| 8 | A draft document key and its payload can name different chats. | Reject mismatched device, session, and document identities. | `tests/critical-sync-contract.py` |
| 9 | A legacy group post reaches the runtime, but its Canvas delivery update is lost. Exact retries remain pending forever. | Read the authoritative receipt before recovery. Do not resend existing or uncertain events. | `tests/critical-sync-contract.py` |
| 10 | Voice can send realtime or speech requests after the selected account loses project access. | Check project admission before outbound voice requests. Preserve explicit team exceptions. | `tests/critical-voice-contract.py` |

## Checks

All 237 unittest cases passed across the runtime, steering, workspace, account,
monitor, approval, native-error, transport, Canvas, and new regression suites.
The critical workspace suite includes the existing 34 workspace cases.
The separate sync, sync HTTP, voice, and voice-validation contracts also passed.
`npm --prefix web run build` passed. `git diff --check` passed.

Independent review found two unsent-request regressions and a branch admission
race. The final tests cover those corrections. Additional checks cover source
completion during a fork, concurrent restore requests, and restore after restart.
The older in-process updater now refuses a runtime without the required transport
exception type before it replaces any methods.

## Verification limits

The regression tests inject the failures at their actual storage, transport, or
request boundaries. No live model request is part of these tests.

No active agent, monitor, terminal, or backend process was stopped for this work.
Source publication does not activate new Python methods in an existing process.
The separate macOS administrator request for the previous native error update
has not completed. Live incident closure requires deployed evidence.
