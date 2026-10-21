# Layr VM chats

Chat creation accepts `workspaceMode`: `layr`, `image`, or `worktree`.
A new lead chat on a Mac server defaults to `layr`. A top-level task agent from
`POST /api/agents` defaults to `worktree`. Linux servers support `worktree` only.

| Mode       | Lead         | Workers        | Reviewers                 |
| ---------- | ------------ | -------------- | ------------------------- |
| `layr`     | VM main line | Owned VM lines | Read-only VM lines        |
| `image`    | Native macOS | ASIF images    | Read-only selected folder |
| `worktree` | Native host  | Git worktrees  | Read-only selected folder |

A chat stores `workspaceMode` and derives `executionMode` (`vm` or `native`).
Workers inherit the chat mode. Native workers can select another native workspace per spawn.
VM agents stay on layr lines in the VM. The old per-worker Linux environment setting is unsupported.

## Project lifetime

The first layr chat imports the Mac project once. Later chats reuse its VM main line.
The VM main line is the source of truth. The Mac folder stays separate for native chats.
There is no automatic sync. Use `layr export` only when the user requests a copy.
The read-only share exposes main and states on the Mac.

`Client.ensure_layr_project(project_id, source, owner=None, request_id=...)` resolves the project association.
The client preserves the initial upload and import identities after a lost reply.
`project.ensure` reads the association. `project.import` creates it once.
An existing association never replaces main with the current Mac folder.

## Guest boundary

The guest service runs as `studio` on vsock port 4050.
The root broker uses `/run/codex-studio/layr-admin.sock` and accepts only the service user.
The broker owns exact request digests and durable receipts.
The guest service stays non-root with `NoNewPrivileges=true`.

`AgentHandlers` exports `METHODS` and `READ_METHODS`.
Its constructor accepts the private broker state path and the layr root.
Its `dispatch(request_id, method, params, emit)` handles line and provider operations.
The broker exposes no generic root shell command. `project.exec` runs a command from the Mac
as the project owner in main ([vm-layr.md](vm-layr.md), "Operator commands").

The project owner is `studio-p-<sha256(projectId)[:16]>`.
Each worker has a separate Linux user and a private home.
The lead provider runs as the project owner in main.
Worker providers run as their line owners. Reviewer lines are read-only at the filesystem level.
Provider argv and profile paths are fixed before the root broker drops privilege.

| Method                  | Behavior                                                      |
| ----------------------- | ------------------------------------------------------------- |
| `line.bind`             | Bind a lead to main                                           |
| `line.branch`           | Create an owned worker or read-only reviewer line             |
| `agent.context`         | Read the stored project, line, cwd, owner, UID, GID and home  |
| `line.save`             | Save one turn-end state with the exact turn receipt           |
| `line.evidence`         | Verify that the submitted state is the line head              |
| `line.merge`            | Merge with the reviewed state guard                           |
| `line.remove`           | Stop the worker provider and delete its line; keep states     |
| `agent.release`         | Stop the lead provider and release its association; keep main |
| `layr.provider.*`       | Start, inspect, stop or attach the owned provider             |
| `layr.credentials.sync` | Copy access credentials into the private agent profile        |
| `layr.exec`             | Run an owner command with a detached journal and deadline     |
| `layr.file.*`           | Read files through the line owner's permissions               |

The guest service also serves the browser bridge ([vm-browser-bridge.md](vm-browser-bridge.md)).
Agents send sign-in links on `/run/codex-studio-browser/open.sock`; the peer UID names the chat.

| Method             | Behavior                                                       |
| ------------------ | -------------------------------------------------------------- |
| `browser.wait`     | Long-poll the queued sign-in links after a sequence number     |
| `browser.answer`   | Approve or decline a link; an approval arms the loopback lease |
| `browser.callback` | Relay one cleaned redirect GET to the tool's loopback listener |
| `browser.close`    | Close a link and its lease                                     |

## Turn states and task results

Studio saves a state after each completed, failed or interrupted agent turn.
The next provider turn waits until pending state receipts have a proven result.
A lost reply preserves the exact request ID. It never creates a second state.

The worker commits with layr and submits `layr rev-parse HEAD` as its revision.
Studio stores the project, line and state with the task result.
Acceptance calls `layr merge <line> --expect <reviewed state>`.
A changed source state rejects acceptance. The lead resolves conflicts in main.
After acceptance, an inactive worker can be archived. Archive removes its line and retains its states.
Restore requires a new worker from the retained state.

## Providers and credentials

The Mac still owns sign-in and refresh. The credential flow does not change.
Codex uses the existing external-auth bootstrap and host refresh path.
Claude uses an access-only guest profile. Guest profiles contain no refresh credentials.
Each account profile stays private to the Linux user who runs the provider.

A VM chat can change to another Claude account with the normal account transfer
(`codex_vm_account_switch.py`). Every chat of the team must be idle. Studio saves the
pending turn states, stops the chat's provider and copies its native session files, as the
line owner, from the source profile into the target profile. The source files stay. The chat
keeps its thread ID, so the next turn resumes the same session with the target account. A
team with Mac chats and VM chats changes each part on its own.

Providers use the existing durable native supervisor transport.
A guest service or Studio restart can attach to the same live provider.
A VM reboot ends the process. An unproven result remains unknown.
A caller must recover its receipt before it retries a mutation.

The guest relays each provider journal request to its native supervisor with short deadlines.
It answers `outcome_unknown` when the supervisor does not answer in time.
The host sends a journal read (`next`, `replay`, `eventRead`, `info`, `status`) or an `ack`
again for up to `GUEST_PROVIDER_READ_RETRY_SECONDS` (30 s). These requests change no provider
state. A stall shorter than this does not end the provider connection. A write is never sent again.

When the connection ends, Studio settles a turn start with no turn ID in one of three ways:

- `AppServer.write` refused the start before it wrote a byte: the batch returns to the queue once.
- Native history has a turn with the exact input ID: Studio binds that turn and recovers it.
- Neither is proven: the input stays uncertain. "Check connection" reads the history again.

Use `host_exec` for macOS commands. It uses a Mac slot and returns the result through layr.
The Mac native project folder is not that slot and does not receive automatic changes.
VM command monitors use guest Bash and the owned provider sandbox.
Mac-only browser paths are not injected into guest provider configuration.

Existing native sessions keep their frozen tools, role text, and original Source headers.
The frozen runtime references are not documentation that can be revised.

## Guest updates

`upgrade-linux-vm-layr.py` updates the guest of an existing VM. Many VM agents can run at that
time, so a change that the guest can take in place does not reboot the VM:

```sh
python3 workspaces/runtime/apps/server/src/upgrade-linux-vm-layr.py --state <vm-state> --helper <helper> --operation-id <id>
```

The root broker owns the rules (`vm-guest/guest_update.py`). The Mac
(`codex_linux_vm_update.py`) asks it for a path with `update.plan`. The request has the SHA-256
of each payload file in `/opt/codex-studio` and the payload record: the Codex, Claude and pnpm
versions and the hash of the provision script. Provisioning writes that record to
`/var/lib/codex-studio/guest-payload.json`. The broker answers with one path:

- `current`: the guest has these files. The receipt is `complete` and nothing changes.
- `in_place`: only files below `vm/guest/`, `vm/layr/`, `vm/layr-desk/` or `.agents/skills/`
  changed.
- `reboot`: any other change, a different payload record, or a guest without the update key or
  the update methods (provisioned by an older Studio). The provider CLIs, the Claude bridge,
  the package manifests and `scripts/` (the process supervisor of the running providers) need
  this path.

The in-place path:

1. The Mac sends each changed file with `update.stage`, in parts of at most
   `GUEST_UPDATE_PART_BYTES` (1 MiB). Each part has its own exact request ID.
2. `update.apply` writes the files. The studio user relays every request, so the broker
   accepts only a request that the update key signs (HMAC-SHA256 of the operation ID and the
   whole manifest). The Mac keeps the key in `<vm-state>/update-key`. Provisioning writes it
   to `/etc/codex-studio/update-key`, for root only.
3. The broker runs `install.sh` with `STUDIO_DEFER_RESTARTS=1`: owners, modes, units, and a
   layr build when its source changed. The install restarts nothing.
4. After its reply the broker waits until no other broker request runs (at most 10 minutes).
   Then it restarts the changed units: the guest service and the broker for a guest Python
   module, `codex-studio-layr` for a layr source (systemd then restarts the broker too,
   because the broker requires the daemon), the share bridge for `share_bridge.py`, and a
   changed unit file. A restart of the broker runs from a separate transient unit
   (`systemd-run`). The new broker process marks that restart complete.
5. The Mac reads `update.status` until the stage is `complete`. A request that fails during
   the restarts is read again. Then the Mac checks that `update.plan` says `current` and that
   `layr.health` is `ready`.

Running agents keep their turns. A restart of the guest service or of the broker does not end
a provider: the units use `KillMode=process`, each provider runs under a detached process
supervisor, and Studio attaches to it again. A restart of the layr daemon lets its in-flight
request children finish.

An operation that wrote files and did not finish its install or its restarts stays unfinished
in the broker (`install_failed`, `restart_failed`, `writing`). The next plan and apply include
its files, also in a new operation, so the work is not lost. The Mac keeps a failed operation
in its receipt (stage `in-place`). Read `update.status` of that operation, then use a new
operation ID.

`--reboot` takes the reboot path also for a change that the guest can take in place. The
reboot path is the earlier full path: it requires idle providers, makes a disk backup, stops
the VM, writes a new provision seed and starts the VM.

A `running`, `starting` or `lost` provider in `provider.list` stops the reboot path. An
operator can approve one lost provider with `--approved-orphan <handle>`. The handle is
`linux-worker:<agent UUID>` (owner `LINUX_VM_PROVIDER_HANDLE_PREFIX` in
`runtime-schema/src/shared_values.rs`), or the
UUID name of a provider directory without `native.json`. An approved row does not block when:

- its reason is `native_process_lost`: the guest proved that the process is gone, so its
  agent ID only names the former owner.
- its reason is `outcome_unknown` and it has no agent ID.

An approved row with any other reason still blocks, because the process can still run.

## x86_64 binaries

The VM can run Linux x86_64 binaries with Rosetta for Linux, for example a prebuilt x64 tool
from a project's toolchain. This is a platform capability. It has no rules for a tool.

The VM helper (`workspaces/client/apps/desktop/native/linux-vm/main.swift`) checks `VZLinuxRosettaDirectoryShare.availability`
when it starts the VM:

- `installed`: the helper adds a virtiofs device with the tag `LINUX_VM_ROSETTA_TAG`
  (`rosetta`, owner `codex_shared_values.py`) and the Rosetta share.
- `notInstalled` or `notSupported`: the VM starts without the device. The helper does not
  install Rosetta and does not prompt. To get it, install Rosetta on the Mac
  (`softwareupdate --install-rosetta`), then restart the VM.

`host.status` (and so `Client.status()` and the `linuxVm` status) has `rosetta`:
`availability` (now), `attached` (this boot has the share) and `error` (the share failed, and
the VM started without it). `--check` prints the availability. The helper needs no entitlement
other than `com.apple.security.virtualization`.

In the guest (`vm-guest/rosetta.py`):

- `codex-studio-rosetta.service` runs `rosetta.py activate` at each boot. It mounts the share
  read-only at `/media/rosetta` and registers `/media/rosetta/rosetta` as the binfmt_misc
  handler `rosetta`, with Apple's x86_64 ELF magic and mask. The flag is `F`: the kernel opens
  the translator at registration, so x86_64 programs also start in the Codex sandbox and in
  other mount namespaces. Without `C`, a set-user-ID x86_64 file runs with the caller's
  credentials. Without the share the unit logs `STUDIO_ROSETTA: no virtiofs share` and
  succeeds, so boot does not change.
- `install.sh` runs `rosetta.py install-runtime`. It adds the `amd64` architecture, the amd64
  archive (`/etc/apt/sources.list.d/codex-studio-amd64.sources`) and installs `libc6:amd64`,
  `libstdc++6:amd64` and `zlib1g:amd64`. It uses the network only when a package is missing.
  It runs also without Rosetta, so a later install of Rosetta needs only a VM restart. A
  failure does not fail the guest install. The next install tries again. The image's arm64
  sources stay unchanged: apt skips the arm64 mirror for amd64 with a notice.

An existing VM gets the capability in two steps, in any order:

1. A guest update (`upgrade-linux-vm-layr.py`). The change is below `vm/guest/`, so it takes
   the in-place path: it installs the unit and the amd64 runtime, without a reboot.
2. A VM restart with the new helper (a Studio install, then a VM stop and start). Virtualization
   adds the device only when the VM starts. After the restart the unit mounts the share.

To check a running VM: `cat /proc/sys/fs/binfmt_misc/rosetta` shows `enabled`, and
`systemctl status codex-studio-rosetta.service` shows the last `STUDIO_ROSETTA` line.
A static x86_64 binary needs only the handler. A dynamic glibc binary also needs the amd64
runtime. A binary that needs another amd64 library needs that package too: the project
installs it, as for any other guest package.

## Guest crash watchdog

The guest kernel can panic, for example `Kernel panic - not syncing: corrupted stack end
detected inside scheduler` in a kernel thread that waits on virtio. The helper and its
Virtualization process stay alive, so the VM looked `running` while every guest request failed.

### Detection

`Client.status()` (`codex_linux_vm.py status`, `/api/desktop` `linuxVm`, the Linux VM settings)
reports two states when the helper says `running`:

- `failed`: a kernel line in `console.log` starts with `Kernel panic - not syncing:`,
  `Internal error: Oops`, `Unable to handle kernel`, `BUG: ` or `general protection fault`.
  The helper clears `console.log` at each boot, so the line is from the current boot. After
  a panic, an oops or a kernel BUG the kernel is no longer trustworthy, so this is a proven
  death. On 2026-10-13 an oops in autofs4 (guest memory zeroed under host memory pressure)
  was followed by oopses in each reader of the mount table, and `btrfs send` hung in D state
  for hours, which the host cannot see. The text must start the kernel line:
  `watchdog: BUG: soft lockup` (a stall that can pass under host load) and
  `traps: <process> general protection fault` (a user process) are not a death.
- `unresponsive`: the guest answered in this boot (same helper PID) and then gave no answer
  for 180 seconds, over at least 3 probes. The backend probes every 30 seconds and
  `ensure_running` records the first answer, in `guest-health.json`. A booting or provisioning
  guest is not unresponsive. A gap of more than 90 seconds between two probes (Mac sleep,
  backend stop) starts a new span. No answer does not prove death, so this state is never
  restarted.

The probe (`Client.ping`) sends a method that the guest service does not have, and waits for
one byte. Any byte proves that the kernel, vsock and the service run: the service answers with
an error from its event loop. The probe does not use `health`, because `health` waits for the
root broker and layr, which can be slow for minutes during a large import. The probe has
three results:

- An answer: one byte. It clears `unresponsive`.
- Busy: a close without any byte in less than 10 seconds. The guest service closes a new
  connection at once when it has 32 clients (provider streams and `browser.wait` long polls
  hold them), and the helper does so when its 64 channels are in use. A busy probe is neither
  an answer nor a failure.
- No answer: no byte before the probe timeout (15 seconds), or a close without a byte after
  10 seconds. The helper closes a vsock connection that a dead kernel never accepts after
  15 seconds.

Both states have `error` (the reason), `guestFailure` (`kernel_panic` or `unanswered`) and
`helperState: running`. The Linux VM settings show the reason. A stopped helper is `stopped`,
also when the console of its last boot has a panic.

### LAYR fault

The guest service can answer while the layr daemon or the root broker is down. On 2026-10-14
the guest answered every probe, `layr admin channel is unavailable` failed every layr call, and
all VM chats paused while the status said `running`. After a guest answer the watchdog also asks
`layr.health` (through the guest service and the root broker), at most once a minute, and saves
the result under `layr` in `guest-health.json`. Three failed answers over 120 seconds in the same
boot make a LAYR fault:

- the state stays `running`, because the providers of the VM chats still run;
- `status()` has `error` and `layrFault` (`layr_unavailable`, since, error), and the Linux VM
  settings show the reason;
- the LAYR chip of each project that is in the VM shows `failed` with the reason. The watchdog
  updates the chips when a fault starts and when it ends.

A record that is more than 5 minutes old (a stopped VM or backend) shows no fault. The watchdog
does not act on a LAYR fault. The Mac has no root path into the guest when the root broker is
down, and a VM restart ends every provider. In the guest, systemd restarts a dead daemon, broker
or guest service. Their units have no start limit (`StartLimitIntervalSec=0`): with the default
limit, a run of OOM kills could leave the daemon failed for good, and the broker requires it.

### Recovery

`codex_linux_vm_watchdog.py` runs in the macOS backend whose state directory holds the VM
state (`<state>/linux-vm`). It restarts only a proven death: a `failed` guest (kernel panic),
or a helper that reports `failed` itself (Virtualization stopped the VM with an error). For an
`unresponsive` guest it adds an episode to `watchdog.json` (start, reason, end) and shows the
last one in the status as `watchdog.lastUnresponsive`. It does not restart it: an operator
decides. For a failed VM it:

1. Copies `console.log` to `console-crash-<UTC time>.log` (the newest 5 stay) and adds a
   record to `watchdog.json`: time, reason, console tail (4 KiB), stage, outcome, and chats.
2. Stops the VM, then starts it with `ensure_running`. This normal path also ensures the Mac
   host_exec service, the guest clock, the guest configuration and the share mounts.
3. Ends the provider connections of the dead boot (see "VM chats" below).

Limits:

- One restart in 10 minutes, and at most 4 in 24 hours. A held restart shows
  `watchdog.held` (rule and time) in the status. The VM stays failed until the limits permit a
  restart or an operator restarts it.
- No restart while `upgrade-linux-vm-layr.py` holds `maintenance.lock` (for the whole upgrade),
  or while create, start or stop holds `client.lock`. An upgrade does not start during a restart.
- The watchdog acts only on a live helper. It starts a stopped VM only when its last intent is a
  start from before the current Mac boot (`kern.boottime`): the Mac restarted under the VM. The
  same limits apply, and the record has kind `host_restart`. A VM that stopped in this boot (a
  guest power-off, a helper exit) or by a stop on purpose stays stopped.
  `Client.stop()` saves `intent.json` before it takes the client lock, also for a stopped VM.
  A stop at any time before the watchdog's start cancels that start (outcome `cancelled`).
  The watchdog does not hold the client lock while the helper stops a dead kernel.
- One exception: a restart that did not reach its start (a backend stop, or a stop without a
  proven result). The next check finishes it if it is less than 30 minutes old. When the
  guest of such a restart already answers, the next check closes the restart instead.
- A failed start is recorded. If the helper still runs with a failed guest, the next attempt
  waits for the limits. A VM that did not start stays stopped.

`status()` shows the last restart and the number in the last 24 hours as `watchdog`.

### VM chats

A restart ends every guest provider. The watchdog uses the normal transport loss path for
each chat that had a provider connection in the dead boot (`runtime.disconnected`):

- A running turn becomes `interrupted` with an unknown outcome. Connection recovery checks it.
- An input in dispatch becomes `uncertain`. Studio does not send a user message or a command
  again.
- The chat is offline. Its next message starts a new provider in the new boot.

Each of these chats also gets one notice that tells the reason and the restart time.

### Why not `panic=` and `guestDidStop`

With the kernel option `panic=N`, the kernel resets after a panic. Virtualization then ends the
VM, calls `guestDidStop`, and the helper exits with code 0. The VM then looks like a VM that a
person stopped, so the backend still needs a loop that starts it and that knows a crash from a
stop. It needs a new signed helper and a VM restart on each Mac, and an oops that does not
panic never resets the kernel. The host-side check reads the panic line and the oops lines
from the console. It keeps the limits, the console copy, the records, the locks and the chat
notices in one backend path, and a backend update delivers it. It also shows a guest that gives
no answer, without a restart. A dead kernel is restarted at the next check (at most
30 seconds); an unresponsive guest shows after 3 to 4 minutes.

## Checks

Run checks one at a time:

```sh
python3 workspaces/runtime/apps/server/src/codex_python.py --exec workspaces/runtime/apps/server/tests/vm-agents-contract.py
python3 workspaces/runtime/apps/server/src/codex_python.py --exec workspaces/runtime/apps/server/src/test_codex_linux_vm_watchdog.py
python3 workspaces/runtime/apps/server/src/codex_python.py --exec workspaces/runtime/apps/server/src/test_codex_linux_vm_update.py
python3 -B -m unittest discover -s workspaces/runtime/apps/vm-guest -p test_guest_update.py
python3 workspaces/runtime/apps/server/src/codex_python.py --exec workspaces/runtime/apps/server/src/test_codex_linux_vm_rosetta.py
python3 workspaces/runtime/apps/server/src/codex_python.py --exec workspaces/runtime/apps/server/tests/linux-vm-studio-native.py --state-dir <isolated-vm-state> --helper <native-helper>
```

The first suite uses a local native provider fixture. It sends no model request.
The second suite checks the watchdog with a fake helper. It starts no VM.
The third and fourth suites check guest updates with the real broker handler and a recorded
install script and systemctl. They start no VM.
The fifth suite checks the Rosetta guest module with a recorded mount and apt, the provision
payload and the helper source. It starts no VM.
The last suite requires a provisioned isolated VM and explicit authority for live model requests.
Do not restart a VM or remove a journal that has an active or unknown provider result.
The watchdog restarts only a guest that is dead or gives no answer: Studio cannot reach its providers.
