"""Installed Rust lifecycle binding contract; no backend/provider involved."""

# Support direct execution without runner-provided PYTHONPATH.
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()


import hashlib
import json
import socket
import subprocess
import time
import unittest
from types import SimpleNamespace
from unittest.mock import patch

sys.path.insert(0, str(SERVER_SOURCE_ROOT))

import studio_operations_native as native

from codex_operations_adapter import (
    _legacy_state,
    apply_decision,
    check_protocol,
    content_for_record,
    missing_receipt_outcome,
    state_for_record,
    transition_record,
)

FIXTURE = (
    Path(__file__).resolve().parents[3]
    / "packages/operations/tests/fixtures/python-contract-cases.json"
)


def _fingerprint(value):
    return list(hashlib.sha256(value.encode("utf-8")).digest())


def _attempt(epoch, generation=None):
    return {
        "agent": "agent-a",
        "account": "default",
        "thread": "thread-a",
        "epoch": epoch,
        "generation": generation,
    }


def _fixture_kind(step):
    kind = step["kind"]
    if kind == "admit":
        return {"Admit": {"content": _fingerprint(step["content"])} }
    if kind == "dispatch":
        return "ClaimDispatch"
    if kind == "cancel":
        return "Cancel"
    if kind == "server_restart":
        return "ServerRestarted"
    if kind in {"invalid_response", "timeout", "process_lost"}:
        failure = {
            "invalid_response": "InvalidPostExecutionResponse",
            "timeout": "Timeout",
            "process_lost": "ProcessLost",
        }[kind]
        return {"Failure": failure}
    if kind == "committed_receipt":
        return "CommittedOperationReceipt"
    if kind == "applied_response":
        return "AppliedResponse"
    if kind == "receipt_missing":
        return "ReceiptMissing"
    raise AssertionError(f"Unknown fixture event kind: {kind}")


class OperationsNativeContract(unittest.TestCase):
    def test_python_fixture_replays_through_installed_native_module(self):
        cases = json.loads(FIXTURE.read_text(encoding="utf-8"))
        for case in cases:
            with self.subTest(case=case["name"]):
                state = {"revision": 0, "operation": None, "tombstone": None}
                identity = {
                    "scope": {"ToolRequest": {"account": "default", "thread": "thread-a"}},
                    "request_id": "call-a",
                }
                rejected = False
                for index, step in enumerate(case["steps"]):
                    attempt = _attempt(step.get("epoch", 4), step.get("generation"))
                    event = {
                        "id": f"{case['name']}:{index}",
                        "expected_revision": state["revision"],
                        "identity": identity,
                        "attempt": attempt,
                        "kind": _fixture_kind(step),
                    }
                    decision = json.loads(
                        native.transition(json.dumps(state), json.dumps(event))
                    )
                    rejected = isinstance(decision["kind"], dict)
                    if not rejected:
                        state = decision["next_state"]
                actual = (
                    state["operation"]["state"]
                    if state["operation"] is not None
                    else "Absent"
                )
                self.assertEqual(actual, case["rust_state"])
                self.assertEqual(
                    rejected,
                    case["name"] == "r14_changed_content_rejects_before_effect",
                )

    def test_malformed_native_payload_raises_value_error(self):
        with self.assertRaisesRegex(ValueError, "Invalid operation state"):
            native.transition("{", "{}")

    def test_protocol_mismatch_fails_clearly(self):
        with self.assertRaisesRegex(RuntimeError, "protocol mismatch"):
            check_protocol(SimpleNamespace(PROTOCOL_VERSION=native.PROTOCOL_VERSION + 1))

    def test_all_current_persisted_record_states_replay_without_dispatch(self):
        records = [
            ("queued", "pending", False, "Reserved"),
            ("running", "pending", False, "Dispatched"),
            ("running", "pending", True, "CancelRequested"),
            ("cancelled", "not_applied", True, "CancelledBeforeDispatch"),
            ("interrupted", "unknown", False, "Unknown"),
            ("unknown", "unknown", False, "Unknown"),
            ("queued", "unknown", False, "Unknown"),
            ("completed", "applied", False, "Applied"),
            ("failed", "not_applied", False, "NotApplied"),
            ("failed", "unknown", False, "Unknown"),
        ]
        for index, (stage, outcome, cancelled, expected) in enumerate(records):
            with self.subTest(stage=stage, outcome=outcome, cancelled=cancelled):
                record = {
                    "id": f"request-{index}",
                    "agent": "agent-a",
                    "accountKey": "default",
                    "threadId": "thread-a",
                    "callId": f"call-{index}",
                    "signature": "existing-signature",
                    "epoch": 4,
                    "stage": stage,
                    "outcome": outcome,
                    "cancelRequested": cancelled,
                }
                self.assertEqual(_legacy_state(record)[1], expected)
                state, _, _ = state_for_record(record)
                decision, _ = transition_record(
                    record, "Admit", content=content_for_record(record)
                )
                self.assertEqual(decision["kind"], "ReturnExisting")
                self.assertEqual(state["operation"]["state"], expected)
                self.assertFalse(apply_decision(record, decision))

    def test_adapter_transition_performs_no_io(self):
        record = {
            "id": "request-io",
            "agent": "agent-a",
            "accountKey": "default",
            "threadId": "thread-a",
            "callId": "call-io",
            "signature": "signature",
            "epoch": 4,
            "stage": "queued",
            "outcome": "pending",
            "cancelRequested": False,
        }
        with (
            patch("builtins.open", side_effect=AssertionError("I/O")),
            patch.object(Path, "open", side_effect=AssertionError("I/O")),
            patch.object(socket, "socket", side_effect=AssertionError("I/O")),
            patch.object(subprocess, "Popen", side_effect=AssertionError("I/O")),
            patch.object(time, "time", side_effect=AssertionError("I/O")),
        ):
            decision, _ = transition_record(record, "ClaimDispatch")
            self.assertEqual(decision["kind"], "Transitioned")
            self.assertEqual(missing_receipt_outcome("missing-request"), "unknown")


if __name__ == "__main__":
    unittest.main()
