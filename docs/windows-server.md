# Windows server and worker environments

Status: implementation record, 2026-10-08. The Windows machine is
`kukuka-win`. It runs a native Windows Studio server beside a separate Studio
Linux server inside Windows Subsystem for Linux 2 (WSL2).

## Decisions

1. The Windows server runs without the desktop application. A paired Studio UI
   connects to its loopback API through Tailscale Serve and the existing pairing
   protocol in [multiple Studio servers](multi-server.md).
2. The selected Studio server determines the worker environment. The native
   Windows server runs Windows workers. The separate WSL2 Studio server runs
   Linux workers. The existing `host` default stays unchanged. Existing `linux`
   remains the Linux VM choice on macOS.
3. Native Windows workers use Git worktrees. Image workspaces remain unsupported
   on Windows. Linux VM workspaces remain unsupported on Windows.
4. The WSL2 option runs as a separate Studio Linux server inside WSL2. It uses
   the existing Linux server and worker code. Pair it with the native Windows
   server like any other server. The selected server determines the worker OS.
5. The server, its data, and service profiles use a stable per-install state
   identity. A code update must not move or recreate server state.
6. Windows process, file, and IPC ownership must use Windows security
   primitives. Do not emulate POSIX permissions with broad access grants.

## Server shape

The target design uses two Windows services under one dedicated,
non-administrator Windows account:

| Service                 | Role                                                                                                                                                     |
| ----------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `CodexStudioServer`     | Starts the Python API and orchestration backend, binds the API to `127.0.0.1`, records health, and restarts the backend after failure.                   |
| `CodexStudioSupervisor` | Owns native Codex and Claude child processes, their job objects, durable journals, and their local IPC endpoint. It stays up while the backend restarts. |

Use the Windows Service Control Manager (SCM) for boot startup, stop requests,
and bounded recovery after failure. Install both services with the same
dedicated account so they see the same state, Codex profile, Claude profile,
and WSL2 distribution. Grant that account only the service logon right and
access to its configured project folders. Keep the service executable, Python
environment, logs, profiles, and state outside the source checkout.

The service starts Python directly with an absolute interpreter path and a
fixed environment. It does not depend on Electron, a shell startup file, a
logged-in desktop, or the current directory. Use the SCM for orderly shutdown.
Stop accepting requests, stop or detach child handles with their exact saved
identities, flush SQLite, then report stopped. On backend restart, reconnect to
the existing supervisor and recover only children whose process ID, creation
time, launch signature, and job membership match the journal. If ownership is
unclear, preserve the unknown outcome and do not signal or relaunch that child.

An elevated installer registers or removes services and sets state-directory
ACLs. The service itself does not run as administrator. A scheduled task set
to run at boot and without an interactive logon is a fallback only if a tested
service host cannot be packaged. It must use the same account and recovery
rules. A task must not start a second backend when the existing state directory
is occupied.

## kukuka-win deployment

The current deployment runs under the existing `IGOS3` account. A Scheduled
Task starts the server at logon with a limited interactive token. It uses the
Python environment and source checkout under `C:\Users\IGOS3\studio-dev`.
Installer files and server state live under `%LOCALAPPDATA%\CodexStudio`.

The Python entrypoint starts the process supervisor once, then starts the API
on `127.0.0.1:4630`. It restarts the API after a process exit. The supervisor
keeps native workers and reattaches them after each API restart. The task
restarts the entrypoint after a failure. The API and supervisor use the same
`%LOCALAPPDATA%\CodexStudio\state` directory.

Use `manage-windows-server.ps1 -Action StopBackend` to stop only the API with
the Windows console shutdown signal. The entrypoint and supervisor stay up;
supervised workers stay attached. Use `-Action RestartBackend -SourceRoot
`<path>`to stop the API, select a source tree, and start the API again. Use`-Action StopAll` only to stop the server tree and remove its logon task.

The manager supports Windows PowerShell 5.1 and PowerShell 7. The runner writes
`state/windows-server-runner.json` with control protocol 2, its process ID, and
its creation time. The manager verifies this identity before it writes a separate
request file. If this file is absent, the manager uses the old singleton transport.
A stale or invalid identity stops the request.

The old transport permits one request until its receipt confirms the result.
The manager retains that request ID in `windows-server-control-legacy-pending.json`.
If the receipt is absent, inspect the runner and request before any recovery.
Do not submit the action again or move the request to another transport.
An old runner must be replaced separately while it has no active handles.
Stop the backend first. Preserve the existing supervisor during the runner change.

Tailscale Serve keeps the WSL2 Studio server on HTTPS port 443, which proxies
to `127.0.0.1:4720`. The native Windows server uses HTTPS port 8443, which
proxies to `127.0.0.1:4630`. Its public origin is
`https://kukuka-win.tailf00fa0.ts.net:8443`. Discovery probes same-owner
Tailscale nodes on ports 443 and 8443. It records each origin with its port.

The native server discovers Codex and Claude through Windows executable
discovery, including `.cmd` shims. Native implementers use Git worktrees under
the Windows repository. They do not use image workspaces or the WSL2 server's
Linux workspace.

## POSIX dependencies and port plan

The table covers the Python server, its worker and account paths, and the
current native supervisor. Linux-only or macOS-only subsystems stay isolated
and are not imported by the Windows server path.

| Current dependency and locations                                                                                                                                                                                                                                                                                                                                                                      | Windows plan                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                        |
| ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `fcntl.flock`: `codex_runtime.py`, `codex_process_supervisor.py`, `codex_live_updates.py`, `codex_native_binary.py`, `codex_payloads.py`, `codex_progress_layout.py`, `codex_claude_login.py`, `codex_workspace_images.py`, `codex_linux_vm.py`                                                                                                                                                       | Add one small cross-platform file-lock module. Use `LockFileEx` on a byte range for Windows and `flock` on POSIX. Keep exclusive and shared modes, nonblocking behavior, process-exit release, and existing lock-file identity. Keep the image-workspace and Linux-VM code behind platform guards. Use contract tests for contention, crash release, and retry.                                                                                                                                                                                     |
| Unix domain sockets: `studio_api/server.py` optional `canvas.sock`; `codex_process_supervisor.py` client and `ThreadingUnixStreamServer`                                                                                                                                                                                                                                                              | Keep the already-present loopback TCP API on `127.0.0.1`. Do not create the optional canvas socket on Windows. Replace supervisor IPC with a local Windows named pipe. Set an explicit discretionary access control list (DACL) for the service account, deny network clients, validate the client identity, and retain framed messages, bounded reads, stable request IDs, and existing receipts. The default named-pipe DACL is too broad for this protocol.                                                                                      |
| Process groups, sessions, and signals: runtime, process supervisor, model-catalog reader, and native-binary checks use `start_new_session`; supervisor, terminal, worktree, and Claude-login cleanup use `os.killpg` and `SIGTERM`/`SIGKILL`; `codex_canvas.py` maps `SIGTERM` to shutdown                                                                                                            | Create each supervised child with `CreateProcess` or Python `subprocess.Popen`, assign it to a Windows Job Object before it can create descendants, and save its process creation time. Use `TerminateJobObject` only after rechecking the saved identity. Give graceful shutdown a bounded wait, then terminate the verified job. Use service-control events for server shutdown; use `CTRL_BREAK_EVENT` only for compatible console children. Never use `taskkill /IM`, broad PID-only termination, or `os.kill(pid, signal)` as ownership proof. |
| Process identity: supervisor `process_start_time()` has Darwin and Linux implementations and persists PID, process group, and start time                                                                                                                                                                                                                                                              | Add a Windows implementation based on process creation time from `GetProcessTimes`; open processes with minimum rights and treat access denied, PID reuse, missing job membership, or unreadable creation time as unknown. Record job identity and process creation time with the existing durable handle.                                                                                                                                                                                                                                          |
| `os.fork` and `exec`: no `os.fork` call exists in the server path; `codex_terminal_child.py` and `codex-canvas` use `os.execv` to replace a process                                                                                                                                                                                                                                                   | Keep process creation behind `subprocess.Popen`. Replace self-reexec paths with an explicit `Popen` handoff or a Python entrypoint that selects the interpreter before application imports. Windows has no `fork` equivalent needed by this design.                                                                                                                                                                                                                                                                                                 |
| POSIX modes `0600`, `0700`, `0755`, `chmod`, and `fchmod`: `codex_accounts.py`, `codex_runtime.py`, `codex_canvas.py`, `codex_process_supervisor.py`, `codex_claude_login.py`, `codex_context_repair.py`, `codex_multi_server.py`, `codex_progress.py`, `codex_progress_layout.py`, `codex_terminal_child.py`, `codex_voice.py`, trace and cost stores, Claude bridge state, and Electron state files | Create files in directories with an owner-only DACL for the service SID or dedicated account. Use Windows ACL APIs to protect state roots and files. Apply secure ACLs at creation time and verify them after atomic replacement. Preserve atomic temp-file plus replace writes and SQLite WAL. Do not claim that `chmod(0600)` protects Windows files.                                                                                                                                                                                             |
| POSIX path and filesystem assumptions: `~/.local/state`, `~/.local/bin`, XDG variables, `/opt/homebrew/bin`, slash-separated path construction, `dir_fd`, `O_NOFOLLOW`, symlink checks, case-sensitive names, and executable-bit checks in progress, native-input, native-runtime, multi-server, API I/O, and agent-management code                                                                   | Centralize state, cache, executable, and profile path resolution. Default server data to `%LOCALAPPDATA%\CodexStudio\state`, with an explicit `CODEX_AGENTS_STATE_DIR` override; default Codex and Claude profiles to the service account's profile. Use `pathlib`, Windows-aware path comparison, and reparse-point checks. Replace `dir_fd`/`O_NOFOLLOW` operations with handle-based Windows APIs or safe same-directory atomic operations. Keep the state identity stable across upgrades.                                                      |
| Shell and command assumptions: `codex-daemon`, `codex-stop`, `codex-canvas`, `restart-backend-v2.sh`, `bash`, `shlex`, and commands such as `chmod`, `rsync`, `sysctl`, `security`, and `launchctl`                                                                                                                                                                                                   | Add a Windows service entrypoint and PowerShell installer. Use argument arrays and absolute paths, not a shell command string. Select PowerShell only for a user-requested PowerShell monitor. Use Windows equivalents for service install, process control, and file ACLs. Keep `sysctl`, Keychain, launchd, and `chmod` calls inside their current macOS or Linux adapters.                                                                                                                                                                       |
| Python launch and low-level modules: `codex-daemon` assumes `node`, `codex_runtime.py` imports `fcntl`, runtime update discovery expects POSIX Codex layouts, and some features use `select` on subprocess pipes                                                                                                                                                                                      | Make OS-specific imports lazy. The Windows backend must import and start without importing a POSIX-only module. Resolve Python, Node, Codex, Claude, Git, and Tailscale before service registration; save absolute executable paths and versions. Keep child stdin/stdout on pipes and use reader threads or asyncio Proactor support, not `select` on Windows pipes.                                                                                                                                                                               |
| User and process limits: `codex_shell.py` imports POSIX `pwd`; `codex_startup_memory.py` and `codex_open_file_limit.py` import `resource`; `context.py` already guards `os.nice` with `os.name == "posix"`                                                                                                                                                                                            | Replace `pwd` with the configured service account and Windows identity APIs. Make open-file-limit and startup-memory features platform-aware; skip unsupported soft-limit calls or use a measured Windows equivalent. Keep the existing guarded priority adjustment as a no-op on Windows.                                                                                                                                                                                                                                                          |
| Tailscale CLI: `codex_federation.py` calls `shutil.which("tailscale")` for `whoami` and `whois`                                                                                                                                                                                                                                                                                                       | Resolve `tailscale.exe` from an explicit setting, `PATH` (including `PATHEXT`), or the installed Tailscale client path. Pass an argument array and bounded timeout. Report unavailable identity as unavailable; do not bypass the existing Tailscale identity check. Do not assume the service account inherits an interactive user's PATH.                                                                                                                                                                                                         |

The file-mode inventory also includes `codex_account_transfer.py`,
`codex_efficiency.py`, `codex_costs.py`, `codex_http_traces.py`,
`codex_pricing.py`, `codex_session_costs.py`, `codex_sqlite_traces.py`,
`codex_portable_history.py`, `codex_work.py`, and `codex_workspace.py`.
Keep their private writes under the same Windows ACL policy.

`codex_workspace_images.py` uses image and mount helpers and is not a Windows
workspace backend. Select the existing Git worktree fallback for native Windows
workers. Image workspaces require macOS ASIF. Linux servers use Git worktrees.

Apple Virtualization Framework stay unsupported on Windows. Audit the import
graph so a native Windows server does not import these modules at startup.

The named file groups above are the server-path inventory. Other POSIX-only
files belong to platform-specific image, VM, desktop-recovery, benchmark, or
build paths. Keep those paths outside the Windows service import graph.

## Native Windows worker path

Use the existing per-spawn environment field. Resolve `host` to the Windows
provider runner and workspace adapter. Use the existing worktree creation and
result delivery flow with Windows Git. Keep each worker in a separate Git
worktree, branch, and directory. Preserve the base commit, exact branch name,
recovery checks, and the lead's current fetch workflow.

Discover Codex and Claude as native Windows commands. Search configured
absolute paths first, then `PATH` using `PATHEXT` and known per-user install
locations. Support tested `.exe` files and npm `.cmd` shims. Launch `.cmd` files
through a narrowly constructed `cmd.exe /d /s /c` call with tested argument
quoting; do not concatenate untrusted model or project text into a shell
command. Resolve Claude's configured Git Bash path when its Windows CLI
requires Git Bash. Validate the selected Codex executable and its companion
files with the existing version and protocol checks. Persist the selected paths
so a service restart does not silently choose a different binary.

Git remains the worktree and fetch authority. Require a supported Windows Git
for Windows installation. Use `git.exe` with argv, `-C`, bounded timeouts, and
captured output. Test paths with spaces, non-ASCII characters, long paths,
symlinks, line endings, interrupted checkouts, and dirty user files. Enable
long-path support in the installer when policy allows it; otherwise fail with
a direct path-length error. Never remove or reset a worktree that fails its
saved branch, commit, index, or path identity checks.

## Separate WSL2 Studio server

Run the WSL2 option as a separate Studio Linux server inside WSL2. Use the
existing Linux server and worker code. Do not add a `wsl` worker environment to
the native Windows server.

Pair the WSL2 server with the native Windows server through the existing
multi-server discovery and pairing flow. Select the server to select its worker
environment. The WSL2 server runs Linux workers. The native Windows server
runs Windows workers. Keep their state directories, ports, process supervisors,
and worker profiles separate.

On `kukuka-win`, keep the WSL2 server on loopback port 4720 and its Tailscale
Serve mapping on HTTPS port 443. Expose the native Windows server on loopback
port 4630 and HTTPS port 8443. Discovery must distinguish the two origins by
port, even when both servers use the same Tailscale host name.

## Tailscale Serve and access

Bind the API to loopback only. Keep the existing paired-server authentication
and request signing. Install and run the Tailscale Windows client as its own
service. Configure Tailscale Serve to proxy HTTPS to the API's loopback port
with background persistence, then verify it with `tailscale serve status
--json`. Serve resumes after reboot when configured in background mode. Never
bind the API to `0.0.0.0`, use Funnel, or trust a caller-supplied identity
header. Allow the service to start while Tailscale is offline; pairing and
remote access remain unavailable until Tailscale identity and Serve health are
verified.

Tailscale's Windows Serve examples require an Administrator terminal for the
initial configuration. Keep that setup in the elevated installer or operator
steps. Do not run the Python server as administrator.

## Work not supported on Windows

- Image workspaces. Native Windows workers use Git worktrees.
- The Apple Virtualization Framework Linux VM. Use the separate WSL2 Studio
  server for Linux workers on `kukuka-win`.
- macOS Keychain and launchd in the native host process.
  Keep them behind their platform adapters.

## Effort estimate

Estimate: **9 to 14 engineer-weeks**, excluding review queue time and hardware
purchase. One engineer-week is five working days. This is a planning estimate;
the Windows provider and WSL token paths are the largest unknowns.

| Part                                                                                                |     Estimate | Exit condition                                                                                         |
| --------------------------------------------------------------------------------------------------- | -----------: | ------------------------------------------------------------------------------------------------------ |
| Windows platform layer: locks, path rules, ACLs, optional socket, subprocess I/O, OS identity       | 2 to 3 weeks | Core API imports on Windows; contention, ACL, reparse-point, and crash-release contracts pass.         |
| Process supervisor, named-pipe security, Job Objects, durable recovery, backend reattach            | 2 to 3 weeks | Restart tests prove child ownership and exact-once request behavior; uncertain identity fails closed.  |
| Windows server install, service account, SCM recovery, state/log paths, operator update and removal | 1 to 2 weeks | Server starts at boot without desktop logon and recovers after backend failure without moving state.   |
| Native Windows provider discovery, Codex/Claude launch, Git worktree and fetch                      | 1 to 2 weeks | Both providers run a worker that commits in its worktree and the lead fetches its branch.              |
| Separate WSL2 Studio server pairing and same-host discovery                                         | 2 to 3 weeks | Both servers pair on the same host, keep separate state, and expose their native worker environments.  |
| Windows CI, end-to-end recovery and Tailscale pairing checks on `kukuka-win`                        |       1 week | Required native Windows checks pass on the target; no live model request is needed for fixture checks. |

Some work can overlap after the Windows platform contracts are stable. The
estimate assumes a supported Windows 11 x64 host, Python 3.11+, Node.js 22.15+,
Git for Windows, Tailscale, WSL2, and one configured Linux distribution. Confirm
the target's exact versions before implementation.

## Test plan for phase 2

1. Run import and unit contracts on Windows. Verify the API, runtime, account
   store, server launcher, and worker selector load without POSIX-only imports.
2. Test file-lock exclusion across processes, release after process exit, shared
   reads, and nonblocking contention. Test state ACLs, atomic replacement,
   reparse-point rejection, long paths, Unicode paths, and paths with spaces.
3. Test the named pipe from the service account and a different local account.
   Verify the second account is denied. Test truncated frames, request replay,
   request ID conflicts, slow clients, and bounded shutdown.
4. Start a fixture provider that spawns descendants. Kill only the backend,
   restart it, and verify the supervisor retains and reattaches to the same
   worker. Reuse a PID in a fixture and prove the supervisor does not signal
   the new process. Verify graceful stop, timeout escalation, and crash recovery.
5. Run a native Codex worker and a Claude worker with fixture credentials. Check
   model selection, permission checks, logs, file changes, commit, worktree
   cleanup, and lead-side `git fetch`. Do not require a paid model call.
6. Run the per-user scheduled task at logon. Restart the backend and task.
   Verify state ID, API port, worktree, supervisor journal, and request receipts
   stay stable. Verify another backend cannot claim an occupied state directory.
7. On `kukuka-win`, configure Tailscale Serve, pair a separate UI, check API and
   stream access, revoke the pair, and verify access fails. Check
   `tailscale serve status --json`. Do not enable Funnel or change any other
   live server.
8. Start the separate WSL2 Studio server. Pair it with the native Windows
   server on the same Tailscale host. Verify discovery keeps ports 443 and 8443
   distinct, and verify each server starts workers in its own operating system.

Run target-host checks over SSH only after phase 1 review and approval. Do not
start the new server against an occupied state directory.

## References

- [Tailscale Serve command](https://tailscale.com/docs/reference/tailscale-cli/serve)
- [Tailscale Serve examples, including Windows](https://tailscale.com/docs/reference/examples/serve)
- [Microsoft Service Control Manager](https://learn.microsoft.com/en-us/windows/desktop/services/service-control-manager)
- [Windows named-pipe security](https://learn.microsoft.com/en-us/windows/win32/ipc/named-pipe-security-and-access-rights)
- [Windows `LockFileEx`](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-lockfileex)
- [Windows Job Objects](https://learn.microsoft.com/en-us/windows/win32/procthread/job-objects)
- [Claude Code setup, Windows and WSL](https://docs.anthropic.com/en/docs/claude-code/getting-started)
