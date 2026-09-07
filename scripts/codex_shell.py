"""Shell startup for managed commands on Codex's local Unix host."""

import os
from pathlib import Path
import pwd
import re
import shlex
import shutil
import sys


def default_shell():
    """Match Codex shell_detect: passwd first, then platform fallbacks."""
    try:
        preferred = Path(pwd.getpwuid(os.getuid()).pw_shell)
    except KeyError:
        preferred = None
    supported = {"zsh", "bash", "sh", "pwsh", "powershell"}
    if preferred and preferred.name in supported and preferred.is_file():
        return str(preferred)
    names = ("zsh", "bash", "sh") if sys.platform == "darwin" else ("bash", "zsh", "sh")
    for name in names:
        path = shutil.which(name)
        if path:
            return path
        for prefix in ("/bin", "/usr/bin"):
            candidate = Path(prefix) / name
            if candidate.is_file():
                return str(candidate)
    raise ValueError("No supported shell is available for the command")


def monitor_command(server, command, cwd, *, config=None):
    """Use native config and the rc setup used by Codex shell snapshots.

    command/exec already applies the account's shell_environment_policy. Its
    argv API does not perform the shell startup added by native exec_command.
    Run that startup inside the command sandbox, without copying ambient PATH.
    """
    if config is None:
        config = server.call("config/read", {"cwd": cwd, "includeLayers": False})["config"]
    shell = default_shell()
    name = Path(shell).name
    login = config.get("allow_login_shell", True)
    if name in {"pwsh", "powershell"}:
        return [shell, *([] if login else ["-NoProfile"]), "-Command", command]
    if not login:
        return [shell, "-c", command]
    snapshot = (config.get("features") or {}).get("shell_snapshot", True)
    if not snapshot or name not in {"zsh", "bash"}:
        return [shell, "-lc", command]

    # Codex shell_snapshot.rs explicitly sources these rc files in a login
    # shell. Merely changing sh to zsh still misses Homebrew in ~/.zshrc.
    startup = (
        'if [ -r "${ZDOTDIR:-$HOME}/.zshrc" ]; then . "${ZDOTDIR:-$HOME}/.zshrc"; fi'
        if name == "zsh"
        else 'if [ -z "$BASH_ENV" ] && [ -r "$HOME/.bashrc" ]; then . "$HOME/.bashrc"; fi'
    )
    lines = ["{ " + startup + "; } >/dev/null 2>&1"]
    # Explicit policy values win over startup files, as in Codex's snapshot
    # wrapper. Quote values as shell data, never interpolate them as code.
    for key, value in ((config.get("shell_environment_policy") or {}).get("set") or {}).items():
        if re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", key):
            lines.append(f"export {key}={shlex.quote(str(value))}")
    lines.append(f"exec {shlex.quote(shell)} -c {shlex.quote(command)}")
    return [shell, "-lc", "\n".join(lines)]
