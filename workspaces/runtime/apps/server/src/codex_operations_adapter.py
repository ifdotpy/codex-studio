"""Fail-fast import and version check for the native operations boundary."""

from __future__ import annotations

import importlib
import json
from types import ModuleType
from typing import Callable

PROTOCOL_VERSION = 3

try:
    native: ModuleType = importlib.import_module("studio_operations_native")
except ImportError as error:
    raise RuntimeError(
        "The backend requires studio_operations_native. Build it with "
        "`cargo build --release -p studio-operations-python`."
    ) from error

def check_protocol(module: ModuleType) -> None:
    if getattr(module, "PROTOCOL_VERSION", None) == PROTOCOL_VERSION:
        return
    raise RuntimeError(
        "studio_operations_native protocol mismatch; rebuild it with "
        "`cargo build --release -p studio-operations-python`."
    )


check_protocol(native)


def reserve_tool_request(
    candidate_record: dict[str, object],
    signature: str,
    agent: str,
    existing_record: dict[str, object] | None,
    lifecycle_fields: tuple[str, ...],
    aliases: list[tuple[str, str, str]],
    fingerprint: Callable[[str, str], bytes],
) -> tuple[str, list[dict[str, object]]]:
    def projection(record: dict[str, object], record_fingerprint: bytes) -> dict[str, object]:
        return {
            "record": {field: record[field] for field in (*lifecycle_fields, "signature")
                       if field in record},
            "fingerprint": list(record_fingerprint),
        }

    candidate_fingerprint = fingerprint(signature, agent)
    existing_fingerprint = None
    if existing_record is not None:
        stored_signature = existing_record.get("signature", "")
        stored_agent = existing_record.get("agent", "")
        if stored_signature is not None and not isinstance(stored_signature, str):
            raise ValueError("Stored tool request signature must be a string")
        if stored_agent is not None and not isinstance(stored_agent, str):
            raise ValueError("Stored tool request agent must be a string")
        existing_fingerprint = fingerprint(
            "" if stored_signature is None else stored_signature,
            "" if stored_agent is None else stored_agent,
        )
    snapshot = {
        "candidate": projection(candidate_record, candidate_fingerprint),
        "existing": None if existing_record is None or existing_fingerprint is None
        else projection(existing_record, existing_fingerprint),
        "aliases": [
            {"agent": agent, "alias": alias, "request": request}
            for agent, alias, request in aliases
        ],
    }
    result = json.loads(native.reserve_tool_request(json.dumps(snapshot, ensure_ascii=False)))
    if not isinstance(result, list) or len(result) != 2:
        raise RuntimeError("Rust returned an invalid tool request reservation result")
    decision, rejection_message = result
    if rejection_message is not None:
        raise ValueError(rejection_message)
    if not isinstance(decision, dict) or len(decision) != 1:
        raise RuntimeError("Rust returned an invalid tool request reservation decision")
    kind, value = next(iter(decision.items()))
    if kind == "Rejected":
        raise RuntimeError("Rust omitted the tool request reservation rejection")
    if kind not in {"Admitted", "ReturnExisting"} or not isinstance(value, dict):
        raise RuntimeError("Rust returned an unknown tool request reservation decision")
    intents = value.get("intents")
    if not isinstance(intents, list) or any(
        not isinstance(intent, dict) or len(intent) != 1 for intent in intents
    ):
        raise RuntimeError("Rust returned invalid tool request reservation write intents")
    return kind, intents
