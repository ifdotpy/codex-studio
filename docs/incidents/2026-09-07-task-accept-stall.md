# Task accept stall, cell855

Existing complaint: `0d4fe375-a338-530f-ad0a-ba51288acfca`.

Read-only inspection at 15:01:44 UTC found outer native call
`call_decq8iXDgG92HJT5Pm9pNaYU`, started at 14:19:46.293 UTC. The last observed
wait at 14:46:06.956 UTC still returned `Script running with cell ID 855`.

The requested accept targets `40ef931a-bd4a-4330-bab5-f23d2a5cabf0`. Its work
record remains `review`, version 6, last updated at 14:17:32.764 UTC. Its only
decision is the earlier rejection. No matching inner tool ID, task, tool result,
operation receipt, or accept decision was found. The subsequent message has no
execution record. The original cell may still execute; these absences do not
prove `not_applied` and do not authorize a blind retry.

Backend PID 78700 still predates the source fixes. Available evidence cannot
distinguish a code-cell wait before dispatch from an unrecorded native request
or another in-memory wait. This incident does not establish a missing completion
notification or a defect in the AWS command.

The source now routes work-board actions through the coordination pool. A
regression blocks every slow-tool worker and verifies direct and older-thread
accept calls still complete. Another holds the final tool return after an actual
accept commits. Recovery exposes that exact operation receipt without changing
the pending tool state or repeating its decision. The read-only CLI also supports
operation-only receipts on older servers.

These checks address confirmed source gaps. They do not explain or settle the
unobserved inner request from cell855. Production activation must preserve current
threads and wait for active commands before the server restart.
