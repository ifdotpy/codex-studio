# Linux and WSL server setup

Install Python 3.11 or later, Node.js 22.15 or later, Git, and the provider CLI.
Sign in to the provider as the server user. Keep the source and state on the
Linux filesystem. Windows Subsystem for Linux (WSL) uses the same setup.

From the checkout:

```sh
npm ci
python3 scripts/install-cli.py
npm --prefix web ci
npm --prefix web run build
```

Add `~/.local/bin` and the provider CLI directory to `PATH`. Select one idle
boundary before the first cutover. Do not send new work during setup.

```sh
codex-supervisor enable --port 4720 --state ~/.local/state/codex-agents
```

For a running server, the command requires `codex-studio.service` to own the
backend PID. It refuses an unrelated listener or occupied state directory.
It installs `~/.config/systemd/user/codex-studio-supervisor.service` and
`~/.config/systemd/user/codex-studio.service`. It preserves the backend's reported
restart environment and captures the current `PATH`. Both units use
`Restart=always` and `KillMode=process`. The backend starts after the supervisor
unit and waits for its socket, state identity, and protocol 1. The supervisor
does not depend on the backend unit's lifetime.

For service operation after logout, enable user linger once:

```sh
sudo loginctl enable-linger "$USER"
```

Check the installation:

```sh
systemctl --user status codex-studio-supervisor codex-studio
curl --fail http://127.0.0.1:4720/api/desktop
```

The response must contain `supervisorMode: true` and supervisor `protocol: 1`.
For a planned backend restart, use:

```sh
systemctl --user restart codex-studio
```

Do not stop or restart `codex-studio-supervisor` while its handles are live.
A supervisor failure has the separate recovery limits in
[restart recovery](restart-recovery.md).

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
