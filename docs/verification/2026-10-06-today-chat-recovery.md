# Today chat recovery

## Scope

Restore the chats that worked on October 6 in Europe/Warsaw.
Keep their history, pending inputs, exact operation identities, and existing work.
Never repeat an operation with an unknown result.

The initial inventory found 12 lead chats and 50 agents with work today.
The initial inventory found Claude session limits with a reset time of 18:50.
That time has passed. The latest limit receipts do not prove a current limit.
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

All 23 explicit continuation inputs reached status `delivered`.
Later native turns and transcript items confirm work in all 12 lead chats.
Some leads then complete their work or wait for active workers and monitors.
Those states do not require an invented task or a false running label.
The recovery preserves each account, model, thread, and epoch.

## Shared lock defects

A private stack snapshot records the actual lock owners in backend PID 92215.
An account snapshot holds the account lock while the Claude CLI waits for authentication.
The native sweep holds the runtime lock while it waits for that account lock.
The Claude cache also holds its global lock during the CLI call.
These waits stop unrelated dispatch and HTTP requests.

Account methods now capture profile fields under the lock and run authentication outside it.
They check the current profile fields and identity pins again before they publish the result.
The Claude cache permits one probe per exact profile. It releases its global lock during the probe.
A forced probe cannot accept a result that started before the forced request.
The native sweep reads provider types from the registry before it takes the runtime lock.
It does not authenticate accounts.

Fourteen focused account tests pass against both the repository and the installed candidate.
The sweep tests reject the previous lock and authentication behavior.
The live update receipts confirm these changes in the existing backend.

The account display endpoint previously ran three native probes in sequence.
Each probe can wait eight seconds. The renderer stops the request after 15 seconds.
The endpoint now reads a deep copy of the saved registry.
Its authentication notice query uses a read connection without the runtime lock.
Login, mutation, discovery, and native connection paths retain authoritative authentication checks.
Eight focused tests pass, including a blocked runtime lock and an active SQLite writer.
Three live endpoint reads finish in 10.668, 2.007, and 2.008 milliseconds.

The diagnostic snapshot had the same authentication call under the runtime lock.
It now reads saved provider types after it releases the runtime lock.
The account lock and runtime lock have separate scopes.
Three new regressions fail on the former code and pass on the fix.
The existing HTTP and CLI diagnostic contract also passes.
An initial live diagnostic request finishes in 553.66 milliseconds after the update.
That snapshot has zero pending native callbacks, clock replies, tool requests, and recovery entries.
Its runtime lock samples report waits from zero to 0.01 milliseconds.
These counts do not imply that every historical durable input is empty.

## Long SQLite snapshot

The current transaction journal records a 262.897-second read transaction in the session cost computation.
The computation waits for account and transcript preparation inside its SQLite snapshot.
The fix closes the initial snapshot before this preparation.
A final snapshot checks the same root usage and agent identities before it reads usage rows.
Changed inputs cause a bounded retry.

The regression blocks preparation and adds a WAL write through another connection.
The old code prevents the checkpoint. The fixed code permits it.
Seven focused tests pass against both the repository and the installed candidate.

This receipt does not identify the owner of the earlier 282-second transaction.
That transaction predates the retained stack evidence. Its original owner remains unknown.

## Native authentication wait

Read-only native `auth status --json` probes exceed 30 seconds for the three Claude profiles.
A sample at 18 seconds shows the CLI child waiting in macOS Security client initialization.
The child waits in `SecurityServer::ClientSession::activate` through `mach_msg2_trap`.
Its CPU use is zero. Its priority is 31.
The probe does not reach the keychain item lookup in that sample.

The SecurityServer Mach endpoint exists. A separate `security list-keychains` call finishes in 20 milliseconds.
These facts do not prove a locked keychain, a missing service, or a permission dialog.
The recovery does not replace account proof with old cached identity data.
Later native authentication succeeds for the same profiles.
The recovery does not change a Security service, credential, or account identity.
Two lead inputs then fail before submission because the native SDK returns incomplete account metadata.
Fresh native queries subsequently restore both leads.
Actual assistant output and native tools confirm work in all twelve lead chats.

## Continue after a local authentication timeout

A local authentication timeout can occur before Studio submits native input.
The old dispatcher retains the original events but leaves the actor failed.
A later successful authentication check does not resume those events.

The dispatcher now records only an exact, unsubmitted local authentication timeout.
It preserves the original event IDs, account identity, actor settings, and task ownership.
The actor waits instead of reporting a false worker failure to its parent.
At most two profile probes run outside the shared locks.
Each profile has one probe and a 180-second interval.
A fresh matching account proof queues the same events through the normal dispatcher.
Stop, changed input, account changes, and task ownership changes cancel this automatic continuation.
Unknown native outcomes and submitted inputs cannot use this mechanism.

Sixteen focused tests pass against the repository source.
Sixteen tests also pass against the installed candidate. Seven hot-update guard tests pass.
A separate real worker fixture confirms that the task owner stays assigned during the wait.
It also confirms that repeated dispatch does not send native input or report a false child result.
The live receipt records application of the update in backend PID 92215.
Its manifest hash is `4c91ef7fe1b8bfcab91ff95391c233d1a8b7e06bd200a963d55b3ba45b651655`.
Nine new explicit continuation messages use stable IDs and preserve the previous operation receipts.
The native account checks succeed before those successful turns start.

## Context repair receipts

Context repair can verify an exact terminal turn before its callback reaches Studio.
The repaired actor then moves to another thread.
The old callback previously found no actor and lost the parent result.

Repair now saves a bounded source receipt before the fork.
Only a real terminal callback creates the completion record and parent event.
The callback must match the source turn, account, epoch, repair chain, and native process generation.
It preserves the new thread, its input, and its active tools.
Eleven additional tests cover late callbacks, duplicate callbacks, restart, and source changes.

The USDZ lead also waits for a native `thread/resume` acknowledgement for more than one hour.
A read-only inspector later observes its completed preparation future and no pending native requests.
The original reserved event reaches turn `01a11266-462f-7801-a246-357e6f2634ac` once.
Native user input, assistant output, and command results confirm that the chat works again.
The inspector does not submit, replay, interrupt, or restart a native operation.
This observation does not establish a source defect that caused the original delay.

## Fresh native account metadata

The Claude SDK keeps its first initialization account snapshot in `accountInfo()`.
Its `reinitialize()` control returns fresh account metadata from the same native query.
The previous bridge rejects missing initial email metadata without this fresh check.
The saved failures prove that the original input was not submitted.

The bridge now permits one fresh check for incomplete metadata before input admission.
It retains the original deadline and exact configured account identity.
A known foreign account, provider, or API key cannot use this check.
A Stop, changed query, or changed settings still prevents input admission.
The catalog returns the verified fresh account instead of the original incomplete snapshot.

The previous source fails the missing-email regression.
Sixty-nine focused tests pass in the worker checkout.
Eleven new tests pass against both the installed candidate and the final repository source.
An independent review passes all eleven tests and checks the native SDK control.

The installed bridge SHA-256 is `4e2d4ecda579c4c7f3de71d31e0a734824a18a152b9b787ec7a37911cc5aa176`.
The file replacement preserves backend PID 92215 and the backend source inventory.
Existing Node processes retain their current code and active queries.
The new bridge code applies on the next ordinary native bridge start.
The source tests do not prove a live execution of the fresh-control branch.

The showroom continuation message is `ff64ff33-9fb6-5016-9424-c2a362385127`.
Its accepted native turn is `e6afabfe-f870-48c7-a107-5ea2e7941734`.
The Attar lead also produces native tools in turn `672aa316-9032-442a-b1f4-93703549e264`.
Both proofs precede the bridge file replacement.

The final inventory at 18:31:24 UTC has 12 lead chats and 52 current agents with work today.
It has no additional lead chats and no failed current agents.
All twelve leads have automatic wake enabled.
An explicitly stopped, superseded worker remains paused.

The final application signature passes `codesign --verify --deep --strict`.
The final desktop identity retains PID 92215, the original state directory, and supervisor mode without fallback.
A later account display request finishes in 3.405 milliseconds.
A later diagnostic request finishes in 879.105 milliseconds during active work.
It has two pending callbacks, zero pending tool requests, and zero pending recovery entries.
Its largest sampled runtime lock wait is 12.04 milliseconds.
