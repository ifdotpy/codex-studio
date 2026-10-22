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
