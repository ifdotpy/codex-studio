"""Root-owned delegation depth policy shared by tool exposure and spawning."""

from __future__ import annotations

from collections.abc import Mapping
from typing import TYPE_CHECKING

from codex_agent_modes import concurrency

if TYPE_CHECKING:
    import sqlite3
    from codex_records import AgentRecord
    from codex_runtime import Runtime

DEFAULT_DELEGATION_DEPTH = 1
MIN_DELEGATION_DEPTH = 1
MAX_DELEGATION_DEPTH = 3


def delegation_depth(root: "AgentRecord") -> int:
    """Read the root setting, treating pre-setting records as lead-only."""
    defaults: Mapping[str, object] = root.get("workerDefaults") or {}
    value = defaults.get("delegationDepth", DEFAULT_DELEGATION_DEPTH)
    if type(value) is not int or not MIN_DELEGATION_DEPTH <= value <= MAX_DELEGATION_DEPTH:
        return 0
    return value


def actor_depth(runtime: "Runtime", db: "sqlite3.Connection", actor: "AgentRecord") -> int | None:
    """Return the actor's level below its lead, or None for a broken ancestry."""
    root_id = actor.get("rootId")
    if not isinstance(root_id, str) or not root_id:
        return None
    cursor = actor
    seen: set[str] = set()
    depth = 0
    while cursor["id"] != root_id:
        if cursor["id"] in seen:
            return None
        seen.add(cursor["id"])
        parent_id = cursor.get("parentId")
        if not isinstance(parent_id, str) or not parent_id:
            return None
        cursor = runtime.agent(parent_id, db)
        if cursor.get("rootId") != root_id:
            return None
        depth += 1
    root = runtime.agent(root_id, db)
    if not root.get("isLead") or root.get("parentId"):
        return None
    return depth


def can_delegate(
    runtime: "Runtime", actor: "AgentRecord", db: "sqlite3.Connection | None" = None
) -> bool:
    """Use the root's concurrency and maximum depth to decide spawn exposure."""
    if actor.get("role") == "reviewer":
        return False
    if db is None:
        with runtime.read_db() as connection:
            return can_delegate(runtime, actor, connection)
    # Spawn receipts and tool fixtures may carry a compact agent projection.
    # Evaluate against the persisted record, which is the authority for ancestry.
    actor = runtime.agent(actor["id"], db)
    if actor.get("role") == "reviewer":
        return False
    root_id = actor.get("rootId") or (actor.get("id") if actor.get("isLead") else None)
    if not isinstance(root_id, str):
        return False
    root = runtime.agent(root_id, db)
    level = actor_depth(runtime, db, actor)
    depth_limit = delegation_depth(root)
    return level is not None and concurrency(root) > 0 and level < depth_limit


def require_delegation(
    runtime: "Runtime", actor: "AgentRecord", db: "sqlite3.Connection | None" = None
) -> None:
    """Reject a spawn that exceeds its lead's current delegation policy."""
    if can_delegate(runtime, actor, db):
        return
    if db is None:
        with runtime.read_db() as connection:
            return require_delegation(runtime, actor, connection)
    actor = runtime.agent(actor["id"], db)
    root_id = actor.get("rootId") or (actor.get("id") if actor.get("isLead") else None)
    if isinstance(root_id, str):
        root = runtime.agent(root_id, db)
        if concurrency(root) == 0:
            from codex_agent_modes import assert_delegation

            assert_delegation(root)
    raise ValueError(
        "This agent cannot delegate at its current level. Check the lead's "
        "delegation depth and subagent concurrency settings."
    )
