# Owner message triage, 2026-09-09

Scope: the owner asked Studio maintenance to handle orchestrator messages.
Nine owner-facing complaints had no response. Earlier investigations remain open
where the cause or recovery cannot be proved.

## Confirmed Studio defects

- `b9578f7b`: a local workspace reservation rejected `orchestration_send` before
  it saved the message. The recorded request failed at 10:28:10 UTC.
- `723f99a2`: the same guard could fail a reserved start after preparation.
- `3a7894d7`: accepted-task errors instructed the lead to reject a result,
  although rejection requires work in review.

Messages now queue during checkpoint capture. The receipt identifies the agent
and operation that hold the directory. Dispatch waits for the reservation.
A reservation failure before native turn submission returns the exact event
batch to pending. Unknown or submitted attempts are never replayed.
Restore and branch reservations still reject new input because they can change
thread identity. This update does not settle historical unknown delivery receipts.
Stable caller-supplied IDs for native send remain a separate limitation;
existing call IDs can identify saved tool requests.

Accepted decisions remain immutable. Both edit and review errors now direct the
lead to create a follow-up task with the accepted task ID and new evidence.
The existing review rejection path remains available.

## Other messages

| Complaint | Evidence and disposition |
|---|---|
| `bad787bf` | Unauthorized nested reviewer `475f96c5` is paused, autoWake false. The lead excluded its evidence. Close the contained incident; this does not prove technical enforcement of lead-only delegation. |
| `e2df4e42` | Inspected Git objects in the reported worktree: `08aaccb7b` contains four files; `f467817b9` contains the missing eight. Task `95fd6435` records both and lead acceptance. Close the incomplete handoff incident, without a new product acceptance claim. |
| `c62c1ee1` | Disk pressure is recorded. Task `fec76941` now has an accepted decision. Scheduler failure and recovery are documented separately. Build backpressure and all uncertain sends are not verified; keep open. |
| `dfc78c15` | Studio has no implementation of `notes.append_to_file`. The reported new-note workaround succeeded. The underlying error needs provider request evidence; keep open. |
| `81f38e63` | The worker reported closing an unrelated browser. No evidence establishes recovery of unsaved UI state. Preserve the incident and require an owned browser endpoint for later checks. |
| `039bd62c` | The lead reported a port change that invalidated a consumer. Preserve endpoint ownership and coordinate consumers before a change. Current end-to-end recovery is not independently verified. |

Earlier task `40ef931a` now has an accepted decision, version 7. This later state
does not prove which inner operation stalled in cell 855. Keep the original
latency, cancellation, missing-thread, and AX launch investigations open.

## Verification and activation

- Five focused regressions pass in the delivery worktree and current source.
- Thirty-four workspace regressions pass.
- The isolated fixture verifies queued delivery, replay identity, preparation
  races, uncertainty guards, restore refusal, and accepted decision history.
- The legacy send method also passed the focused fixture before activation.
- Installed scoped methods into the existing server, PID 35212.
- Preserved all four native account connections and active work.
- Scheduler remained alive. No live user message or project task was replayed.
- Active historical turns keep their existing Python frames. Future calls use
  the updated methods. Native send tool schemas were not replaced.
