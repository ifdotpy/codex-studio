"""Small fixture for exercising settlement through the typed native boundary."""

import json
from typing import Literal

import studio_operations_native as native


def settlement_outcome(
    tool: str,
    result: dict[str, object],
    *,
    read_only: bool = False,
    outcome: Literal["applied", "not_applied", "unknown"] | None = None,
    operation_receipt_present: bool = False,
    operation_receipt_result: str | None = None,
) -> str:
    record = {
        "id": "settlement-contract",
        "callId": "settlement-contract",
        "accountKey": "default",
        "threadId": "thread-contract",
        "agent": "agent-contract",
        "epoch": 1,
        "stage": "running",
        "outcome": "pending",
        "cancelRequested": False,
        "readOnly": read_only,
        "tool": tool,
    }
    decision = native.settle_tool_request(json.dumps({
        "record": record,
        "fingerprint": [0] * 32,
        "result": result,
        "result_changed": True,
        "outcome": outcome,
        "proof": None,
        "operation_receipt_result": operation_receipt_result or ("{}" if operation_receipt_present else None),
    }))
    return decision.outcome
