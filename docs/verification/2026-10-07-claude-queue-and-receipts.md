# Claude queue and receipt recovery, 2026-10-07

The Attar lead repeatedly reported `Claude preparation timed out before input was submitted`.
Its user could not continue the conversation.

## Confirmed defects

- Studio did not save frozen Claude input when a local busy state already existed.
- A positive native rejection could therefore leave the input without a safe retry path.
- The bridge completion event did not identify the original rejected input.
- The installed and loaded bridge sources differed, but both reported version 18.
- The idle update check accepted version 18 and retained the old process.

Commit `c41b9923` saves the exact input and includes its identity in the native completion event.
A retry requires a positive rejection for that same input.
An uncertain response or a later steer does not permit a retry.
Commit `091af57c` changes the bridge version and its idle update check to 19.
The existing checks still protect active turns, commands, queues, monitors, and unresolved requests.

## Live result

- Lead: `7d737898-02ed-4dfc-b7ab-ab3353221ead`.
- Input: `attar-claude-resume-after-receipt-fix-20261007-0728`, sent once.
- Completed turn: `8cd35f1d-35b3-4fc4-aabc-76f8bb67a0f1`.
- Assistant answer: `msg_011CfnUxjhVuxBg1HxEGL1zg`.
- Claude completed a Studio tool call and a Bash call.
- No new input remained pending after the answer.
- The lead had no error or native failure hold.
- The `gpu-linux-2` worker continued its active turn.

The backend applied the Python changes without a restart.
Its process ID remained 3058.
The queue update manifest hash was `0b557af82d7f8e93551842e5331572cba627cdc644011b836e0af6418a6df19b`.
The version update manifest hash was `ea44503a06b927de2d574d14966b96cea5f89af9e9570dd405c1eb90384b3c82`.
Both applied on their first attempt.
The temporary manifests and Python update helpers were removed after receipt checks.

The active bridge received the completion identity change through a guarded update of its loaded function.
Source readback confirmed the change.
Temporary diagnostic code was removed at `2026-10-07T07:40:08.523Z`.
The completion identity change remained present.
The Node Inspector closed after readback.

The normal idle checks later replaced bridge process 92615 with process 76121.
The supervisor generation changed from 7 to 8.
The new process started at `2026-10-07T07:49:07Z`.
Turn `96257b92-078f-42c4-a1cf-86d12962196a` then completed.
Assistant message `msg_011CfnWbGACJF6qCmLW4n2TX` was saved at `2026-10-07T07:50:14Z`.
It reported the next step of the existing Attar work.

## Historical command receipt

Studio retained an active Bash task from 4 October.
The native history contained its exact terminal result at `2026-10-04T09:35:37.074Z`.
The duration was 1,993 milliseconds.

- Task: `7d737898-02ed-4dfc-b7ab-ab3353221ead:toolu_01WErtBfzjxYokMzC56oTXGy`.
- Native turn: `8e269e51-9cf9-4b51-9a4b-2ade6d140622`.
- Native session: `e2187ecb-97f4-4539-933d-5a0609eb23db`.
- The native input, working directory, tool identity, and source assistant matched the saved task.
- The native result reported `is_error=false` and `interrupted=false`.
- The command was a foreground command.
- The bridge retained an `inProgress` tool item in an interrupted turn.

The bridge reads live results only through the current query's tool map.
It does not read a terminal result missed before that query was replaced.
The stale Studio task therefore prevented an idle bridge update.
An exact native receipt permits a historical task update through `Runtime.record_task(stale=True)`.
It does not permit another command execution or another input submission.

The backend applied this exact historical receipt on its first attempt.
The manifest hash was `d6200a81f086254af35252917a43321632735b5bc0da859c01e0a221e1c900bd`.
The saved task now reports `completed`, with its original 1,993-millisecond duration.
The helper verified that the actor record stayed unchanged within the task update transaction.
The four temporary proof and update files were removed after application checks.

Commit `c031de78` adds the maintained receipt reader to `thread/resume`.
The bridge version and idle update check become 20.
The reader accepts only an exact successful foreground Bash result.
It reads a recent history window of at most 8 MiB.
A positive result receives one bounded check of the same bytes before use.
State polls and `thread/read` do not open the native file.
Background, failed, missing, ambiguous, compacted, and older results remain unknown.
The bridge retains the original turn identity and status when it emits the historical receipt.
The existing Runtime method updates the old task without changing the current turn or input.

The backend applied version 20 on its first attempt without a restart.
The manifest hash was `1a36c9f1198d5130757135589139b6ab79a60a2bce18646bd931f5cc6cd7811a`.
The temporary manifest and Python updater were removed after receipt checks.
The idle checks replaced the bridge with process 71979, supervisor generation 9.
The process started at `2026-10-07T08:03:01Z`.
Turn `60bcd1bb-1b96-49f4-99fe-3ae8d48440d7` then completed without an error.
Assistant message `msg_011CfnXjBUW6rvyPcGgHvocu` was saved at `2026-10-07T08:05:10Z`.
This confirms a real model response after the version 20 update.

Two older team events still have uncertain delivery receipts.
One is an agent message from 4 October.
The other is a work review from 6 October.
This repair preserves both receipts and does not replay those events.
They do not prevent the newer Attar turns from completing.

## Checks and limits

- Queue and retry contracts: 47 cases passed.
- Version update contracts: 24 cases passed.
- The original source failed the new queue and version checks before the fixes.
- Guarded Python updater checks: 9 queue cases and 7 version cases passed.
- Exact historical receipt helper: 19 cases passed, including an independent repeat.
- Historical receipt publisher wrapper: 6 cases passed.
- Maintained reader and real bridge resume: 39 cases passed.
- Related bridge, controls, account admission, and authentication wait contracts: 89 cases passed.
- Syntax, staged lint, and format checks passed for the committed source changes.

The original 60-second preparation timeout remains unexplained.
Fresh probes without user input completed in 2.010 and 3.404 seconds.
A probe under the existing bridge process completed in 5.217 seconds.
Those results do not identify the cause of the earlier timeout.
They do not test a live model request.
The completed Attar turn supplies separate evidence of actual recovery.

The supervisor still has its old daemon resource policy.
Its saved Interactive setting does not change the current process.
The current probes do not prove that this policy caused the latest timeout.
