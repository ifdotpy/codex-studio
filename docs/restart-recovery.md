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
- In supervisor mode, a replacement backend first opens each stable native
  handle. When the supervisor confirms that the same live child resumed, Studio
  restores that handle's exact in-flight turns, accepted monitor commands, and background tasks
  before it consumes journal replay. The existing turn continues; Studio does
  not submit it again. The journal cursor and transcript item identities keep
  replayed output and completion idempotent.
- The supervisor saves each monitor command reply under its accepted operation ID.
  A replacement backend uses that ID to bind the reply to the original monitor.
  It keeps the monitor running while the native command waits to finish.
  If no matching operation exists in the same native generation, the monitor
  stays lost. Studio does not run the command again.
- A new child generation, dead or unverified child, fallback backend, disabled
  supervisor, or failed handle open does not grant reattachment. Existing
  interrupted or uncertain recovery remains in force, and accepted input is
  never resent. Monitor and task outcomes without a durable receipt remain
  unknown.
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

Unacknowledged output is limited to 256 MiB per handle. ACK removes event rows.
SQLite uses an automatic checkpoint every 100 pages.
Operation receipts have no fixed database size limit.
At the per-handle limit the supervisor stops reading that child's output pipe,
which applies backpressure to the child. Status reports the handle as stalled;
an RPC refused because the journal is full returns an explicit error to the
backend request path. It never drops output to make room. ACK cleanup does not
remove operation receipts, which remain necessary to prevent command replay.

Version 1 does not replace supervisor code while handles are active. Install a
new supervisor package only when all handles are idle; hot supervisor replacement
is deferred to v2.

`Server restarted during a turn. Review history, then send a new instruction.`
is correct when the handle did not resume the same live child and startup cannot
prove what happened to the native turn. `Studio could not confirm the exact
native turn after restart. No input was resent.` is correct when a surviving
backend cannot read authoritative state for the exact thread and turn after its
bounded recovery wait. Both messages are stale after a successful same-child
reattach: the child identity is already proven and its buffered notifications
are replayed. `Codex disconnected. Review the transcript before resuming.` is
correct when the connection was actually lost and no same-child reattach has
been confirmed. A later successful reattach clears this provisional state before
replay; an unavailable, new, or mismatched child leaves it in place.

### First cutover and restart

Use `codex-supervisor enable` for a terminal-only installation. Run
`python3 workspaces/runtime/apps/server/src/install-cli.py` first to install the command link. The command
uses the existing state directory. It does not start a second backend.

```sh
codex-supervisor enable --state ~/.local/state/codex-agents --port 4620
```

On macOS, the command preserves the saved recovery paths and environment. It
sets `supervisorEnabled=true`, installs the independent supervisor LaunchAgent,
and waits for protocol 1. It then signals only the idle backend. The recovery
LaunchAgent starts its replacement with supervisor mode enabled. A registered
supervisor or recovery job stays loaded. No desktop window is required.
The LaunchAgents require a logged-in macOS user session.

The first cutover refuses active turns, queued or uncertain input, monitors,
terminals, server commands, and supervisor handles. With a running backend, it is
a first cutover when that backend is not in supervisor mode. With no backend, it
is a first cutover unless the saved configuration has `supervisorEnabled` and
`supervisor.sqlite3` exists. Stop admission of new work
before this command. The command checks activity again before the cutover.
It preserves the state identity and checks the backend process identity.
Do not send new work until the command completes. A repeat with an existing
supervisor-mode backend does not restart it, including when handles are live.

On Linux, the command installs two systemd user units. See
[Linux and WSL server setup](linux-server-setup.md). The supervisor uses
`Restart=always` and `KillMode=process`. The backend waits for supervisor health
before startup. A planned `systemctl --user restart codex-studio` leaves the
supervisor and its children alive. Do not restart the supervisor to update the
backend. `codex-supervisor status --state <path>` reads supervisor health.
`codex-supervisor wait --state <path>` waits up to 60 seconds for protocol 1.

1. Install the desktop build containing `workspaces/runtime/apps/server/src/codex_process_supervisor.py`.
   Existing launchd recovery configuration remains off for supervisor mode by
   default. When enabled, the desktop installs a dedicated
   `local.codex.agents.supervisor.<state-hash>` LaunchAgent before launching or
   attaching the backend. The recovery job uses a separate
   `local.codex.agents.recovery.<state-hash>` label and only probes supervisor
   health. A failed or timed out probe does not start a competing owner. The
   supervisor service uses `KeepAlive` and `AbandonProcessGroup`; desktop quit,
   backend restart, or recovery-job rewrite and kickstart do not unload or
   restart it. The LaunchAgent starts its supervisor with `--wait-for-lease`.
   If a legacy recovery-started owner still holds the lease, the new instance
   stays alive, logs `supervisor waiting for owner <pid>` once, and takes over
   after that owner exits. It then runs the normal identity-checked child cleanup
   and single fallback-generation recovery.
2. Choose one idle boundary: stop admitting new work and wait for turns, monitors,
   background tasks, and user terminals to finish. The first installation cannot
   transfer existing in-process pipes, so this is the one planned interruption.
3. Set `CODEX_AGENTS_SUPERVISOR_MODE=1` in the environment used by the desktop,
   then restart the desktop recovery configuration. `workspaces/client/apps/desktop/recovery.cjs` writes
   the mode into the recovery config and installs the independent supervisor
   LaunchAgent. `workspaces/client/apps/desktop/recover_backend.py` probes the supervisor before any
   backend starts but never takes ownership of its lifecycle. Verify that
   `/api/desktop` reports protocol 1 and an empty supervisor handle list.
4. At the planned idle boundary, run
   `restart-backend-v2.sh --initial-cutover` with the same state directory
   and mode. This one-time option requires an empty supervisor handle journal,
   signals only the old backend, and waits for a supervisor-mode replacement.
   For later backend-only restarts, omit `--initial-cutover`. Do not unload the
   recovery LaunchAgent or terminate the supervisor.

### Migrate an install with a legacy recovery-started owner

Install the new build and leave the existing recovery job and its supervisor
running. Desktop startup bootstraps the independent supervisor LaunchAgent; its
instance waits on the lease held by the legacy owner. The legacy owner hands
over when it exits, including at the next reboot/login or a planned stop. The
waiting LaunchAgent then checks the recorded child identities and performs the
existing crash-recovery cleanup before it accepts backend connections.

The legacy supervisor can still be torn down when launchd rewrites or boots out
the recovery job that started it. Do not disable, rewrite, or boot out that job
while its supervisor owns live handles. `workspaces/client/apps/desktop/recovery.cjs` checks the
recovery job PID, supervisor parent PID, and supervisor status before a restart
or bootout, and refuses the operation while that legacy owner has live handles.
Wait for the legacy owner to exit or its handles to close, then retry the
recovery-job change. The supervisor LaunchAgent remains loaded and takes the
lease when it becomes available.

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

The supervisor checks that at least 272 MiB is free before it creates the
journal and checks free space before durable writes. Low disk space stalls child
output with pipe backpressure; diagnostics identify the affected handle and
state. The UI shows that output has paused. A temporary storage error keeps the
same output frame in memory until SQLite can save it. Operation receipts remain
available to prevent command replay. Native binary replacement is
deferred while supervisor mode is on; v1 keeps the current child identity until
fallback or a planned idle cutover. Native tool catalog replacement
also stays deferred when it requires stopping the child. Existing chats continue
with their current catalog; an unchanged native catalog can still be confirmed.
Active-handle supervisor code replacement
is deferred to v2.

### Backend update with live supervisor handles

This recovery change requires a backend update only. Keep the current supervisor
and its state directory in place. Restart the backend through the existing
recovery path; do not stop the supervisor or its native children. The new backend
uses each handle's `resumed` receipt to restore active runtime records before
journal replay. Verify that the backend reports the original native child PID,
that active turns return to `running`, and that replay settles each final item
and completion once. If a handle opens as a new generation or cannot be verified,
the backend keeps the existing interruption/uncertainty behavior. No database
migration or manual receipt cleanup is required.

No application can guarantee zero data loss after physical storage failure.
Data not yet committed before power loss can be absent. A full or unwritable disk
can prevent both database and journal persistence; Studio must report that failure.
The journal does not constitute a backup of the complete state directory.

### Rust backend and Rust supervisor on macOS

Python stays the default. An operator selects Rust explicitly. The Rust backend
starts providers only through supervisor protocol 2, and only the Rust
supervisor (`codex-process-supervisor`) serves it. The Rust supervisor also
serves the protocol 1 health and `finishFallback` requests that recovery uses.
A Rust backend therefore needs the Rust supervisor job, and a switch in either
direction restarts provider processes.

`background-recovery.json` holds the selection:

| Field               | Meaning                                                                                                                                                |
| ------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `backend`           | `python` (default when absent) or `rust`. Owner: `BACKEND_SELECTIONS` in [`recover_backend.py`](../workspaces/client/apps/desktop/recover_backend.py). |
| `runtimeServer`     | Absolute path of `studio-runtime-server`. Required for `rust`.                                                                                         |
| `processSupervisor` | Absolute path of `codex-process-supervisor`. Required for `rust` with `supervisorEnabled`.                                                             |

With `rust`, `recover_backend.py` keeps every Python rule:

- It attaches to any backend that `/api/desktop` reports for the state. When
  `backendKind` differs from the selection, it reports
  `attached to <kind> backend; <selection> selected` and never replaces it.
- It starts nothing while `runtime.lock` is held.
- It starts `<runtimeServer> --port N` only when that file is executable. The
  Rust server finds its web assets next to its executable. Recovery does not
  write `rust-state-owner.json`; the Rust server records it after it takes
  `runtime.lock` (see
  [Rust backend state compatibility](rust-backend-state-compatibility.md)).
- Before a fallback signal, the reported PID must run the configured
  executable (first launch argument), report `backendKind: rust` and a
  `backendBuild` equal to the SHA-256 of that file, and own the listener.
- The supervisor probe first sends a protocol 2 handshake with `probe: true` to
  `supervisor.sock`. A probe takes no ownership. A refused connection reports
  `supervisor starting`. A supervisor that answers without protocol 2 (the
  Python supervisor) blocks the start with an error. The protocol 1 health
  probe then reads the crash-recovery state as before. Recovery never starts or
  stops the supervisor.

The swap ends the provider processes, so the Rust backend recovers the agents
that Python wrote:

- A restart marker counts only while it is `pending` or `superseded`. A
  disconnect receipt counts only while the agent is `interrupted`. A finished,
  continued, or held marker is history, and a later provider generation does not
  interrupt or hold the agent again.
- A turn with a known identity becomes `interrupted` with a `pending` restart
  marker, as in Python. The first account worker reads that turn from native
  history. An interrupted turn gets one restart follow-up; a completed or failed
  turn is recorded and reported. The old input is not sent again. That first
  pass reads every agent of the account, not only the first scan page.
- A later finished turn of the same thread closes a marker that is still
  `pending` (for example, after a failed history read). The next restart does
  not interrupt that agent again for the old turn.
- An idle agent whose attempt turn is its last finished turn is not changed.
  This includes a turn that ended `failed` or `interrupted`: a backend stop,
  the next start and a new provider generation keep its status and its error.
- Only an input without a turn identity stays held. A new user message clears
  that hold, and later generations do not apply it again.
- An `uncertain` input is never sent again, and it does not stop new input.
- The Claude bridge of a non-default account runs in
  `<state>/account-servers/<account>`, as in Python, so its session snapshots
  are found.
- A ready worktree needs only its Git registration. Its Git budget starts with
  the first Git command, and a failure names that command and the Git error.

Install the selection with the service command. The Rust binaries come from
`CODEX_RUNTIME_SERVER` and `CODEX_PROCESS_SUPERVISOR`, else `<resources>/bin`,
else `<resources>/target/release`:

```sh
CODEX_DESKTOP_BACKEND=rust codex-supervisor enable --port 4620 --state ~/.local/state/codex-agents
```

When no job is registered, `enable` writes the selection, installs a supervisor
job that runs `codex-process-supervisor --state <state> --wait-for-lease`,
installs the unchanged recovery job (`recover_backend.py --config ...`), and
waits up to 90 seconds for `/api/desktop` to report `backendKind: rust`. It
refuses while a Python backend serves the state or another process holds
`runtime.lock`. Registered jobs of the selected kind stay loaded.

`enable` never unloads or restarts a registered supervisor or recovery job.
The recovery job reads the selection on every tick, so a selection change is
also a job change. When a registered supervisor runs the other backend, when a
registered supervisor has arguments that `enable` cannot identify and Rust is
selected, or when the recovery job is registered and the saved selection
differs, `enable` changes nothing. It prints a report with `swapRequired: true`,
the changed jobs, and the swap steps, and exits with status 75.

Swap at a quiet moment:

1. Confirm that no agent, monitor, terminal, or command runs.
2. `launchctl bootout gui/<uid>/local.codex.agents.recovery.<state-hash>`.
3. Send SIGTERM to the backend PID that `/api/desktop` reports for this state.
4. `launchctl bootout gui/<uid>/local.codex.agents.supervisor.<state-hash>`.
   This ends provider processes.
5. Detach the image workspace volumes and record them. Follow
   [Image workspace mounts](#image-workspace-mounts), steps 1 to 3. If one
   detach fails, stop the cutover.
6. Make the state backup that
   [Rust backend state compatibility](rust-backend-state-compatibility.md)
   requires before the first Rust start. Use step 4 of
   [Image workspace mounts](#image-workspace-mounts).
7. Run the same `enable` command again. With no registered job and no backend,
   it installs and starts both jobs. Then attach the recorded volumes again
   (step 6 of [Image workspace mounts](#image-workspace-mounts)), or let each
   agent attach its volume when it starts.

Rollback uses the same steps with the Python selection
(`codex-supervisor enable` without `CODEX_DESKTOP_BACKEND`). The Rust backend can
attach volumes, so step 5 is necessary again. In step 6, restore the pre-Rust
backup instead (step 5 of [Image workspace mounts](#image-workspace-mounts)):
Python refuses a state that the Rust server owns. When the Rust server never
started (no `rust-state-owner.json`), there is nothing to restore. The rollback does not
require idle agents: the saved `supervisorEnabled` and the existing
`supervisor.sqlite3` show that this is not a first cutover, and the swap already
ended the provider processes. `enable` then saves
`backend: python`, removes `runtimeServer` and `processSupervisor`, and installs
the Python supervisor job (`codex_process_supervisor.py --wait-for-lease`).

The Electron desktop keeps a saved Rust selection when it rewrites the recovery
configuration, and it does not write a Python supervisor job over it.
`restart-backend-v2.sh` has no Rust-specific checks and has not been tested with the Rust backend.

### Image workspace mounts

Each image workspace is an APFS volume. Its image is
`<state>/workspaces/agents/<agent>/workspace.asif`, and it is attached at
`<state>/workspaces/mnt/<agent>`. A volume stays attached at the path where it
was attached. These operations strand a volume:

- `mv` of a state folder that has attached volumes. The volumes move with the
  folder, and the live mount points become empty folders.
- `cp -c -R` of a state folder that has attached volumes. The copy contains the
  files of each volume as plain files. A restore of that copy puts a plain
  folder at the live mount point.

Studio uses a mount point only when the volume there carries the agent's marker
and the image file has the inode that Studio recorded at attach (see
[Image workspaces](workspace-images.md#mount-identity)). It refuses a plain folder
with files, a volume of another image, a second attachment of the same volume,
and a replaced image file. Archive and removal refuse in the same states, so they
cannot delete an image whose volume is attached somewhere else. Each backend
checks the mounts once at start. It writes each problem to stderr and shows the
problem codes in `/api/diagnostics` (`imageMounts`). The check repairs nothing.

`studio-runtime-server mounts check [--store <store>]` prints the full report as
JSON and changes nothing. The default store is `<state>/workspaces`. The command
exits with 0 when there is no problem and no stray volume, and with 1 otherwise.
`studio-runtime-server mounts repair [--store <store>]` does only one step: it
attaches each ready agent that has `mounted: true` in its `agent.json`, an empty
mount point, and no attachment of its volume in another place. It does not
detach, move, or delete anything. Every other problem needs an operator.

Use this procedure around a backup, a restore, or a move of the state folder. Stop
the backend first. In the commands, `$SERVER` is the absolute path of
`studio-runtime-server` (the `runtimeServer` value in `background-recovery.json`).

```sh
STATE="$HOME/.local/state/codex-agents"
SERVER=/absolute/path/to/studio-runtime-server
```

1. Check the mounts and record the attached volumes. If the command exits with
   1, stop and resolve each reported problem first.

   ```sh
   "$SERVER" mounts check --store "$STATE/workspaces" > "$STATE.mounts.json" &&
   python3 -c 'import json, sys; print("\n".join(json.load(sys.stdin)["mountsUnderStore"]))' \
     < "$STATE.mounts.json" > "$STATE.mounts.txt"
   ```

2. Detach each recorded volume without `-force`. Stop at the first failure;
   a busy volume has an open process that must stop first.

   ```sh
   while read -r mount; do hdiutil detach "$mount" || break; done < "$STATE.mounts.txt"
   ```

3. Make sure that no volume is mounted under the state folder. Do not continue
   while this command prints a line.

   ```sh
   /sbin/mount | grep -F " on $STATE/" && echo "Volumes are still mounted under $STATE" >&2
   ```

4. Back up. The backup does not contain `workspaces/mnt`; it gets an empty
   `mnt` folder. `cp -c` clones the images, so the backup needs almost no
   space. Do the step 3 check again immediately before this command.

   ```sh
   BACKUP="$STATE.backup-$(date +%Y%m%d-%H%M%S)"
   mkdir "$BACKUP" "$BACKUP/workspaces" &&
   (cd "$STATE" && find . -mindepth 1 -maxdepth 1 ! -name workspaces -print0 |
     xargs -0 -I{} cp -c -R {} "$BACKUP/") &&
   (cd "$STATE/workspaces" && find . -mindepth 1 -maxdepth 1 ! -name mnt -print0 |
     xargs -0 -I{} cp -c -R {} "$BACKUP/workspaces/") &&
   mkdir "$BACKUP/workspaces/mnt"
   ```

5. To restore, do steps 1 to 3 on the live state folder first. Never `mv` a
   state folder while step 3 prints a line. Then keep the live folder as a
   separate recovery point and clone the backup into its place:

   ```sh
   mv "$STATE" "$STATE.replaced-$(date +%Y%m%d-%H%M%S)" &&
   cp -c -R "$BACKUP" "$STATE"
   ```

6. Before the backend starts, run `"$SERVER" mounts check --store "$STATE/workspaces"`.
   Then run `"$SERVER" mounts repair --store "$STATE/workspaces"` to attach
   the recorded volumes again. Its `actions` list shows each attached agent. An
   agent that it does not attach attaches its volume when it starts, or the
   start fails with the reason.

## Verification

Run the isolated recovery checks:

```sh
python3 -B workspaces/runtime/apps/server/tests/restart-recovery-contract.py
python3 -B workspaces/runtime/apps/server/tests/monitor-restart-contract.py
python3 -B workspaces/runtime/apps/server/tests/terminal-history-restart-contract.py
pnpm --filter codex-agents-desktop run test
pnpm --filter codex-agents-web run test:restart
pnpm --filter codex-agents-web run test:rxdb-cache
pnpm --filter codex-agents-web run test:display
pnpm --filter codex-agents-web run test:diagnostics
```

These checks exercise abrupt process exit, backend and renderer failure, receipt
recovery, duplicate prevention, and explicit pauses. A simulated crash does not
prove behavior during a physical power failure or a complete macOS reboot.
