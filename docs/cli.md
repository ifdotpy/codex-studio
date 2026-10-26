# Studio command guide

Studio installs Rust commands for the local server, process supervisor, and
diagnostics. Managed agent work uses the application's orchestration tools.
Keep runtime state outside the checkout and preserve its existing identity.

## Deliver an update

Use `just deliver-server COMMIT` from a checkout at the requested commit.
Run `just test-fast` and `just test-comprehensive` on that exact commit first.
Linux uses the source release and systemd owner described in [Linux setup](linux-server-setup.md).

On macOS, the command builds and checks a signed native package. It verifies the
existing backend, state directory, recovery configuration, and app signature.
It runs the upgrade canary before it submits one staged update to the existing
recovery owner. It does not stop agents, monitors, or the process supervisor.
Completion requires the matching install receipt, bundle, backend, and retained
provider process identifiers (PIDs) and generations. A timeout leaves the result unknown.

For an existing package, use:

```sh
just deliver-server COMMIT --app '/absolute/package/Codex Studio.app' --dry-run
just deliver-server COMMIT --app '/absolute/package/Codex Studio.app'
```

`--dry-run` checks the target without a package build, canary, or state write.
Use `--skip-canary` only with explicit approval to install a prepared `--app`
without the canary. This option records a skip, not a passing verdict. It keeps
the target, signature, receipt, backend, and retained-process checks. For this
explicit prepared-package path, the clean operator checkout may have a different
commit. The package must still match `COMMIT`. The skip record identifies both
commits and the exact artifact. Do not combine it with `--dry-run` or the retained
Claude admission option.
`--url` selects another loopback origin. `--receipts` selects the local receipt
directory. The default receipt directory is `~/.cache/codex-studio/deliveries/COMMIT`.
Retry with the same commit, package, and receipt directory after a lost response.
A saved submission never starts another install. A changed package is refused.
The command keeps partial copies and uncertain receipts for inspection.
The state directory keeps a separate journal for each request in `local-deliveries`.
Loss of the receipt cache does not permit another install request.

### A backend that cannot finish startup

On macOS, a backend can own its listener and state lease before HTTP becomes available.
Use the explicit startup option only with a prepared package and the existing canonical state path:

```sh
just deliver-server COMMIT --app '/absolute/package/Codex Studio.app' \
  --starting-backend-pid PID --state /canonical/state/path --dry-run
just deliver-server COMMIT --app '/absolute/package/Codex Studio.app' \
  --starting-backend-pid PID --state /canonical/state/path
```

The usual signature, canary, and exact-commit checks still apply.
The target package must declare the startup protocol and contain its required recovery resources.
This option records a typed process proof. It does not create an HTTP identity.
The proof binds the kernel birth, executable file identity and hash, launch arguments,
state files, occupied runtime lease, supervisor mode, and retained provider identities.
The process reader returns only the fixed state and supervisor environment fields.
It does not return credentials.

The existing installer owns the bundle swap. Its legacy HTTP check can leave the backend running.
The replacement recovery helper reads the durable startup intent before its HTTP check.
It requires the exact installed receipt and bundle, then verifies the pinned old process again.
Only that backend receives SIGTERM. The supervisor and its provider processes remain outside this stop.
The normal recovery loop starts the replacement after the pinned process exits and releases its lease and listener.
If another existing startup owner starts the exact target first, recovery requires its real HTTP identity and unchanged retained proof.
It then records the old process as ended. It sends no signal to that replacement.
The LaunchAgent uses `KeepAlive` and `AbandonProcessGroup`, so the helper can restart without adopting a new install owner.

A mismatch preserves the journal and blocks the stop and replacement.
A crash after `stop-requested` leaves the signal outcome unknown. Recovery does not repeat the signal.
It can continue when the exact old process has ended and the state is free.
Retry delivery with the same package, commit, state, PID, and receipt directory.
Completion still requires a real HTTP identity for the replacement and unchanged retained provider PIDs and generations.

### An installed delivery whose backend exits before HTTP

Use this separate proof when the installer finished, but its backend cannot serve HTTP:

```sh
just deliver-server COMMIT --app '/absolute/package/Codex Studio.app' \
  --recover-installed-delivery PRIOR_UUID --state /canonical/state/path --dry-run
just deliver-server COMMIT --app '/absolute/package/Codex Studio.app' \
  --recover-installed-delivery PRIOR_UUID --state /canonical/state/path
```

The prior state journal and installed receipt must identify the exact current bundle.
The prior journal can remain `submitted`. This option does not report that delivery as complete.
The new signed package must be newer and declare the installed-delivery recovery capability.
The proof binds fixed launch scope, state file identities, and the original retained supervisor handles.
It stores no backend PID as a signal target and copies no environment or provider launch record.
The prior journal remains unchanged.

Publication uses the existing delivery, installer, maintenance, and runtime leases.
A legacy helper can already have passed its lease probe before publication acquires the runtime lease.
Such a launch attempt must fail its authoritative runtime lease before any database write.
This option does not guarantee zero launch attempts.

The existing installer owns the swap. The new recovery helper sends no signal through this proof.
It waits for every previous-image backend of this canonical state to end.
It preserves backend processes for other states and the exact read-only `state-check` command.
An unknown process observation blocks recovery.
A target process that already exists must supply its exact real HTTP identity or end before another launch.
Only the normal recovery loop starts a replacement after the state becomes free.
Final delivery success requires real HTTP and unchanged retained handles.
Retry the same new package, commit, prior UUID, state, and receipt directory after an uncertain response.

## Install commands

From the repository root, with the pinned Rust toolchain and Node.js available:

```sh
pnpm install --frozen-lockfile
just install
```

Add `~/.local/bin` and the provider CLI directory to `PATH`. To choose another
command directory, use `just install --bin-dir DIRECTORY`. The installer builds
the binaries, preflights all new links, and refuses unrelated executables or
links. It removes and reports only obsolete Python command links belonging to
this checkout. Repeating the same installation is safe. No Python environment
or extension installation is required.

## Commands

| Command                                                                        | Function                                            |
| ------------------------------------------------------------------------------ | --------------------------------------------------- |
| `studio-runtime-server [--port PORT]`                                          | Run the loopback backend and announce its URL       |
| `codex-canvas [--port PORT]`                                                   | Alias of the Rust runtime server                    |
| `studio-runtime-server service install\|status\|uninstall [--state DIRECTORY]` | Manage user-service startup without restarting work |
| `studio-runtime-server state-check DIRECTORY`                                  | Check existing runtime state without changing it    |
| `codex-process-supervisor --state DIRECTORY [--wait-for-lease]`                | Run the independent native child owner              |
| `codex-diagnostics [--port PORT]`                                              | Read the backend's shareable diagnostics snapshot   |

`CODEX_AGENTS_STATE_DIR` selects the runtime state directory. When starting a
server manually, use the existing state path and confirm that no backend already
owns it. `--port` overrides the listening port; `0` requests an ephemeral port.
The default local port is owned by
[`service-layout.json`](../workspaces/runtime/apps/runtime-server/service-layout.json).
Open the URL announced by the server. Keep its terminal alive while using a
manually started instance. Build the web interface before starting it:

```sh
pnpm --filter codex-agents-web run build
codex-canvas
```

## User service commands

```sh
studio-runtime-server service install --state ~/.local/state/codex-agents
studio-runtime-server service status
```

Installation writes and enables the matching user configuration. It does not
start, restart, or stop a service or create runtime state. A changed existing
configuration is an error; resolve it at an idle boundary. Linux services use
the shared release root's `current` binary. Only `just deliver-server` selects
and restarts the live Linux release, with health checks and rollback.
On macOS the command writes and enables a LaunchAgent label; bootstrap it
explicitly at an idle boundary after preparing its release.

`service uninstall --state DIRECTORY` disables startup and removes the matching
base configuration. It leaves any running backend and independent supervisor
alone. Use the same state and launch environment as installation. See
[Linux setup](linux-server-setup.md) and the
[operator contract](../workspaces/runtime/apps/runtime-server/README.md#command-and-user-service-installation).

## Managed teams and recovery

Create a lead conversation in Studio. The lead uses `orchestration_spawn` for
workers, `orchestration_send` for instructions, and `orchestration_monitor` for
commands. The server owns queues, concurrency, receipts, waits, and parent
continuation. Use `orchestration_status` for a decision; completion events arrive
automatically. Workers report evidence through their assigned task.

After losing a tool response, use `orchestration_request` with the original
request identity before retrying. Missing evidence means the outcome remains
unknown. Do not repeat a command or message under a fresh identity to recover a
lost response. `orchestration_read` retrieves saved large outputs.

Use `orchestration_message` for agent coordination and `orchestration_task` for
assignments and review decisions. Read the relevant room before acting on a
coordination question. The lead owns review, integration, and user contact.
See [managed orchestration](orchestration.md) for permissions and recovery.

The retired commands `codex-chat`, `codex-control`, `codex-supervisor`,
`codex-daemon`, `codex-graph`, `codex-models`, `codex-report`, `codex-steer`,
`codex-stop`, `codex-swarm.mjs`, `codex-watch`, and `luna` have no compatibility
aliases. Use managed orchestration tools and the Rust commands above. Existing
state and unrelated local command names remain untouched.

## Focused verification

```sh
cargo nextest run --locked -p studio-runtime-server -E 'test(operator_cli::)'
```

These tests use disposable HOME/XDG directories and fake service managers. They
cover safe link migration, matching service retries, manager failures, shared
delivery paths, and Python independence. They never operate the host service.
