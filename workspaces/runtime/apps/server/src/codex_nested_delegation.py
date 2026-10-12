"""Root-owned delegation depth policy shared by tool exposure and spawning."""

from __future__ import annotations

from collections.abc import Mapping
from dataclasses import dataclass
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
    stored = root.get("workerDefaults")
    defaults: Mapping[str, object] = stored if isinstance(stored, Mapping) else {}
    value = defaults.get("delegationDepth", DEFAULT_DELEGATION_DEPTH)
    if type(value) is not int or not MIN_DELEGATION_DEPTH <= value <= MAX_DELEGATION_DEPTH:
        return DEFAULT_DELEGATION_DEPTH
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
        try:
            cursor = runtime.agent(parent_id, db)
        except ValueError:
            return None
        if cursor.get("rootId") != root_id:
            return None
        depth += 1
    root = runtime.agent(root_id, db)
    if not root.get("isLead") or root.get("parentId"):
        return None
    return depth


@dataclass(frozen=True)
class DelegationDecision:
    allowed: bool
    reason: str | None = None


def delegation_decision(
    runtime: "Runtime", actor: "AgentRecord", db: "sqlite3.Connection | None" = None
) -> DelegationDecision:
    """Evaluate delegation once, returning a reason when policy denies it."""
    if db is None:
        with runtime.read_db() as connection:
            return delegation_decision(runtime, actor, connection)
    # Spawn receipts and tool fixtures may carry a compact agent projection.
    # Evaluate against the persisted record, which is the authority for ancestry.
    actor = runtime.agent(actor["id"], db)
    if actor.get("role") == "reviewer":
        return DelegationDecision(False, "reviewer")
    root_id = actor.get("rootId") or (actor.get("id") if actor.get("isLead") else None)
    if not isinstance(root_id, str):
        return DelegationDecision(False, "broken ancestry")
    try:
        root = runtime.agent(root_id, db)
    except ValueError:
        return DelegationDecision(False, "broken ancestry")
    level = actor_depth(runtime, db, actor)
    if level is None:
        return DelegationDecision(False, "broken ancestry")
    if concurrency(root) <= 0:
        return DelegationDecision(False, "no subagent concurrency")
    depth_limit = delegation_depth(root)
    if level >= depth_limit:
        return DelegationDecision(False, "delegation depth reached")
    return DelegationDecision(True)


def can_delegate(
    runtime: "Runtime", actor: "AgentRecord", db: "sqlite3.Connection | None" = None
) -> bool:
    """Use the root's concurrency and maximum depth to decide spawn exposure."""
    return delegation_decision(runtime, actor, db).allowed


def require_delegation(
    runtime: "Runtime", actor: "AgentRecord", db: "sqlite3.Connection | None" = None
) -> None:
    """Reject a spawn that exceeds its lead's current delegation policy."""
    decision = delegation_decision(runtime, actor, db)
    if decision.allowed:
        return
    raise ValueError(f"This agent cannot delegate: {decision.reason}.")
