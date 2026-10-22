# Studio command guide

Studio installs Rust commands for the local server, process supervisor, and
diagnostics. Managed agent work uses the application's orchestration tools.
Keep runtime state outside the checkout and preserve its existing identity.

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
