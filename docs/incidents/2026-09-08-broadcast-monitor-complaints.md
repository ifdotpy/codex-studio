# Broadcast and monitor complaints, 2026-09-08

## Scope

Fix confirmed Studio defects without restarting native sessions or repeating
project commands. Preserve direct-message and child-result continuation.

## Broadcast resumed obsolete work

Complaint: `acf45148-00ce-577b-a43b-49f87d719a6a`.

The stored broadcast `exec-c851b286-0e8a-4b26-9a04-e18c27ee41b7` queued
messages for 23 recipients at 07:47:26 UTC. Recipient
`9f62e3de-d5ee-5c1c-9d3e-e79a207be582` received it in turn
`01a07ffc-74cd-7910-8d16-d1c3a2583fa6` and started obsolete work.

`chat_message` checked only `autoWake`. An idle or completed worker with that
flag could start a new turn from an information broadcast. Broadcasts now
notify only agents with active work. Other recipients retain access through
chat history. Direct messages keep their existing continuation behavior.
A broadcast received during a turn cannot start another turn by itself after
completion, unless an active monitor or child still needs supervision.

## Cancelled monitor notification

Complaint: `d943aedd-27f1-52bc-bc82-25d55dcd5765`.

Monitor `8448a7f8-3189-53cd-9ed4-54259a237237` has a persisted cancellation
result: exit 137, finished 07:49:29 UTC. There was no `monitor_exit` event.
The `_finish_monitor` condition explicitly excluded cancelled commands.
The missing notification does not mean the command remained active.
Cancelled commands now retain one terminal event. An event from an earlier
owner epoch remains in history without waking a resumed agent.

## Cargo monitor outcome remains unknown

Complaint: `1d2a854d-f839-5f3f-989f-420e56f9ba98`.

Monitor `5a729202-9c7c-543f-b38c-dd84b3a8148f` has no exit code. Its log has
13,307 bytes and ends during compilation. Native termination returned
`no active command/exec for process id`. The live server inspection found
no thread or pending request for this monitor. The account connection is
still `58b1ce1e-4b02-44eb-b6cd-9c196059d0cd`.

The original native result cannot be recovered from these records. Process
absence does not establish success or an exit code. Account logs contain
untimed database I/O errors, but they do not prove this incident's cause.
Keep the complaint open until a trace identifies where the result was lost.
Required evidence: exact request identity, native result, callback outcome,
and persistence error with time and monitor identity.

## Project complaint

Complaint `2b315a95-ba83-52e9-b265-8a2eb6550d48` concerns Attar's optional
`node.fs` capability. Its lead already assigned a fix and acceptance tests.
Studio changes do not establish that the Attar defect is fixed.

## Checks

- Runtime contracts: 49 passed.
- Broadcast completion boundaries: 7 passed.
- Monitor lifecycle: 15 passed.
- Panel feed monitor boundaries: 20 passed.
- Guarded live update: 3 passed.
- Native command fixture: real exit 0 and cancelled exit 137, one receipt each.
  Native commands use an isolated home; model responses come from a fixture.

These checks do not establish the lost Cargo command's outcome.

## Live update

Applied five guarded methods to backend PID 35212 without a restart. All three
native connection identities remained unchanged. The existing HTTP account-cost
handler was preserved.

The missing `monitor:8448a7f8-3189-53cd-9ed4-54259a237237` event was restored
from the recorded exit 137. It retains owner epoch 0 and delivery status
`cancelled`. The worker remains paused at epoch 1, with no new turn. No project
command was replayed. Cargo's unknown record was not changed.
