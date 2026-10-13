# Layr VM chats

Chat creation accepts `workspaceMode`: `layr`, `image`, or `worktree`.
On macOS, `layr` is the default. Linux servers support `worktree` only.

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

Providers use the existing durable native supervisor transport.
A guest service or Studio restart can attach to the same live provider.
A VM reboot ends the process. An unproven result remains unknown.
A caller must recover its receipt before it retries a mutation.

Use `host_exec` for macOS commands. It uses a Mac slot and returns the result through layr.
The Mac native project folder is not that slot and does not receive automatic changes.
VM command monitors use guest Bash and the owned provider sandbox.
Mac-only browser paths are not injected into guest provider configuration.

Existing native sessions keep their frozen tools, role text, and original Source headers.
The frozen runtime references are not documentation that can be revised.

## Guest crash watchdog

The guest kernel can panic, for example `Kernel panic - not syncing: corrupted stack end
detected inside scheduler` in a kernel thread that waits on virtio. The helper and its
Virtualization process stay alive, so the VM looked `running` while every guest request failed.

### Detection

`Client.status()` (`codex_linux_vm.py status`, `/api/desktop` `linuxVm`, the Linux VM settings)
reports `state: failed` when the helper says `running` and one of these is true:

- `console.log` has a `Kernel panic - not syncing:` line. The helper clears `console.log` at
  each boot, so the line is from the current boot.
- The guest answered in this boot (same helper PID) and then gave no answer for 180 seconds,
  over at least 3 probes. The backend probes every 30 seconds and `ensure_running` records
  the first answer, in `guest-health.json`. A booting or provisioning guest is not a failure.
  A gap of more than 90 seconds between two probes (Mac sleep, backend stop) starts a new span.

The probe (`Client.ping`) sends a method that the guest service does not have. The service
answers it with an error from its event loop. Any answer proves that the kernel, vsock and the
service run. Only a transport failure counts as no answer. The probe does not use `health`,
because `health` waits for the root broker and layr, which can be slow for minutes during a
large import. One answer clears an `unanswered` verdict, also while a restart waits.

The failed status has `error` (the reason), `guestFailure` (`kernel_panic` or `unanswered`) and
`helperState: running`. The Linux VM settings show the reason. A stopped helper is `stopped`,
also when the console of its last boot has a panic.

### Recovery

`codex_linux_vm_watchdog.py` runs in the macOS backend whose state directory holds the VM
state (`<state>/linux-vm`). For a failed VM, including a helper that reports `failed`, it:

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
- The watchdog acts only on a live helper and never starts a stopped VM.
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
stop. That option also covers panics only, not a guest that hangs without a panic. It needs a
new signed helper and a VM restart on each Mac. The host-side check covers both cases. It
keeps the limits, the console copy, the records, the locks and the chat notices in one backend
path, and a backend update delivers it. A panic shows at the next check (at most 30 seconds);
a hang shows after 3 to 4 minutes.

## Checks

Run checks one at a time:

```sh
python3 workspaces/runtime/apps/server/src/codex_python.py --exec workspaces/runtime/apps/server/tests/vm-agents-contract.py
python3 workspaces/runtime/apps/server/src/codex_python.py --exec workspaces/runtime/apps/server/src/test_codex_linux_vm_watchdog.py
python3 workspaces/runtime/apps/server/src/codex_python.py --exec workspaces/runtime/apps/server/tests/linux-vm-studio-native.py --state-dir <isolated-vm-state> --helper <native-helper>
```

The first suite uses a local native provider fixture. It sends no model request.
The second suite checks the watchdog with a fake helper. It starts no VM.
The third suite requires a provisioned isolated VM and explicit authority for live model requests.
Do not restart a VM or remove a journal that has an active or unknown provider result.
The watchdog restarts only a guest that is dead or gives no answer: Studio cannot reach its providers.
