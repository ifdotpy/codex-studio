# Linux and WSL server setup

Install the pinned Rust toolchain, Node.js 22.15 or later, Git, pnpm, and the
provider CLI. Sign in to the provider as the server user. Keep source and state
on the Linux filesystem. WSL uses the same user-service setup.

From the checkout:

```sh
pnpm install --frozen-lockfile
just install
studio-runtime-server service install --state ~/.local/state/codex-agents
```

Add `~/.local/bin` and the provider CLI directory to `PATH` before installing
the service. The installer writes and enables `codex-studio.service` in
`$XDG_CONFIG_HOME/systemd/user` (default `~/.config/systemd/user`). It carries
`PATH`, `CODEX_CANVAS_PUBLIC_ORIGIN`, `CODEX_BIN`, and `CODEX_NODE` from
the shell and preserves the selected state identity. It never starts, restarts,
or stops a service. Repeating the same installation is safe; a differing
existing unit is an error that requires an operator to resolve at an idle boundary.

The base unit and delivery script share
[the service layout](../workspaces/runtime/apps/runtime-server/service-layout.json).
They use the same default local server port and release root. Set
`CODEX_STUDIO_SERVER_ROOT` before installation and delivery to use a different
release root. The base unit starts the binary under its `current` release.

Only `just deliver-server` changes this host's live Linux server release.
It builds a commit snapshot, selects it through `current`, restarts the backend,
checks health, and rolls back an unhealthy release. Keep the state directory
identity and never start a second backend against it. Do not interrupt active
agents, commands, monitors, or terminals for a source update.

Delivery requires systemd 254 or newer and a working systemd user manager.
The minimum version in [deliver.sh](../workspaces/runtime/apps/runtime-server/deliver.sh)
ensures literal command arguments are supported; older versions fail before
a delivery starts. The entire
build, restart, health check, rollback, and pruning run in a separate transient
user service, outside the backend's cgroup. The caller prints its unit name and
a `journalctl --user --unit=... --no-pager` command before handing off. Output
goes directly to the unit journal; a surviving caller receives the output replay
at completion and the delivery's exit status. If the calling agent or monitor
is killed by the restart, use the printed journal command to read progress and
the final exit code. Collection of the transient unit leaves its journal intact.
Each unit name includes a random UUID, and the worker records the manager's
`INVOCATION_ID` under the release root's `delivery-invocations/` directory before
building. A surviving caller filters its journal replay by
`_SYSTEMD_INVOCATION_ID`, so a previous invocation's output cannot stand in for
the current result. These small identity records also remain available after a
caller disconnect.
Check that outcome before retrying; a lost caller result does not mean delivery
failed. Concurrent deliveries using the same release root are refused by
`deliver.lock` inside the unit.

The launcher passes the invoking shell's toolchain, path, proxy, XDG, and release
root settings listed in [deliver.sh](../workspaces/runtime/apps/runtime-server/deliver.sh).
It fails if `systemd-run` is missing instead of delivering in the caller's process.
`CODEX_STUDIO_DELIVER_IN_UNIT` is the internal recursion guard. The worker
accepts it only with a valid manager `INVOCATION_ID` and membership in the
expected delivery unit in `/proc/self/cgroup`; an inherited guard outside that
unit fails clearly. The explicit `CODEX_STUDIO_DELIVER_TEST_IN_PROCESS=1`
bypass also requires the fixture-only `.codex-studio-delivery-test` marker beside
a disposable copy of the script. Production checkouts never contain that marker.
Tests use mocked service commands; never set the bypass or create the marker for
a real delivery.

At an authorized maintenance boundary, deliver the selected commit:

```sh
just deliver-server HEAD
studio-runtime-server service status
```

For startup after logout, enable user linger once:

```sh
sudo loginctl enable-linger "$USER"
```

`studio-runtime-server service uninstall --state DIRECTORY` disables startup
and removes the matching base unit. It leaves any running backend and release
directories alone. Keep the same state and launch environment when removing a
configuration. The desktop's recovery helpers and any separately managed
process supervisor have their own lifetimes; this CLI does not stop them.

## WSL networking and Tailscale

Enable systemd in `/etc/wsl.conf` inside Ubuntu:

```ini
[boot]
systemd=true
```

Set mirrored networking in `%UserProfile%\.wslconfig` on Windows:

```ini
[wsl2]
networkingMode=mirrored
```

Apply these settings with `wsl --shutdown`, then start Ubuntu. Do this before
Studio starts. WSL shutdown stops the distribution and all its work.
Allow the Studio port through the Windows and Hyper-V firewall for the intended
Tailscale peers. Test access from the paired server before pairing Studio.

When Tailscale is installed on Windows, its CLI can run through WSL interop.
systemd user services do not inherit `WSL_INTEROP`. Install this wrapper as
`~/.local/bin/tailscale` and make it executable. It selects a live interop socket
before it starts the Windows CLI:

```sh
#!/bin/sh
exe='/mnt/c/Program Files/Tailscale/tailscale.exe'
if [ -z "$WSL_INTEROP" ]; then
  for socket in $(ls -t /run/WSL/*_interop 2>/dev/null); do
    [ -S "$socket" ] || continue
    if WSL_INTEROP="$socket" "$exe" version </dev/null >/dev/null 2>&1; then
      WSL_INTEROP="$socket"
      break
    fi
  done
  export WSL_INTEROP
fi
exec "$exe" "$@"
```

Run `chmod 700 ~/.local/bin/tailscale`. Keep `~/.local/bin` before Windows
directories in the service `PATH`. Leave a WSL session available for interop.
If no live socket exists, the wrapper fails through the Windows CLI. Start an
Ubuntu session and retry the read-only Tailscale check.
