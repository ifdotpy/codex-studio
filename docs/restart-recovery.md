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
