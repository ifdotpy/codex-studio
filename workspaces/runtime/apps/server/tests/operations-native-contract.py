"""Installed Rust lifecycle binding contract through the Python caller."""

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))

from codex_layout import SERVER_SOURCE_ROOT

from test_isolation import isolate_supervisor_environment

isolate_supervisor_environment()

import json
import ast
import unittest
from unittest.mock import patch
from types import SimpleNamespace

sys.path.insert(0, str(SERVER_SOURCE_ROOT))

import studio_operations_native as native

from codex_operations_adapter import PROTOCOL_VERSION, check_protocol
from codex_tool_requests import (
    RequestMixin, _apply_decision, _content_fingerprint, _lifecycle, _prefix,
    _missing_receipt_outcome,
)

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
    def test_reservation_bridge_returns_typed_decision_fields_and_aliases(self):
        record = _record()
        record["request_id"] = "request-a"
        fingerprint = list(_content_fingerprint(record['signature'], record['agent']))
        snapshot = {
            'candidate': {'record': record, 'fingerprint': fingerprint},
            'existing': None,
            'call_id': 'call-a',
        }
        decision = native.reserve_tool_request(json.dumps(snapshot), "thread-a:")
        self.assertEqual(decision.kind, "Admitted")
        self.assertEqual((decision.stage, decision.outcome, decision.cancel_requested),
                         ("queued", "pending", False))
        self.assertEqual(decision.aliases, [
            ("agent-a", "call-a", record["id"]),
            ("agent-a", "thread-a:call-a", record["id"]),
            ("agent-a", "request-a", record["id"]),
        ])

        snapshot['candidate']['record']['epoch'] = 9
        snapshot['existing'] = {
            'record': {**record, 'stage': 'queued', 'outcome': 'pending'},
            'fingerprint': fingerprint,
        }
        decision = native.reserve_tool_request(json.dumps(snapshot), "thread-a:")
        self.assertEqual(decision.kind, "ReturnExisting")
        self.assertEqual(len(decision.aliases), 3)

        snapshot['candidate']['fingerprint'] = list(b'\x02' * 32)
        with self.assertRaisesRegex(ValueError, 'This request id has different content'):
            native.reserve_tool_request(json.dumps(snapshot), "thread-a:")

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
                    if event == "Admit":
                        candidate = {**record}
                        if step.get("content") is not None:
                            candidate["signature"] = step["content"]
                        snapshot = {
                            "candidate": {
                                "record": candidate,
                                "fingerprint": list(_content_fingerprint(
                                    candidate["signature"], candidate["agent"])),
                            },
                            "existing": None if empty else {
                                "record": record,
                                "fingerprint": list(_content_fingerprint(
                                    record["signature"], record["agent"])),
                            },
                            "call_id": candidate["callId"],
                        }
                        try:
                            decision = native.reserve_tool_request(
                                json.dumps(snapshot), _prefix("default", record["threadId"]))
                        except ValueError as error:
                            self.assertEqual(str(error), "This request id has different content")
                            rejected = True
                            last_kind = "Rejected"
                            continue
                        last_kind = decision.kind
                        if decision.kind == "Admitted":
                            record.update(stage=decision.stage, outcome=decision.outcome,
                                          cancelRequested=decision.cancel_requested,
                                          signature=candidate["signature"])
                        continue
                    decision = _lifecycle(
                        record,
                        event,
                        empty=empty,
                        generation=step.get("generation"),
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

    def test_committed_receipt_followed_by_inferred_failure_stays_unknown(self):
        record = _record("running", "pending")
        receipt = _lifecycle(record, "CommittedOperationReceipt")
        self.assertTrue(_apply_decision(record, receipt))
        self.assertEqual((record["stage"], record["outcome"]), ("failed", "unknown"))
        failure = _lifecycle(record, "Failure")
        _apply_decision(record, failure)
        self.assertEqual(record["outcome"], "unknown")
        self.assertNotEqual(record["outcome"], "applied")

    def test_unknown_persisted_pair_rejects_with_legacy_message(self):
        record = _record("future-stage", "pending")
        with self.assertRaisesRegex(
            ValueError,
            "Invalid stored tool request record",
        ):
            _lifecycle(record, "ServerRestarted")

    def test_changed_content_and_missing_receipt_keep_existing_messages(self):
        record = _record()
        fingerprint = list(_content_fingerprint(record['signature'], record['agent']))
        snapshot = {
            'candidate': {'record': {**record, 'signature': 'different-signature'},
                          'fingerprint': list(_content_fingerprint('different-signature', record['agent']))},
            'existing': {'record': record, 'fingerprint': fingerprint},
            'call_id': record['callId'],
        }
        with self.assertRaisesRegex(ValueError, "This request id has different content"):
            native.reserve_tool_request(json.dumps(snapshot), "thread-a:")
        self.assertEqual(_missing_receipt_outcome("missing-request"), "unknown")

    def test_stub_literal_members_match_rust_exports(self):
        tree = ast.parse((Path(SERVER_SOURCE_ROOT) / "studio_operations_native.pyi").read_text())
        functions = {
            node.name: node for node in tree.body if isinstance(node, ast.FunctionDef)
        }
        reservation = functions["reserve_tool_request"]
        self.assertEqual([arg.arg for arg in reservation.args.args], ["snapshot_json", "alias_prefix"])
        self.assertEqual(ast.unparse(reservation.returns), "ReservationResult")
        aliases = {
            target.id: node.value
            for node in tree.body if isinstance(node, ast.Assign)
            for target in node.targets if isinstance(target, ast.Name)
        }
        contracts = {
            "EventName": ("EVENT_NAMES", native.EVENT_NAMES),
            "ProofName": ("PROOF_NAMES", native.PROOF_NAMES),
            "ResultKind": ("RESULT_NAMES", native.RESULT_NAMES),
            "ReservationKind": (None, ("Admitted", "ReturnExisting")),
            "PersistedStage": ("PERSISTED_STAGES", native.PERSISTED_STAGES),
            "PersistedOutcome": ("PERSISTED_OUTCOMES", native.PERSISTED_OUTCOMES),
        }
        for alias, (export, expected) in contracts.items():
            with self.subTest(alias=alias):
                expression = aliases[alias]
                members = expression.slice.elts if isinstance(expression.slice, ast.Tuple) else [expression.slice]
                self.assertIsInstance(expression, ast.Subscript)
                self.assertEqual([ast.literal_eval(member) for member in members], list(expected), export)

    def test_all_event_and_proof_names_are_accepted_and_unknown_names_rejected(self):
        for event in native.EVENT_NAMES:
            with self.subTest(event=event):
                self.assertIn(_lifecycle(_record(), event).kind, native.RESULT_NAMES)
        for proof, record in (
            ("QueuedCancellation", _record("queued", "pending")),
            ("ReadOnlyRejection", _record("running", "pending")),
            ("ExactPreWriteRejection", _record("running", "pending")),
            ("ReviewChildAbsent", _record("running", "pending")),
        ):
            with self.subTest(proof=proof):
                if proof == "ReviewChildAbsent":
                    _apply_decision(record, _lifecycle(record, "Failure"))
                decision = _lifecycle(record, "DefinitiveRejection", proof=proof)
                self.assertEqual(decision.kind, "Transitioned")
                self.assertTrue(_apply_decision(record, decision))
                self.assertEqual(record["outcome"], "not_applied")
        with self.assertRaisesRegex(ValueError, "Unknown tool request event"):
            _lifecycle(_record(), "NotAnEvent")
        with self.assertRaisesRegex(ValueError, "Unknown tool request rejection proof"):
            _lifecycle(_record(), "Failure", proof="NotAProof")

    def test_malformed_arguments_and_record_fields_fail_closed(self):
        with self.assertRaisesRegex(ValueError, "Invalid stored tool request record"):
            native.transition_record("{", "Cancel", b"\0" * 32, empty=True)
        with self.assertRaisesRegex(ValueError, "fingerprint must contain 32 bytes"):
            native.transition_record("{}", "Cancel", b"bad", empty=True)
        with self.assertRaises(TypeError):
            native.transition_record("{}", "Cancel", 7, empty=True)
        for field, value in (("cancelRequested", 1), ("epoch", "4"),
                             ("signature", 1), ("agent", 1),
                             ("supervisor", {"generation": "5"})):
            with self.subTest(field=field):
                record = _record()
                record[field] = value
                with self.assertRaises(ValueError):
                    _lifecycle(record, "ServerRestarted")
        for missing in ("stage", "outcome"):
            with self.subTest(missing=missing):
                record = _record()
                del record[missing]
                with self.assertRaisesRegex(ValueError, "Unsupported stored tool request lifecycle"):
                    _lifecycle(record, "ServerRestarted")

    def test_recovery_logs_and_leaves_malformed_record_unapplied(self):
        import logging
        import sqlite3
        from codex_payloads import resolve_record, state_root

        class Runtime(RequestMixin):
            def __init__(self):
                self.writes = []

            def put(self, db, kind, record):
                self.writes.append(record)

        db = sqlite3.connect(":memory:")
        runtime = Runtime()
        record = _record()
        record["cancelRequested"] = 1
        db.execute("CREATE TABLE runtime_tool_requests (id TEXT PRIMARY KEY, record TEXT NOT NULL)")
        db.execute("INSERT INTO runtime_tool_requests VALUES (?, ?)", (record["id"], json.dumps(record)))
        with patch("codex_payloads.resolve_record", side_effect=lambda _root, value: value), \
             patch("codex_payloads.state_root", return_value="isolated"), \
             self.assertLogs("codex_tool_requests", level=logging.WARNING) as logs:
            runtime.setup_tool_requests(db)
        self.assertIn(record["id"], logs.output[0])
        self.assertEqual(runtime.writes, [])
        self.assertEqual(db.execute("SELECT record FROM runtime_tool_requests").fetchone()[0], json.dumps(record))


if __name__ == "__main__":
    unittest.main()
