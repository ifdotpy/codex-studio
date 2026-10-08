# Windows server and worker environments

Status: design for phase 1, 2026-10-08. This document defines a Windows server
that runs workers natively or in Windows Subsystem for Linux 2 (WSL2). The
Windows machine is `kukuka-win`. Phase 1 does not access it. Phase 2 requires
separate approval and SSH access.

## Decisions

1. The Windows server runs without the desktop application. A paired Studio UI
   connects to its loopback API through Tailscale Serve and the existing pairing
   protocol in [multiple Studio servers](multi-server.md).
2. `environment: "host"` selects native Windows on a Windows server. A new
   `environment: "wsl"` selects a WSL2 distribution on that server. The
   existing `host` default stays unchanged. Existing `linux` remains the Linux
   VM choice on macOS.
3. Native Windows workers use Git worktrees. Image workspaces remain unsupported
   on Windows. Linux VM workspaces remain unsupported on Windows.
4. WSL2 workers use Linux builds of Codex and Claude Code. They use the existing
   Linux workspace engine and the guest-service design where it fits. The
   Windows server owns access-token refresh. It sends only short-lived access
   credentials to WSL2, as it does for Linux VM workers.
5. The server, its data, and service profiles use a stable per-install state
   identity. A code update must not move or recreate server state.
6. Windows process, file, and IPC ownership must use Windows security
   primitives. Do not emulate POSIX permissions with broad access grants.

## Server shape

Install two Windows services under one dedicated, non-administrator Windows
account:

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
workers. `codex_workspace_linux.py` and its `fcntl`, overlay, `chmod`, and
`setsid` calls run inside the WSL2 guest only. `codex_linux_vm.py` and the
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

## WSL2 worker path

Add `wsl` to the validated environment choices in `orchestration_spawn` and
project worker defaults. Require an installed, healthy WSL2 distribution with
Codex/Claude Linux binaries and Git. Pin the distribution name in server
settings. Run guest commands through `wsl.exe --distribution <name> --user
<guest-user> --exec ...`; do not use an implicit login shell.

Keep each WSL workspace in the distribution's Linux filesystem, not under
`/mnt/c`. Map the Windows project identity to a guest project root in a saved
manifest. Use `wslpath` only at the explicit Windows-to-guest boundary. Give the
Linux guest a Linux path for its working directory and report that path in the
worker record. Reuse `codex_workspace_linux.py` for Linux workspace isolation
and a small guest service for workspace and provider process control. Do not
pass Windows paths into the Linux provider process.

Use a guest service user with an owner-only home. The host obtains the selected
account's current access token and validates its account identity. It passes
only allowlisted access-token fields to the guest. Never copy the host's full
Codex or Claude profile, refresh token, browser store, or Windows credentials.
The host performs refresh, checks the account again, and sends a replacement
access token over authenticated local IPC. The guest stores each token with
mode `0600`, scopes it to that worker/account, and removes it at worker cleanup.
For Claude, confirm that the Linux CLI can use the token-only flow before
enabling the environment. If it needs a refresh token or full profile, fail the
spawn with a clear unsupported-auth error.

Expose WSL Git results through an explicit Git remote helper, such as
`git-remote-wsl`, that invokes the selected distribution's `git-upload-pack`
with a fixed repository path. The lead then uses ordinary `git fetch` for the
worker branch. Restrict the helper to registered repository roots and refs;
reject arbitrary guest paths and command arguments. Keep fetch read-only. Do
not copy a checkout over the Windows project folder.

WSL2 is not a Windows service dependency that can be assumed ready at boot.
The server checks the configured distribution with a bounded health request,
starts it when needed, and reports offline or setup-required status. It never
falls back from `wsl` to `host` after a failed start. A restart or lost response
uses the same workspace and operation identities. Unknown mutations stay
unknown and are not replayed.

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
- The Apple Virtualization Framework Linux VM. WSL2 is the Linux worker option.
- macOS Keychain, launchd, and Linux overlay mounts in the native host process.
  Keep them behind their platform adapters.
- A WSL Claude worker until access-token-only authentication passes the guest
  integration test.

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
| WSL2 selection, guest service, workspace mapping, token-only auth, Git remote helper                | 2 to 3 weeks | WSL worker runs in Linux paths, refreshes through the host, commits, and the lead fetches its branch.  |
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
6. Run the server service at boot without an interactive logon. Restart the
   backend and both services. Verify state ID, API port, worktree, supervisor
   journal, and request receipts stay stable. Verify another backend cannot
   claim an occupied state directory.
7. On `kukuka-win`, configure Tailscale Serve, pair a separate UI, check API and
   stream access, revoke the pair, and verify access fails. Check
   `tailscale serve status --json`. Do not enable Funnel or change any other
   live server.
8. Install one WSL2 distribution under the service account. Test cold start,
   offline distribution, restart, token refresh, token revocation, guest file
   mode, cross-account rejection, Unicode and space-containing paths, worker
   commit, and lead-side `git fetch` through the helper.

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
