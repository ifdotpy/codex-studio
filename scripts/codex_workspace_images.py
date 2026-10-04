"""Platform-neutral image workspace engine.

Platform backends implement :class:`WorkspaceBackend`. Git object setup and
agent commit collection stay in this module so every backend has identical
merge semantics.
"""

from __future__ import annotations

from pathlib import Path
from typing import Any, Protocol


class WorkspaceBackend(Protocol):
    """Storage operations required by the common workspace engine."""

    def supported(self, repo_root: Path) -> tuple[bool, str]: ...

    def build_base(self, repo_root: Path, base_dir: Path, version: str,
                   token: Any) -> dict[str, Any]: ...

    def clone_workspace(self, base_image: Path, agent_dir: Path) -> Path: ...

    def mount_workspace(self, layer: Path, mount: Path, *,
                        base_image: Path | None = None) -> dict[str, Any]: ...

    def sync_delta(self, repo_root: Path, target_repo: Path, token: Any) -> Any: ...

    def unmount_workspace(self, mount: Path, *, force: bool = False) -> None: ...

    def remove_layer(self, agent_dir: Path) -> None: ...

    def private_bytes(self, path: Path) -> int: ...

    def exec_prefix(self) -> list[str]: ...


def _backend() -> WorkspaceBackend:
    """Load the host backend lazily so importing this module is portable."""
    import sys

    if sys.platform == "darwin":
        from codex_workspace_macos import Backend
    elif sys.platform.startswith("linux"):
        from codex_workspace_linux import Backend
    else:
        raise RuntimeError("Image workspaces are not supported on this platform")
    return Backend()
