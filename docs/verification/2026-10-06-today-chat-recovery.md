# Today chat recovery

## Scope

Restore the chats that worked on October 6 in Europe/Warsaw.
Keep their history, pending inputs, exact operation identities, and existing work.
Never repeat an operation with an unknown result.

The initial inventory found 12 lead chats and 50 agents with work today.
The initial inventory found four lead chats and one worker with a Claude session limit until 18:50.
Their durable automatic resume records remain scheduled for 18:50:05.
The recovery did not change their account or model.

## Confirmed Claude defects

An exact rejected input stayed attached to its former backend connection.
A new user instruction could not resume that input after a reconnect.

A separate background turn could compact the same native history.
The rejected input then failed the source check because its compaction count changed.

A valid retry could deliver that input through a separate active background turn.
The dispatcher kept its old retry marker and blocked every later input.

The fix preserves the original input, message identity, transcript hash, and event hashes.
A newer pending user instruction permits a new connection and a strictly increased integer compaction count.
The account, thread, epoch, workspace, model, and permission fields must still match.
A Stop, changed input, or unknown native result still blocks the retry.
The submission transaction now retires the marker for a valid retry into a background turn.

## Checks

- Previous code fails the reconnect regression.
- Previous code fails the background retry regression.
- Claude input recovery: 33 tests pass in 18.194 seconds.
- Claude pre-admission receipts: 7 tests pass in 8.869 seconds.
- Independent review: no unsafe replay or permission bypass found.
- A newer failed attempt consumes the earlier resume instruction and remains held.

## Live evidence

The target chat is `Оценка хода FieldView global optimizer`.
Its ID is `0a1f4496-3d06-4fa7-97cd-76c7207a3e15`.

The first patch removed the reconnect block.
A real preparation timeout then produced another durable `not_applied` receipt.
The fix did not treat that timeout as a successful delivery.

A later background turn compacted the history from count 9 to 10.
The compaction patch permitted the exact saved input.
The native provider accepted it through attempt `34e62587-63f9-4897-9db7-dca90819ae5b`.
Its accepted turn is `a21d0444-99c2-4a12-872d-b8346f76174a`.

A guarded live settlement removed only the obsolete marker and its specific hold.
It checked the exact accepted attempt, source, event hashes, transcript, account, epoch, and turn.
It preserved the active turn and every pending input.

The original message `тишина`, ID `ab0e0b06-1f26-4c58-8a39-d93cbe173461`, now has status `delivered` in that turn.
The chat has no native failure hold or retry marker.
The first live settlement kept backend PID 75638. The later authorized process restart replaced it.

The temporary patches and their manifests remain in the existing state directory backup.
The installed application was signed again.
The source changes also include the retained executable recovery described in [the native recovery evidence](../research/2026-10-06-supervisor-retained-executable.md).

## Authorized native process cutover

The attached old Codex process did not answer history reads.
Three Claude lead retries also failed with `Claude Code process aborted by user`.
The user authorized a restart and requested restoration of all chats that worked today.

The recovery saved a bounded private snapshot in the existing state directory.
It contains the active scopes, pending message identities, 23 operations, and 12 monitors.
The temporary recovery configuration pause prevented a second backend during the cutover.
The backend stopped gracefully and saved restart authority.
Exact PID, process start, signature, and generation checks guarded each native close.
Durable close records and process checks confirmed every result before the supervisor restarted.

The final backend is PID 92215, in supervisor mode, with fallback disabled.
The final supervisor is PID 92109, with no degraded or blocked ownership state.
The original background recovery configuration was restored byte for byte.
The installed application was signed again.

A live `/api/state?view=chat` request completed in 1.64 seconds after startup.
The recovery used stable message IDs for every explicit continuation.
A saved existing receipt prevents duplicate submissions when the recovery script resumes.
The continuation asks each lead to restore its unfinished workers and monitors.
It requires exact receipt and result checks before any command repeats.

The first delivery readback confirmed 15 delivered continuation inputs.
Further inputs were still reserved or pending, so the recovery continued to check them.
Five lead chats and one worker now have native Claude session-limit receipts.
All six have durable resumes for 18:50:05 Europe/Warsaw.
Each resume retains the exact account, thread, and epoch.
The recovery did not change their model or account to bypass the limit.
