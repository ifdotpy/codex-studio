# Restart recovery

Studio keeps its runtime database in the existing state directory. The packaged
macOS application installs a per-user background recovery service. The application
menu can disable that service without stopping the server or its active work.

## Automatic recovery

- The service starts after macOS login and restores a stopped backend.
- Closing the desktop window preserves the backend. A renderer crash reloads its
  interface. The retry budget resets after 60 seconds with a healthy renderer.
  Repeated immediate failures show a reload page. The native Reload command works
  when the renderer has crashed or has no focus. Crash reasons enter the desktop
  profile's `renderer-recovery.jsonl`. The service can also restore a crashed
  desktop process. It respects an explicit Quit. After a normal macOS shutdown,
  window reopening follows the macOS preference; the backend still starts after login.
- The desktop restores normal window bounds and the maximized state. It adjusts
  saved bounds when the available displays change.
- A subagent card or conversation render error shows a local retry control.
  Other interface sections remain available. An error outside those sections shows
  a Studio recovery page. Recovery does not clear saved chats or drafts.
- Native, account, task, and approval diagnostics display text summaries with
  their structured details. HTTP failures retain the status and original response.
  Display recovery does not approve requests or repeat failed actions.
- The backend restores queued input that was recorded before native submission.
  It retains each original message identity and payload.
- Previously active work retains its continuation permission, account, epoch,
  thread, and turn identity. Native history must confirm the previous outcome.
  Confirmed completion restores result delivery. Confirmed interruption can queue
  one continuation when no operation or input has an unknown outcome.
- Explicit user pauses, failed turns, budget limits, account restrictions, and
  unknown mutations remain protected. Reopening Studio does not override them.
- Definitive monitor results enter a separate durable journal before the SQLite
  result commit. Restart restores the exact result and its event. The journal
  also covers a result received during graceful server shutdown.
- A rule resumes only when its exact interrupted check has a definitive saved
  result. A stopped or replaced rule does not acquire new permission.
- Local questions remain available when their owner still has valid permission.
  Native approval requests belong to their original process and expire.
- Monitor logs retain complete future output. Terminal output has a persistent
  archive. The live terminal view remains bounded. Older output discarded by a previous release cannot be reconstructed.
- Drafts and accepted outbox messages retain their workspace identity. Sending
  clears the draft and attachment selection only after local persistence succeeds.
  Non-secret question answers, task notes, complaint replies, profile and rule
  drafts, and chat scroll positions survive interface reload.
- Selected upload bytes and their stable IDs commit locally before upload starts.
  Saved uploads resume only for the same workspace. Confirmed attachment references
  persist before the local file journal entry is removed. Storage refusal keeps
  the failure visible; an uncommitted file has no recovery guarantee.
- Each chat refresh reads only its saved projection. It does not scan other
  cached chats or create bidirectional replication metadata. Storage revisions and
  server sequence numbers protect concurrent writes and removed-chat records.
- The RxDB 17.5.0 event cache uses weak keys so completed updates can leave memory.
  The install, build, and development commands apply the checked dependency patch.
  The patch also preserves projection conflicts for the server sequence check.
  A different dependency version requires review of that patch.

## Limits

An operation can finish outside Studio before its response reaches durable
storage. Without an authoritative receipt, Studio cannot determine whether it
executed. It keeps the request visible and does not repeat that operation.
An incomplete native history can also prevent automatic continuation.

A host reboot destroys local process memory, terminal shells, and open native
connections. Studio preserves recorded output and work state. It cannot restore
an arbitrary process at its previous instruction or safely rerun every command.
Remote processes can remain alive; their owners must check them before a retry.

A powered-off or sleeping Mac cannot serve the iPhone. FileVault unlock and user
login precede the per-user service. Missing network access, expired credentials,
and provider limits can delay work. An alive server with an uncertain identity is
not replaced automatically.

Microphone capture, voice calls, secret form answers, and native permission
requests require a new user action. iOS can suspend a PWA and prevent background
message delivery. Browser storage eviction or deletion removes local-only data.
Fullscreen mode, minimized state, and the previous macOS Space are not restored
automatically.

## Opt-in process supervisor (v1)

`CODEX_AGENTS_SUPERVISOR_MODE=1` enables the process supervisor. The default is
off. The desktop copies this setting into its background-recovery configuration
and passes it to replacement backends. When enabled, startup verifies the
packaged supervisor protocol and state-directory identity before creating any
AppServer. A missing or incompatible supervisor is a startup error; the backend
does not fall back to in-process children.

The supervisor owns Codex app-server and Claude bridge children, including turns,
monitors and background tasks carried by those app-servers, plus the terminal
app-server and its user PTYs. The backend reconnects using stable handles and
sequence cursors. The supervisor accepts each RPC into its SQLite journal with
`synchronous=FULL` before writing stdin. There is no fsync batching: acceptance
latency measured 1 October 2026 on a private fake-model fixture was p50 6.655 ms
and p95 18.854 ms over 80 RPCs. These figures are host- and storage-dependent.
If the supervisor dies between the durable receipt and the child write, the
operation outcome is uncertain; the receipt prevents an automatic retry.

Unacknowledged output is limited to 256 MiB per handle. ACK removes event rows;
the supervisor checkpoints and truncates its WAL after ACK, with an automatic
checkpoint every 100 pages. The SQLite database also has a 256 MiB page limit.
At the per-handle limit the supervisor stops reading that child's output pipe,
which applies backpressure to the child. Status reports the handle as stalled;
an RPC refused because the journal is full returns an explicit error to the
backend request path. It never drops output to make room. ACK cleanup does not
remove operation receipts, which remain necessary to prevent command replay.

Version 1 does not replace supervisor code while handles are active. Install a
new supervisor package only when all handles are idle; hot supervisor replacement
is deferred to v2.

### First cutover and restart

1. Install the desktop build containing `scripts/codex_process_supervisor.py`.
   Existing launchd recovery configuration remains off for supervisor mode by
   default. When enabled, the desktop installs a dedicated
   `local.codex.agents.supervisor.<state-hash>` LaunchAgent before launching or
   attaching the backend. The recovery job uses a separate
   `local.codex.agents.recovery.<state-hash>` label and only probes supervisor
   health. A failed or timed out probe does not start a competing owner. The
   supervisor service uses `KeepAlive` and `AbandonProcessGroup`; desktop quit,
   backend restart, or recovery-job rewrite and kickstart do not unload or
   restart it.
2. Choose one idle boundary: stop admitting new work and wait for turns, monitors,
   background tasks, and user terminals to finish. The first installation cannot
   transfer existing in-process pipes, so this is the one planned interruption.
3. Set `CODEX_AGENTS_SUPERVISOR_MODE=1` in the environment used by the desktop,
   then restart the desktop recovery configuration. `desktop/recovery.cjs` writes
   the mode into the recovery config and installs the independent supervisor
   LaunchAgent. `desktop/recover_backend.py` probes the supervisor before any
   backend starts but never takes ownership of its lifecycle. Verify that
   `/api/desktop` reports protocol 1 and an empty supervisor handle list.
4. At the planned idle boundary, run
   `scripts/restart-backend-v2.sh --initial-cutover` with the same state directory
   and mode. This one-time option requires an empty supervisor handle journal,
   signals only the old backend, and waits for a supervisor-mode replacement.
   For later backend-only restarts, omit `--initial-cutover`. Do not unload the
   recovery LaunchAgent or terminate the supervisor.

The supervisor LaunchAgent survives desktop quits, recovery-job restarts, and
backend restarts, and starts at login after a host reboot. It records its PID
and start time, plus each native child's PID, process group, and start time.
After supervisor death, recovery verifies these identities. It sends TERM, then
KILL after 1.5 seconds, only to a process group whose PID and start time still
match. A missing or reused PID is never signalled and blocks fallback. When
ownership is proven, recovery retains all receipts, records the cleanup result,
and starts one backend generation with supervisor mode off. Existing turn,
monitor, and terminal recovery handles interrupted work; accepted or uncertain
requests are not resubmitted. The UI and `/api/diagnostics` show one recovery
notice. After that fallback backend exits, the next backend generation returns
to supervisor mode. The v1 tests exercise process death using private state and
fake children; they do not prove model-provider behavior under a host power
failure.

The supervisor checks that at least 272 MiB is free before creating its bounded
journal and checks free space before durable writes. Low disk space stalls child
output with pipe backpressure; diagnostics identify the affected handle and
state. The UI shows that output has paused. The overall database page limit is
256 MiB, including retained operation receipts. Native binary replacement is
deferred while supervisor mode is on; v1 keeps the current child identity until
fallback or a planned idle cutover. Native tool catalog replacement
also stays deferred when it requires stopping the child. Existing chats continue
with their current catalog; an unchanged native catalog can still be confirmed.
Active-handle supervisor code replacement
is deferred to v2.

No application can guarantee zero data loss after physical storage failure.
Data not yet committed before power loss can be absent. A full or unwritable disk
can prevent both database and journal persistence; Studio must report that failure.
The journal does not constitute a backup of the complete state directory.

## Verification

Run the isolated recovery checks:

```sh
python3 -B tests/restart-recovery-contract.py
python3 -B tests/monitor-restart-contract.py
python3 -B tests/terminal-history-restart-contract.py
npm --prefix desktop test
npm --prefix web run test:restart
npm --prefix web run test:rxdb-cache
npm --prefix web run test:display
npm --prefix web run test:diagnostics
```

These checks exercise abrupt process exit, backend and renderer failure, receipt
recovery, duplicate prevention, and explicit pauses. A simulated crash does not
prove behavior during a physical power failure or a complete macOS reboot.
