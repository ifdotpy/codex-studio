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
- The RxDB event cache uses weak keys so completed updates can leave memory.
  Installation applies the checked dependency patch declared in
  [`pnpm-workspace.yaml`](../pnpm-workspace.yaml).
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

### User service installation and updates

Install the Rust commands, then enable backend startup for the existing state:

```sh
just install
studio-runtime-server service install --state ~/.local/state/codex-agents
studio-runtime-server service status
```

Installation writes and enables the user configuration without starting,
restarting, or stopping a backend or supervisor. It does not open runtime state,
probe a backend, transfer active pipes, or perform a first cutover. An identical
retry is safe. A differing existing configuration is an error; resolve ownership
at an authorized idle boundary instead of overwriting it.

On Linux, the base unit uses the shared release root's `current` binary.
`just deliver-server` owns release selection, restart, health checks, and
rollback. See [Linux setup](linux-server-setup.md). Do not use a manual systemd
binary-path edit or a second backend as a source-update mechanism.

On macOS, installation writes a LaunchAgent and enables its label. It leaves
registered jobs alone and does not bootstrap a backend. Prepare the release
and bootstrap its printed plist path explicitly when the state has no competing
backend. Desktop recovery configuration is separate and stays unchanged.

`studio-runtime-server service uninstall --state DIRECTORY` disables startup
and removes the matching configuration. Running work and independent supervisor
jobs remain alive. Keep the same state and launch environment when removing it.

### Migrate an install with a legacy recovery-started owner

Install the new build and leave the existing recovery job and its supervisor
running. Desktop startup bootstraps the independent supervisor LaunchAgent; its
instance waits on the lease held by the legacy owner. The legacy owner hands
over when it exits, including at the next reboot/login or a planned stop. The
waiting LaunchAgent then checks the recorded child identities and performs the
existing crash-recovery cleanup before it accepts backend connections.

The legacy supervisor can still be torn down when launchd rewrites or boots out
the recovery job that started it. Do not disable, rewrite, or boot out that job
while its supervisor owns live handles. `workspaces/client/apps/desktop/recovery.ts` checks the
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

`just install` installs the Rust server, process supervisor, and diagnostics
commands. The standalone service command manages backend startup:

```sh
studio-runtime-server service install --state ~/.local/state/codex-agents
studio-runtime-server service status --state ~/.local/state/codex-agents
```

The command writes a state-specific LaunchAgent under `~/Library/LaunchAgents`
and enables its label. Its executable and working directory use the release
root's `current` path from
[the shared service layout](../workspaces/runtime/apps/runtime-server/service-layout.json).
Set `CODEX_STUDIO_SERVER_ROOT` before installation for a different release root.
It carries the selected state, `PATH`, and provider launch settings. It never
loads, unloads, or restarts a job. After preparing the release, bootstrap the
printed plist path explicitly at an authorized idle boundary:

```sh
launchctl bootstrap "gui/$(id -u)" /absolute/path/from-install-output.plist
```

A logged-in macOS user session is required. Confirm that no backend already owns
the selected state before bootstrapping it. Leave any independent supervisor
job in place. The CLI does not install or replace a supervisor job, rewrite
`background-recovery.json`, or switch a desktop recovery configuration.
The packaged desktop's recovery helper remains separately owned by
[`recovery.ts`](../workspaces/client/apps/desktop/recovery.ts) and
[`recover_backend.py`](../workspaces/client/apps/desktop/recover_backend.py).

The removed Python backend and `codex-supervisor enable` command have no rollback
selection in the Rust installer. Preserve the existing state directory and
release history. Any planned state restoration follows
[Rust state compatibility](rust-backend-state-compatibility.md); installation
alone never restores a backup or changes ownership records.

After an authorized backend restart, Rust recovers stored agents as follows:

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
A backend judges only the volumes of its own store: a volume whose image or mount
point is in the store, or a volume of one of its agents. A volume of another
state folder (for example the live Studio while a test fixture runs) is not a
stray volume of this backend.

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

The former Python backend command has been removed. See [testing](testing.md)
for current Rust checks.

These checks exercise abrupt process exit, backend and renderer failure, receipt
recovery, duplicate prevention, and explicit pauses. A simulated crash does not
prove behavior during a physical power failure or a complete macOS reboot.
