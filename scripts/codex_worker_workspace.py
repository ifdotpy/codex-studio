"""Choose and validate the isolation mode for a spawned worker."""
from __future__ import annotations

import platform
from typing import Any, Mapping

WORKSPACE_MODES = ("image", "worktree", "shared")


def select(
    runtime: Any,
    spec: Mapping[str, Any],
    workspace_root: str | None,
    git_repo: str | None,
    environment: str,
) -> tuple[str, str | None]:
    requested = spec.get("workspace")
    explicit = "workspace" in spec
    role = spec.get("role", "implementer")

    if explicit and (not isinstance(requested, str) or requested not in WORKSPACE_MODES):
        raise ValueError('workspace must be "image", "worktree", or "shared"')
    if role == "reviewer":
        if explicit:
            raise ValueError("workspace applies to implementers; reviewers use the shared folder with read-only access")
        return "shared", None

    if environment == "linux":
        if explicit and requested != "image":
            raise ValueError('environment "linux" requires workspace "image"')
        if platform.system() == "Windows":
            raise ValueError('workspace "image" is unavailable on Windows')
        return "image", None

    if explicit:
        if not isinstance(requested, str):
            raise ValueError('workspace must be "image", "worktree", or "shared"')
        if requested == "image":
            if platform.system() == "Windows":
                raise ValueError('workspace "image" is unavailable on Windows')
            supported, reason = runtime.image_workspace_support(workspace_root)
            if not supported:
                detail = (": " + str(reason)) if reason else ""
                raise ValueError("workspace \"image\" is unavailable" + detail)
        elif requested == "worktree" and not git_repo:
            raise ValueError('workspace "worktree" requires a Git repository')
        return requested, None

    if spec.get("_defaultWorkspaceMode") == "worktree":
        if git_repo:
            return "worktree", None
        return "shared", "This folder has no Git repository; the worker uses the shared folder"
    supported, reason = runtime.image_workspace_support(workspace_root)
    if supported:
        return "image", None
    if git_repo:
        return "worktree", "Image workspaces are unavailable: " + str(reason)[:500]
    return "shared", "Image workspaces are unavailable: " + str(reason)[:500]


def image_backend(environment: str) -> str:
    return "vm" if environment == "linux" or platform.system() == "Linux" else "asif"
