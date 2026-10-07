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

## Guest protocol (vsock, one JSON object per line)

Request `{"id", "method", "params"}`, response `{"id", "result"}` or `{"id", "error"}`, stream
events `{"id", "event", "data"}`. Methods (initial set; owners may extend with lead approval):
`health`, `exec` (argv, cwd, env, stdin, streaming), `sync.push` (tar stream into a path),
`workspace.startBase`, `workspace.create`, `workspace.archive`, `workspace.remove`,
`workspace.status`, `provider.start` (argv, env, cwd; returns a handle), `provider.write`,
`provider.stop`, `provider.list`, `credentials.put`.

## Production requirements

- Restart safety: a Studio backend restart does not stop the VM or its agents; the guest
  supervises provider processes and Studio reattaches.
- Disk guard: same floors as image workspaces; VM disk is sparse and has a maximum size.
- Security: credentials only in the VM user home with mode 0600; the guest listens only on vsock.
- Every step has a bounded timeout and a clear error; failure falls back to host workspaces.
- Tests: unit and contract tests for each component, a real end-to-end test on this Mac (VM
  boots, agent runs, commits, lead fetches), restart and failure tests.

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
