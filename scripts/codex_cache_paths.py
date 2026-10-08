"""Resolve the shared user cache root without importing application state."""
import os
from pathlib import Path
import platform


def cache_dir(environment=None, system=None):
    """Return the configured user cache root with the platform precedence."""
    values = os.environ if environment is None else environment
    current_system = platform.system() if system is None else system
    configured = values.get("CODEX_AGENTS_CACHE_DIR")
    xdg = values.get("XDG_CACHE_HOME")
    if current_system == "Windows":
        if configured:
            return Path(configured).expanduser()
        if xdg:
            return Path(xdg).expanduser()
        if values.get("LOCALAPPDATA"):
            return Path(values["LOCALAPPDATA"]) / "CodexStudio" / "cache"
    else:
        if xdg:
            return Path(xdg).expanduser()
        if configured:
            return Path(configured).expanduser()
    if current_system == "Darwin":
        return Path.home() / "Library" / "Caches"
    return Path.home() / ".cache"
