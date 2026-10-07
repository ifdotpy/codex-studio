# Linux VM workspaces: implementation contract

Status: in progress (2026-10-07). Goal: production-ready Linux environments for Studio agents on
macOS. Linux VM workspaces are an addition for Linux projects. macOS projects keep ASIF image
workspaces ([workspace-images.md](workspace-images.md)), because Xcode and Swift need macOS.

## Decisions

1. Studio runs its own Linux VM with Apple `Virtualization.framework` through a signed native
   helper. No dependency on OrbStack, Docker or Lima.
2. One long-lived VM for all Linux agents. Agents get btrfs snapshots inside the VM, through the
   existing Linux backend (`scripts/codex_workspace_linux.py`).
3. The code lives inside the VM. The base is copied once; later changes use the existing change
   detector (git status and HEAD diff, rsync for folders without git), not FSEvents.
4. Agents (Codex and Claude, Linux builds) run inside the VM. Studio on the Mac talks to a guest
   service over vsock. Account credentials are copied into the VM and refreshed.
5. Studio and the lead get results with `git fetch` from the agent path inside the VM, over the
   guest channel (a `git` remote helper or ssh over vsock).
6. VM CPU, RAM and disk limits are Studio settings with safe defaults.

## Selection

- Per spawn: `environment: "linux"` in `orchestration_spawn` agents (default `"host"`).
- Per project: a "Default worker environment" setting (Host or Linux VM).
- Reviewers and the lead stay on the host.

## Components and owners

| Component          | Files                                                                                                                                                | Content                                                                                                                                                                                            |
| ------------------ | ---------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| VM host            | `desktop/native/linux-vm/` (Swift), `desktop/package.mjs`, `desktop/signing.mjs`, `scripts/codex_linux_vm.py`                                        | Helper binary: create, start, stop, status of the VM; disk images; vsock and NAT; image download with checksum; first-boot provisioning; resource limits; Python client                            |
| Guest service      | `vm/guest/` (Python, runs in the VM), provisioning scripts                                                                                           | vsock JSON-RPC: exec with streaming, file and tree sync, workspace engine calls (base, create, archive, remove), provider process supervision (start, stdio relay, stop), credential files, health |
| Studio integration | `scripts/codex_runtime.py`, `scripts/codex_workspace_images.py` callers, provider launch, `scripts/codex_agent_management.py`, settings UI in `web/` | Spawn option and project default, base build into the VM, remote provider processes, credentials sync, result fetch, archive and removal, disk and RAM reporting, docs                             |

## Guest protocol, version 1

The guest listens on vsock port **4050**, only. The host helper relays each client
connection through `~/.local/state/codex-agents/linux-vm/guest.sock` (mode 0600).
`scripts/codex_linux_vm.py` owns `ensure_running()`, `status()`, `stop()`, settings,
and `connect()`. Studio uses that client and this protocol.

Each UTF-8 line contains one JSON object. The maximum line size is 2 MiB.
Requests contain `{"id":"stable-operation-id","method":"health","params":{}}`.
Responses contain `{"id":"...","result":{...}}` or
`{"id":"...","error":{"code":"...","message":"..."}}`.
Events contain `{"id":"...","event":"output","data":{...}}`.
No JSON-RPC notifications or batches are accepted. IDs are nonempty strings,
maximum 128 characters. Each mutating operation uses a new ID. A retry uses the
same ID and identical method and parameters. The guest saves the request hash
and receipt before it responds. Different content with the same ID fails with
`id_conflict`. An interrupted operation with no proven receipt fails with
`outcome_unknown`; the guest never repeats it. Read methods use fresh IDs.

The service runs as `studio` (not root). Its home is `/home/studio`. Its btrfs
workspace store is `/var/lib/codex-studio/workspaces`. Source trees from the host
are under `/var/lib/codex-studio/projects`. Service state is under
`/var/lib/codex-studio/guest`. Paths are absolute guest paths. The service rejects
paths outside these roots and the service user's home. A request never starts a
shell implicitly. `argv` contains executable and arguments. `env` contains string
values; omitted entries use the service environment. `HOME` remains the service
user's home. With `agentId`, commands enter the existing workspace mount namespace.

| Method                | Parameters                                                                                                                    | Result                                                                                                                                                                                                            |
| --------------------- | ----------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `health`              | `{}`                                                                                                                          | `{protocol:1, uid, home, store, projects, state, filesystem, providers, disk:{freeBytes,totalBytes}, memory:{totalBytes,availableBytes}}`                                                                         |
| `exec`                | `{argv:[string], cwd:string, env?:object, stdin?:base64, agentId?:string, timeoutSeconds?:number, outputLimitBytes?:integer}` | Streams `output` and `exit`; returns `{handle, exitCode, reason, lastSeq}`. Default timeout 300 seconds, maximum 3600.                                                                                            |
| `sync.push`           | `{path:string, data:base64, sha256:string}`                                                                                   | `{path, sha256, bytes}`. Convenience import for a new project root. Maximum 1 MiB archive. Use `upload.*` for larger trees and updates.                                                                           |
| `upload.begin`        | `{uploadId:string, root:string, totalBytes:integer, sha256:string, mode?:string, deletePaths?:[string]}`                      | `{uploadId, root, totalBytes, receivedBytes, nextSeq, state, mode}`. Mode: full or delta (default full). Root must be directly under projects. Maximum 256 GiB archive and expanded tree. Disk floors also apply. |
| `upload.chunk`        | `{uploadId:string, seq:integer, data:base64}`                                                                                 | The current upload status. Sequence starts at 0. Maximum 1 MiB decoded data per chunk. Identical repeated chunks succeed; changed chunks fail.                                                                    |
| `upload.commit`       | `{uploadId:string}`                                                                                                           | `{uploadId, root, sha256, bytes, state:"applied"}`. Extraction and apply each have a 1800-second deadline.                                                                                                        |
| `workspace.startBase` | `{root:string, timeoutSeconds?:number}`                                                                                       | `{state, version, error}` after the existing engine finishes. Default and maximum timeout 1800 seconds.                                                                                                           |
| `workspace.create`    | `{root:string, agentId:string, timeoutSeconds?:number}`                                                                       | `{path, mount}`. Default and maximum timeout 1800 seconds.                                                                                                                                                        |
| `workspace.archive`   | `{agentId:string}`                                                                                                            | `{state, freedBytes}`. Timeout 120 seconds. Refuses a live provider in the workspace.                                                                                                                             |
| `workspace.remove`    | `{agentId:string}`                                                                                                            | `{state, freedBytes}`. Timeout 120 seconds. Refuses a live provider in the workspace.                                                                                                                             |
| `workspace.status`    | `{root?:string, agentId?:string}`                                                                                             | `{base?:object, workspaces:[object]}`. Timeout 15 seconds.                                                                                                                                                        |
| `provider.start`      | `{argv:[string], cwd:string, env?:object, agentId?:string, transport?:string, handle?:string, outputLimitBytes?:integer}`     | `{handle, pid, state, lastSeq}`. Transport: stdio or native (default stdio). Native transport also returns `{resumed, initResult, generation, acknowledged, sequence, returnCode}`. Startup timeout 10 seconds.   |
| `provider.rpc`        | `{handle:string, action:string, ...}`                                                                                         | The existing `codex_process_supervisor.py` action result, as specified below. Native transport only. Timeout 10 seconds.                                                                                          |
| `provider.write`      | `{handle:string, data?:base64, close?:boolean}`                                                                               | `{written, closed}`. Timeout 10 seconds. A maximum of 1 MiB per write.                                                                                                                                            |
| `provider.attach`     | `{handle:string, afterSeq?:integer, waitMs?:integer, maxEvents?:integer}`                                                     | Streams saved `output`/`exit` events; returns `{handle, state, nextSeq, hasMore}`. Defaults: 0, 1000 ms, 256 events. Limits: 30000 ms, 1000 events.                                                               |
| `provider.stop`       | `{handle:string}`                                                                                                             | `{handle, state, exitCode, reason, lastSeq}`. TERM, then KILL after 3 seconds. Timeout 10 seconds.                                                                                                                |
| `provider.list`       | `{}`                                                                                                                          | `{providers:[{handle, pid, state, exitCode, reason, lastSeq, agentId}]}`                                                                                                                                          |
| `file.stat`           | `{path:string, agentId?:string}`                                                                                              | `{path, bytes, token, sha256}`. Regular file only. Deadline: 300 seconds.                                                                                                                                         |
| `file.read`           | `{path:string, agentId?:string, token:string, offset?:integer, maxBytes?:integer}`                                            | `{path, bytes, token, offset, nextOffset, data:base64, sha256}`. SHA256 applies to the chunk. Default chunk: 256 KiB, maximum 1 MiB. Deadline: 15 seconds. A changed file fails.                                  |
| `credentials.put`     | `{files:[{path:string, data:base64}]}`                                                                                        | `{files:[{path, bytes}]}`. Relative paths under the service user's home. Atomic file writes, mode 0600. Maximum 16 files, 1 MiB each.                                                                             |

For stdio transport, provider handles are guest-generated UUIDs. Native transport
accepts a stable caller handle, up to 180 characters (for example
`linux-worker:<agent UUID>`). With a fresh outer request ID, `provider.start`
reattaches to the same live child and returns its current initialization cache
and acknowledgement cursor. An uncertain start repeats its saved outer ID.
It must not use a fresh ID. A completed child's handle can start a new native
generation, as in the existing supervisor.

`provider.rpc` supports the maintained native supervisor actions:

- `write`: `operationId`, `nativeId`, `message` (JSON object). The supervisor saves
  the acceptance receipt and remaps the native request ID. A repeated
  `operationId` with identical business content returns the same `remoteId`.
  A new local message ID does not change business content.
- `operationStatus`: `operationId`, using the existing `monitor:` identity rules.
- `responseStatus`: `nativeId` (string or integer), `generation` (integer).
  Returns `{accepted, generation, state}`. Acceptance requires a current-generation
  operation receipt and a live child with the saved PID and process start time.
  Missing, stale, dead, or unproven acceptance returns `accepted:false`.
- `ack`: `sequence`. A repeated acknowledgement at or below the saved cursor
  succeeds. A cursor above the saved sequence fails.
- `next`: `cursor`. Returns the existing `{event, returnCode, backpressure}`.
- `replay`: `cursor`, optional `limit` (default and maximum 128). Returns
  `{events, sequence, acknowledged, backpressure, hasMore}`. Native events retain
  `{sequence, kind, payload, generation}`. The page also fits the line limit.
- `status`, `detach`: no additional parameters, existing supervisor result.
- `info`: current `{handle, pid, sequence, lastSeq, acknowledged, generation,
initResult, state, reason}` without opening a new generation.

The outer `provider.rpc` ID identifies the transport exchange. Native writes use
`operationId` for durable deduplication. Studio can resend the same operation with
a new outer ID after a reconnect. Read RPC methods use fresh outer IDs.
`provider.stop` verifies the native PID, process start time, and launch signature
before it stops the child. Archive and removal refuse live or unproven providers.

Uploads preserve the project root identity. `uploadId` contains up to 128 ASCII
letters, digits, hyphens, or underscores. `sha256` contains 64 lowercase
hexadecimal characters. `upload.begin` with the same identity and content returns
its saved offset and sequence. Chunk bytes become durable before the offset
advances. `upload.commit` checks the checksum and tar paths before any source
change. A full upload runs rsync with deletion. Rsync uses checksums to detect equal-size, equal-timestamp file changes. A delta upload removes only
`deletePaths`, then runs rsync without deletion. Deletion paths are relative to
the root, cannot contain `..`, and cannot traverse a symlink outside the root.
Regular files, directories, and relative symlinks are supported. Every symlink
chain must remain inside the tree. Devices, hard links, absolute paths, and
traversal fail. Set-ID bits are removed. Uploads and workspace operations lock the
same root. A repeated completed commit returns the saved receipt without
reapplying the tree. An interrupted apply returns `outcome_unknown`. Start a new
full upload only after inspecting that state. At most 32 uploads can be pending.

For stdio transport, Each output event has
`{handle, seq, stream:"stdout"|"stderr", data:base64}`. The final `exit` event has
`{handle, seq, exitCode, reason}`. Sequence numbers start at 1 and increase across
both output streams. Bytes preserve newlines and partial UTF-8 characters.
`provider.attach` sends only events with `seq > afterSeq`. Save each consumed
sequence number, then reconnect and attach with that number. Duplicate delivery
is possible after a lost client acknowledgement. Provider input is deduplicated
by request ID. A disconnected client does not stop a command or provider.

Detached guest supervisors own stdio provider stdin and drain stdout/stderr into
private SQLite journals. A Studio restart, relay disconnect, or guest service
restart preserves the supervisor and journals. On reconnect, call `provider.list`
and `provider.attach`. A VM reboot ends its processes; saved exit or lost-state
records remain visible. Stdio journals have a 64 MiB output limit per process. Native providers use the
existing supervisor journal limit and acknowledgement backpressure.
`outputLimitBytes` can reduce the stdio limit, from 1 byte to 64 MiB. Exceeding
that limit stops the process with `reason:"output_limit"`. Completed journals and
request receipts remain until an operator removes the guest state. `exec` uses
the same supervisor and journal; retrying its ID replays its saved events.

Errors use `invalid_request`, `invalid_params`, `not_found`, `id_conflict`,
`outcome_unknown`, `timeout`, `busy`, `output_limit`, or `internal`.
Clients must report `outcome_unknown` and inspect saved state. They must not retry
the operation with a new ID. Socket input and output have bounded deadlines.
The guest accepts at most 32 connections and 16 concurrent operations.
Resource locks have a 30-second deadline. Internal helper deadlines remain
active after the service exits. Bundle transfer uses `file.stat` and `file.read`,
then verifies the whole file checksum on the Mac. It does not use stdout journals. Slow readers receive
no unbounded output queue. The systemd unit preserves supervisors when the guest
service restarts. Credentials are never returned in receipts or error text.

## Production requirements

- Restart safety: a Studio backend restart does not stop the VM or its agents; the guest
  supervises provider processes and Studio reattaches.
- Disk guard: same floors as image workspaces; VM disk is sparse and has a maximum size.
- Security: credentials only in the VM user home with mode 0600; the guest listens only on vsock.
- Every step has a bounded timeout and a clear error; failure falls back to host workspaces.
- Tests: unit and contract tests for each component, a real end-to-end test on this Mac (VM
  boots, agent runs, commits, lead fetches), restart and failure tests.

## Studio integration

Studio keeps the lead and reviewers on the host. An implementer can use
`environment: "linux"` in `orchestration_spawn`. The project account settings
contain a default worker environment. The nearest project default applies when
the spawn request omits `environment`. An explicit `host` choice overrides it.

A Linux worker waits for its base before the first provider turn. The original
assignment stays in the Studio input queue. If the VM or source copy fails before
provider input, Studio records the error and uses the existing host workspace
fallback. A lost VM after provider input interrupts the worker with an unknown
turn result. Studio does not replay that input or create a replacement VM.

`scripts/codex_linux_workspaces.py` uses `codex_linux_vm.connect()`. The host
helper owns the VM and socket. Studio has no separate socket or VM process.
Source archives use the existing Git HEAD and status detector. The first upload
contains the source tree, Git metadata, the index, and uncommitted files. Later
uploads contain changes and explicit deletions. Each source has a stable guest
root. Pending uploads keep their archive, checksum, chunk sequence, and request
IDs outside the checkout. A retry uses the same content and identities.

The guest runs the maintained native supervisor. Studio uses
`provider.start` with `transport: "native"` and the stable caller handle
`linux-worker:<agent UUID>`. `GuestProcessProxy` forwards `provider.rpc` through
the host client. The existing AppServer handles request ID remap, initialization,
operation receipts, event ACK, and native thread state. Replay starts after the
last acknowledged event. The guest returns at most 128 replay events per call.
A new connection gets the current open receipt. A lost start response keeps its
original request ID. The host account key stays unchanged. Each Linux worker has
its own connection ID and native provider. Its disconnect affects that worker.

Codex and Claude credentials use private guest profiles for the selected host
account. Studio copies Codex `auth.json` and Claude `.credentials.json` (or the
macOS Keychain credential). It verifies Claude account identity before
and after the read. Token values stay out of launch arguments and receipts.
Studio compares credential hashes and checks for host token changes every
30 seconds while Linux providers exist. Account transfers require the Linux
workers to be archived first.

Custom Claude Keychain profiles use `Claude Code-credentials-` plus the first
eight SHA256 hex characters of the selected `CLAUDE_CONFIG_DIR` string, without
a trailing slash. The default profile uses `Claude Code-credentials`.
This rule comes from the lead's measurement on this Mac with Claude Code
2.1.291, not an Anthropic protocol guarantee. A missing service gives a clear
credential read error. A unit test retains the measured service name.

Worker results contain the guest path and a host fetch command. Commit the result
on a named branch. Run the fetch command from the host checkout:

```sh
python3 scripts/codex_linux_vm_fetch.py AGENT_ID GUEST_PATH BRANCH --cwd HOST_REPO
```

The command creates a Git bundle in the guest Git directory. It reads bounded
file chunks and checks the file token, chunk checksums, and bundle checksum.
It verifies the bundle and fetches the branch into host `FETCH_HEAD`. It does
not check out or merge files. The result includes the guest export path. That
file remains available for inspection or an interrupted transfer retry.
Use `--request-id` to retain the export identity after a lost response.

Archive uses the existing Studio blocker checks. It closes the verified idle
native provider, then archives the guest snapshot. Restore keeps that snapshot
and returns the worker paused. Removal deletes the guest workspace after its
provider closes. Studio shutdown detaches providers and keeps their guest
processes alive.

Native tool catalog replacement stays deferred while the maintained supervisor
owns the provider. Linux threads never enter host account rollout maintenance.

`maintenance_report` includes Linux VM disk allocation, disk free space, and
memory totals when the VM responds. Limits appear in Studio Settings, Linux VM.
The host helper validates processor, memory, and disk limits. The VM must be
stopped before a limit change. Saved disks cannot shrink. This settings page does
not stop active workers or provision the VM.

Contract tests use a fake guest and the maintained native supervisor. They test
Studio spawn, the base wait, native process reuse, host connection isolation,
credential changes, upload response loss, and a real Git bundle fetch. Real VM
checks require the host helper and guest service from their component branches.
`tests/linux-vm-studio-native.py` uses an already provisioned, isolated VM. It
checks the Studio caller, source deltas, a native Codex model commit, native
process reuse after a Runtime restart, result fetch, archive, restore, removal,
and resource reports. Use `--claude` to check a native Claude model turn with
the signed-in host profile. The test copies credentials and preserves host files.

## Host interface

The guest listens on vsock port 4050. The helper relays each guest connection through
`~/.local/state/codex-agents/linux-vm/guest.sock`. The socket has mode `0600`.
The helper accepts connections from the same macOS user only.

`scripts/codex_linux_vm.py` provides these functions:

- `ensure_running(settings=None, timeout=1500)` creates the VM when needed and waits for guest health.
- `status()` returns the VM state, process ID, resource settings, allocated disk bytes, and free host bytes.
- `get_settings()` returns the resource limits. `set_settings(values)` saves limits when the VM is stopped.
- `stop(timeout=40)` requests guest shutdown. The helper forces VM shutdown after 20 seconds.
- `connect()` returns a guest client. `request(method, params, request_id=None, timeout=60)` returns a result.
- `stream(method, params, request_id=None, timeout=60)` yields guest event and result frames.

Each guest request uses one connection. A subsequent request reconnects automatically.
The client does not retry a mutation after a lost response. `LinuxVMError.uncertain`
identifies failures where the guest can have applied the request.
`ensure_running()` also waits for the guest clock to synchronize before providers use TLS (Transport Layer Security).

Resource settings use `cpus`, `memoryBytes`, `systemDiskBytes`, and `dataDiskBytes`.
Defaults: 4 CPUs, 4 GiB RAM, 16 GiB system disk, 128 GiB data disk.
The CPU default decreases when the host has fewer CPUs. Disk files are sparse.
Resource changes require a stopped VM. Existing disk limits cannot decrease.
The host uses the same 20 GiB creation floor and 5 GiB start floor as image workspaces.
The existing `CODEX_WORKSPACE_MIN_FREE_BYTES` and `CODEX_WORKSPACE_AGENT_MIN_FREE_BYTES`
variables control those floors.

The host downloads the Ubuntu 24.04 arm64 raw disk archive from release `20260926`.
It checks the pinned SHA-256 checksum before extraction. It does not require QEMU.
The helper uses `VZLinuxBootLoader` with the matching checksum-verified Ubuntu kernel
and initrd. It expands the kernel's gzip Image payload before boot.
The kernel command line uses `root=/dev/vda rw console=hvc0`.
Kernel updates use the pinned Studio image release. Apt does not select the boot kernel.
The first boot uses cloud-init to install tools, pinned host CLI versions, and the guest service.
Each npm download has bounded fetch retries. Each command timeout also forces termination.
A systemd unit retries an incomplete provision on the next VM boot. It skips a complete provision.
The guest component supplies `vm/guest/install.sh`. The host copies that component into
`/opt/codex-studio/vm/guest` in the cloud-init seed. No credentials enter the seed.

To build and sign the helper from a checkout:

```sh
node desktop/native/linux-vm/build.mjs /tmp/studio-linux-vm
CODEX_LINUX_VM_HELPER=/tmp/studio-linux-vm python3 scripts/codex_linux_vm.py start
```

`desktop/package.mjs` includes the helper and guest payload. The helper has the
`com.apple.security.virtualization` entitlement. A backend exit does not stop it.
A separate helper lease prevents two VMs from opening the same disks.
The helper keeps the latest 1 MiB of guest console output for boot diagnostics.
The helper preserves the MAC (media access control) address across VM boots.
Cloud-init uses that address to select the network device.

The Claude bridge uses `/opt/codex-studio/claude_bridge/bridge.mjs`.
Provisioning installs its production dependencies from the maintained package lock.
The guest install script resizes the btrfs data filesystem on each boot.

Run the native host proof from the checkout:

```sh
python3 -B desktop/native/linux-vm/test.py
```

The proof creates isolated temporary disks. It checks boot, CLI versions, a btrfs
workspace, a guest commit, host fetch, provider reconnect, and native Codex initialization.
It removes the VM after the check. It uses no account credentials or model requests.
Use `--provision-restart-check` to inject one provision failure and check recovery after a VM reboot.
