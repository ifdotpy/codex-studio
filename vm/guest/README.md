# Linux guest service

The service runs as `studio`. It listens on vsock port 4050. The protocol is in
[linux-vm-workspaces.md](../../docs/linux-vm-workspaces.md).

Copy this folder to `/opt/codex-studio/vm/guest`. Copy these runtime files to
`/opt/codex-studio/scripts`:

- `codex_workspace_images.py`
- `codex_workspace_linux.py`
- `codex_process_supervisor.py`
- `codex_open_file_limit.py`

Install Python 3.11 or later, rsync 3 or later, btrfs-progs, git, util-linux, procps,
and lsof.
Mount the btrfs data disk at `/var/lib/codex-studio`. Use the
`user_subvol_rm_allowed` mount option. Run `install.sh` as root.

The installer creates the user and systemd units. The unit expands the data
filesystem at each boot. The service reports readiness after it binds its socket.
A service restart preserves detached provider owners and the workspace namespace.
A VM reboot ends those processes. A lost process remains visible as `lost`.

The installer permits unprivileged user namespaces in this dedicated Ubuntu VM.
It sets runtime files to root ownership. Credential files stay in the service
user's home with mode 0600. State, source trees, and workspace layers stay on the
data disk. The installer does not install provider binaries or credentials.

Run the contract tests:

```sh
python3 -B -m unittest discover -s vm/guest -p test_service.py -v
```

The Linux test requires an isolated installed service and a private Unix socket.
For the test machine only, add this systemd override:

```ini
[Service]
ExecStart=
ExecStart=/usr/bin/python3 /opt/codex-studio/vm/guest/service.py --unix-socket /var/lib/codex-studio/guest/test.sock
```

Restart that test service. Run the test as `studio` with permission to restart
that service through sudo:

```sh
python3 vm/guest/test_linux_integration.py --socket /var/lib/codex-studio/guest/test.sock
```

The test checks a btrfs base, a private workspace, a Git commit, and bundle fetch.
It also checks raw and native provider processes across a real systemd restart.
An exec command runs for 65 seconds and returns its result through the same
connection. A retry on a new connection retrieves its saved result. The test also
checks recovery of a dead native supervisor with its lease and socket on disk.
The native check uses a local JSON provider fixture. It sends no model request.
Remove the test machine after the test. Production uses vsock only.

Request receipts and completed journals remain on disk. An operator can remove
old guest state after all related work is complete. Do not remove an active
provider's journal or an upload with an unproven apply result.
