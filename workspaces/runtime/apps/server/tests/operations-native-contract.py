"""Installed Rust lifecycle binding contract through the Python caller."""

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))

from codex_layout import SERVER_SOURCE_ROOT

from test_isolation import isolate_supervisor_environment

isolate_supervisor_environment()

import json
import unittest
from types import SimpleNamespace

sys.path.insert(0, str(SERVER_SOURCE_ROOT))

import studio_operations_native as native

from codex_operations_adapter import PROTOCOL_VERSION, check_protocol
from codex_tool_requests import _apply_decision, _lifecycle, _missing_receipt_outcome

FIXTURE = (
    Path(__file__).resolve().parents[3]
    / "packages/operations/tests/fixtures/python-contract-cases.json"
)


def _record(stage="queued", outcome="pending", cancelled=False):
    return {
        "id": "default:thread-a:call-a",
        "agent": "agent-a",
        "accountKey": "default",
        "threadId": "thread-a",
        "callId": "call-a",
        "signature": "signature-a",
        "epoch": 4,
        "stage": stage,
        "outcome": outcome,
        "cancelRequested": cancelled,
        "readOnly": False,
    }


class OperationsNativeContract(unittest.TestCase):
    def test_python_fixture_replays_through_typed_record_boundary(self):
        for case in json.loads(FIXTURE.read_text(encoding="utf-8")):
            with self.subTest(case=case["name"]):
                record = _record()
                record.pop("stage")
                record.pop("outcome")
                rejected = False
                last_kind = None
                for step in case["steps"]:
                    record["epoch"] = step.get("epoch", 4)
                    if step.get("generation") is not None:
                        record["supervisor"] = {"generation": step["generation"]}
                    kind = step["kind"]
                    event = {
                        "admit": "Admit",
                        "dispatch": "ClaimDispatch",
                        "cancel": "Cancel",
                        "server_restart": "ServerRestarted",
                        "invalid_response": "Failure",
                        "timeout": "Failure",
                        "process_lost": "Failure",
                        "committed_receipt": "CommittedOperationReceipt",
                        "applied_response": "AppliedResponse",
                        "receipt_missing": "ReceiptMissing",
                    }[kind]
                    empty = "stage" not in record
                    if empty and event == "Admit" and step.get("content") is not None:
                        record["signature"] = step["content"]
                    decision = native.transition_record(
                        json.dumps(record),
                        event,
                        empty=empty,
                        generation=step.get("generation"),
                        admission_signature=step.get("content"),
                        admission_agent=record["agent"],
                    )
                    last_kind = decision.kind
                    rejected = last_kind == "Rejected"
                    if rejected:
                        self.assertEqual(
                            decision.rejection_message,
                            "This request id has different content",
                        )
                        continue
                    _apply_decision(record, decision)
                    if event == "ServerRestarted" and record["stage"] != "cancelled":
                        # Match the caller's additional recovery label after Rust records
                        # the uncertain outcome.
                        record["stage"] = "interrupted"
                self.assertEqual(rejected, case["name"] == "r14_changed_content_rejects_before_effect")
                if case["name"] == "r14_same_identity_and_content_returns_saved_running_operation":
                    self.assertEqual(last_kind, "ReturnExisting")
                elif case["name"] == "r15_missing_receipt_is_unknown_not_proof":
                    self.assertEqual(last_kind, "ReceiptMissingUnknown")
                else:
                    self.assertEqual(last_kind == "Rejected", rejected)
                self.assertEqual(record.get("stage", "not_found"), case["python_stage"])
                self.assertEqual(record.get("outcome", "unknown"), case["python_outcome"])

    def test_protocol_mismatch_fails_clearly(self):
        self.assertEqual(native.PROTOCOL_VERSION, PROTOCOL_VERSION)
        with self.assertRaisesRegex(RuntimeError, "protocol mismatch"):
            check_protocol(SimpleNamespace(PROTOCOL_VERSION=native.PROTOCOL_VERSION + 1))

    def test_supported_persisted_pairs_replay_without_a_transition(self):
        records = [
            ("queued", "pending", False, "Reserved", "queued", "pending", False),
            ("running", "pending", False, "Dispatched", "running", "pending", False),
            ("running", "pending", True, "CancelRequested", "running", "pending", True),
            ("cancelled", "not_applied", True, "CancelledBeforeDispatch", "cancelled", "not_applied", True),
            ("interrupted", "unknown", False, "Unknown", "failed", "unknown", False),
            ("unknown", "unknown", False, "Unknown", "failed", "unknown", False),
            ("queued", "unknown", False, "Unknown", "failed", "unknown", False),
            ("completed", "applied", False, "Applied", "completed", "applied", False),
            ("failed", "not_applied", False, "NotApplied", "failed", "not_applied", False),
            ("failed", "unknown", False, "Unknown", "failed", "unknown", False),
        ]
        for stage, outcome, cancelled, state, persisted_stage, persisted_outcome, persisted_cancel in records:
            with self.subTest(stage=stage, outcome=outcome, cancelled=cancelled):
                result = _lifecycle(_record(stage, outcome, cancelled), "Admit")
                self.assertEqual(result.kind, "ReturnExisting")
                self.assertEqual(result.stage, persisted_stage)
                self.assertEqual(result.outcome, persisted_outcome)
                self.assertEqual(result.cancel_requested, persisted_cancel)

    def test_transition_result_drives_persisted_write_and_duplicate_dispatch_is_noop(self):
        record = _record()
        decision = _lifecycle(record, "ClaimDispatch")
        self.assertEqual(decision.kind, "Transitioned")
        self.assertTrue(_apply_decision(record, decision))
        self.assertEqual((record["stage"], record["outcome"]), ("running", "pending"))
        repeated = _lifecycle(record, "ClaimDispatch")
        self.assertEqual(repeated.kind, "NoEffect")
        self.assertFalse(_apply_decision(record, repeated))

    def test_unknown_persisted_pair_rejects_with_legacy_message(self):
        record = _record("future-stage", "pending")
        with self.assertRaisesRegex(
            ValueError,
            "Unsupported stored tool request lifecycle: future-stage/pending",
        ):
            _lifecycle(record, "ServerRestarted")

    def test_changed_content_and_missing_receipt_keep_existing_messages(self):
        record = _record()
        decision = native.transition_record(
            json.dumps(record), "Admit", admission_signature="different-signature"
        )
        self.assertEqual(decision.kind, "Rejected")
        self.assertEqual(decision.rejection_code, "DifferentContent")
        self.assertEqual(decision.rejection_message, "This request id has different content")
        self.assertEqual(_missing_receipt_outcome("missing-request"), "unknown")


if __name__ == "__main__":
    unittest.main()
