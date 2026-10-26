"""Project account transfer records into the public API shape."""

from __future__ import annotations

from typing import Any

from .models import TransferMemberPhase

_TRANSFER_FIELDS = (
    "id", "leadId", "targetAccountKey", "status", "created", "updated",
    "scope", "targetProvider", "finishHistory", "requests",
)
_TRANSFER_MEMBER_FIELDS = (
    "phase", "sourceAccountKey", "sourceThreadId", "targetThreadId", "provider",
    "name", "reason", "error", "waiting", "lazy", "interruptReason",
    "interruptOutcome", "continueAfterTransfer",
)
_MEMBER_TERMINAL_PHASES = {TransferMemberPhase.COMPLETED, TransferMemberPhase.LEFT}
_MEMBER_MOVING_PHASES = {
    TransferMemberPhase.READING,
    TransferMemberPhase.SUBMITTED,
    TransferMemberPhase.LAZY_SUBMITTED,
    TransferMemberPhase.INTERRUPTING,
}
_MEMBER_WAIT_EXCLUDED_PHASES = {
    TransferMemberPhase.BLOCKED,
    TransferMemberPhase.UNKNOWN,
    TransferMemberPhase.LAZY,
    TransferMemberPhase.LAZY_SUBMITTED,
}


def public_transfer_data(operation: dict[str, Any]) -> dict[str, Any]:
    """Return the public transfer projection without loading the HTTP router."""
    projected = {key: operation[key] for key in _TRANSFER_FIELDS if key in operation}
    raw_members = operation.get("members")
    members: dict[str, Any] = {}
    can_retry = False
    if isinstance(raw_members, dict):
        for agent_id, raw_member in raw_members.items():
            if isinstance(raw_member, dict):
                if (
                    raw_member.get("phase") == TransferMemberPhase.BLOCKED.value
                    and raw_member.get("archiveInvalidated") is not True
                ):
                    can_retry = True
                members[agent_id] = {
                    key: raw_member[key]
                    for key in _TRANSFER_MEMBER_FIELDS
                    if key in raw_member
                }
    projected["members"] = members

    phase_members: list[tuple[TransferMemberPhase, dict[str, Any]]] = []
    for member in members.values():
        if not isinstance(member, dict):
            continue
        phase_value = member.get("phase")
        if isinstance(phase_value, str):
            phase_members.append((TransferMemberPhase(phase_value), member))
    phases = [phase for phase, _member in phase_members]
    projected.update({
        "total": len(phases),
        "completed": sum(phase in _MEMBER_TERMINAL_PHASES for phase in phases),
        "moved": sum(
            phase is TransferMemberPhase.COMPLETED or member.get("lazy") is True
            for phase, member in phase_members
        ),
        "nativeHistoryPending": sum(
            isinstance(member, dict)
            and member.get("lazy") is True
            and member.get("phase") != TransferMemberPhase.COMPLETED.value
            for member in members.values()
        ),
        "movingNow": sum(phase in _MEMBER_MOVING_PHASES for phase in phases),
        "canFinishHistory": False,
        "interrupted": _transfer_notices(members, "interruptReason"),
        "leftOnSource": _transfer_notices(members, "phase", "left"),
        "blocked": _transfer_notices(members, "phase", "blocked", "unknown"),
        "waitingCount": sum(
            phase not in _MEMBER_TERMINAL_PHASES
            and phase not in _MEMBER_WAIT_EXCLUDED_PHASES
            for phase in phases
        ),
        "needsAttention": any(
            phase in {TransferMemberPhase.BLOCKED, TransferMemberPhase.UNKNOWN}
            for phase in phases
        ),
        "canRetry": can_retry,
    })
    return projected


def _transfer_notices(
    members: dict[str, Any],
    condition_key: str,
    *condition_values: str,
) -> list[dict[str, str]]:
    notices: list[dict[str, str]] = []
    for agent_id, member in members.items():
        if not isinstance(member, dict):
            continue
        condition = member.get(condition_key)
        if not condition:
            continue
        if condition_values and condition not in condition_values:
            continue
        notice = {"id": agent_id}
        for key in ("name", "provider"):
            value = member.get(key)
            if isinstance(value, str):
                notice[key] = value
        reason_key = "interruptReason" if condition_key == "interruptReason" else (
            "error" if condition_key == "phase" and condition in {"blocked", "unknown"}
            else "reason"
        )
        reason = member.get(reason_key)
        if isinstance(reason, str):
            notice["reason"] = reason
        notices.append(notice)
    return notices
