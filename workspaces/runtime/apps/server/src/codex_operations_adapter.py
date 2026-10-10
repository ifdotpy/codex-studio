"""In-memory conversion between durable tool receipts and the Rust lifecycle."""

from __future__ import annotations

import hashlib
import importlib
import json
import uuid
from typing import Any

try:
    _native = importlib.import_module("studio_operations_native")
except ImportError as error:
    raise RuntimeError(
        "The backend requires studio_operations_native. Build it with "
        "`cargo build --release -p studio-operations-python`."
    ) from error

PROTOCOL_VERSION = 1


def _compact(value: Any) -> str:
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"))


def _integer(value: Any, default: int = 0) -> int:
    return value if type(value) is int and value >= 0 else default


def _identity(record: dict[str, Any]) -> dict[str, Any]:
    account = str(record.get("accountKey", "default"))
    thread = str(record.get("threadId", ""))
    prefix = (account + ":" if account != "default" else "") + thread + ":"
    key = str(record.get("id", ""))
    canonical_request_id = key[len(prefix):] if key.startswith(prefix) else str(record.get("callId", ""))
    return {
        "scope": {
            "ToolRequest": {
                "account": account,
                "thread": thread,
            }
        },
        "request_id": canonical_request_id,
    }


def _attempt(record: dict[str, Any], generation: int | None = None) -> dict[str, Any]:
    saved_generation = record.get("supervisor", {}).get("generation") if isinstance(record.get("supervisor"), dict) else None
    return {
        "agent": str(record.get("agent", "")),
        "account": str(record.get("accountKey", "default")),
        "thread": str(record.get("threadId", "")),
        "epoch": _integer(record.get("epoch")),
        "generation": generation if generation is not None else (
            saved_generation if type(saved_generation) is int and saved_generation >= 0 else None
        ),
    }


def _content(record: dict[str, Any]) -> list[int]:
    signed = _compact([record.get("signature", ""), record.get("agent", "")]).encode("utf-8")
    return list(hashlib.sha256(signed).digest())


def content_for_record(record: dict[str, Any]) -> list[int]:
    return _content(record)


def _legacy_state(record: dict[str, Any]) -> tuple[int, str]:
    """Map every persisted stage/outcome pair, including pre-adapter records."""
    stage, outcome = record.get("stage"), record.get("outcome")
    cancelled = bool(record.get("cancelRequested"))
    if stage == "queued" and outcome == "pending":
        return 0, "Reserved"
    if stage == "running" and outcome == "pending":
        return (2, "CancelRequested") if cancelled else (1, "Dispatched")
    if stage == "cancelled" and outcome == "not_applied":
        return 3, "CancelledBeforeDispatch"
    if stage in {"queued", "running", "interrupted", "failed", "unknown"} and outcome == "unknown":
        return 3, "Unknown"
    if stage == "completed" and outcome == "applied":
        return 3, "Applied"
    if stage == "failed" and outcome == "not_applied":
        return 3, "NotApplied"
    if stage == "failed" and outcome == "unknown":
        return 3, "Unknown"
    raise ValueError(f"Unsupported stored tool request lifecycle: {stage}/{outcome}")


def state_for_record(record: dict[str, Any]) -> tuple[dict[str, Any], dict[str, Any], dict[str, Any]]:
    revision, state_name = _legacy_state(record)
    identity = _identity(record)
    attempt = _attempt(record)
    state = {
        "revision": revision,
        "operation": {
            "identity": identity,
            "content": _content(record),
            "attempt": attempt,
            "state": state_name,
        },
        "tombstone": None,
    }
    return state, identity, attempt


def _raise_rejection(error: str) -> None:
    if error == "DifferentContent":
        raise ValueError("This request id has different content")
    if error == "IdentityMismatch":
        raise ValueError("This request id has different content")
    if error == "ConflictingEvidence":
        raise ValueError("Conflicting evidence for tool request")
    if error == "StaleRevision":
        raise ValueError("This request receipt changed while applying its lifecycle transition")
    if error == "StaleEpoch":
        raise ValueError("This request belongs to an older account/thread epoch")
    if error == "StaleGeneration":
        raise ValueError("This request belongs to an older process generation")
    if error == "GenerationUnavailable":
        raise ValueError("The request has no captured process generation")
    raise ValueError(f"Invalid tool request lifecycle transition: {error}")


def transition_record(
    record: dict[str, Any], kind: Any, *, empty: bool = False,
    generation: int | None = None, event_name: str | None = None,
    content: list[int] | None = None,
) -> tuple[dict[str, Any], dict[str, Any]]:
    if empty:
        state: dict[str, Any] = {"revision": 0, "operation": None, "tombstone": None}
        identity, attempt = _identity(record), _attempt(record, generation)
    else:
        state, identity, attempt = state_for_record(record)
        if generation is not None:
            attempt["generation"] = generation
    if content is not None:
        kind = {"Admit": {"content": content}}
    event = {
        "id": event_name or str(uuid.uuid5(uuid.NAMESPACE_URL, f"{record.get('id')}:{state['revision']}:{_compact(kind)}")),
        "expected_revision": state["revision"],
        "identity": identity,
        "attempt": attempt,
        "kind": kind,
    }
    decision = json.loads(_native.transition(_compact(state), _compact(event)))
    return decision, state


def apply_decision(record: dict[str, Any], decision: dict[str, Any], *, stage: str | None = None) -> bool:
    """Apply only the Rust next state; return whether it made a transition."""
    if decision["kind"] != "Transitioned":
        return False
    operation = decision["next_state"]["operation"]
    if operation is None:
        raise RuntimeError("Rust lifecycle decision omitted its active operation")
    state = operation["state"]
    persisted = {
        "Reserved": ("queued", "pending", False),
        "Dispatched": ("running", "pending", False),
        "CancelRequested": ("running", "pending", True),
        "CancelledBeforeDispatch": (
            "cancelled", "not_applied", bool(record.get("cancelRequested"))
        ),
        "NotApplied": ("failed", "not_applied", bool(record.get("cancelRequested"))),
        "Applied": ("completed", "applied", bool(record.get("cancelRequested"))),
        "Unknown": ("failed", "unknown", bool(record.get("cancelRequested"))),
    }
    if state not in persisted:
        raise RuntimeError(f"Rust returned an unsupported lifecycle state: {state}")
    next_stage, outcome, cancelled = persisted[state]
    record.update(stage=stage or next_stage, outcome=outcome, cancelRequested=cancelled)
    return True


def check_protocol(module: Any) -> None:
    if getattr(module, "PROTOCOL_VERSION", None) != PROTOCOL_VERSION:
        raise RuntimeError(
            "studio_operations_native protocol mismatch; rebuild it with "
            "`cargo build --release -p studio-operations-python`."
        )


check_protocol(_native)


def require_decision(decision: dict[str, Any]) -> None:
    kind_value = decision["kind"]
    if isinstance(kind_value, dict) and "Rejected" in kind_value:
        _raise_rejection(kind_value["Rejected"])


def missing_receipt_outcome(request_id: str) -> str:
    decision, _ = transition_record(
        {"id": request_id, "callId": request_id}, "ReceiptMissing", empty=True
    )
    if decision["kind"] == "ReceiptMissingUnknown":
        return "unknown"
    require_decision(decision)
    raise RuntimeError("Rust returned an invalid missing-receipt decision")
