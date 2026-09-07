#!/usr/bin/env python3
"""Question history, exact answer receipts and deferral without live model calls."""
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("runtime_contract", Path(__file__).with_name("runtime-contract.py"))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
Runtime, FakeServer, eventually = fixture.Runtime, fixture.FakeServer, fixture.eventually


class QuestionHistoryContract(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        self.runtime = Runtime(self.root, FakeServer)
        self.lead = self.runtime.create({"name": "Lead", "cwd": str(self.root), "prompt": "Coordinate"})
        eventually(lambda: self.runtime.agent(self.lead["id"])["status"] == "running")
        self.lead = self.runtime.agent(self.lead["id"])

    def tearDown(self):
        self.runtime.close()
        self.tmp.cleanup()

    def question(self, **extra):
        self.runtime.request({"id": 102, "method": "item/tool/requestUserInput", "params": {
            "threadId": self.lead["threadId"], "questions": [{"id": "q", "question": "Which file?", **extra}]}})
        return self.runtime.snapshot()["requests"][-1]

    def history(self):
        return self.runtime.question_history(self.lead["id"])["items"]

    def test_deferral_preserves_pending_rpc_and_suppresses_inbox(self):
        request = self.question()
        responses = len(self.runtime.server.responses)
        first = self.runtime.defer_question(request["id"])
        self.assertEqual(first, self.runtime.defer_question(request["id"]))
        self.assertEqual(first["status"], "pending")
        self.assertEqual(len(self.runtime.server.responses), responses)
        self.assertEqual(self.runtime.agent(self.lead["id"])["status"], "approval")
        self.assertFalse(any(row["kind"] == "request" for row in self.runtime.workspace_snapshot(self.lead["id"])["inbox"]))
        self.assertTrue(self.history()[0]["deferred"])
        self.assertEqual(self.history()[0]["deferredBy"], "user")
        self.runtime.defer_question(request["id"], False)
        self.assertTrue(any(row["kind"] == "request" for row in self.runtime.workspace_snapshot(self.lead["id"])["inbox"]))

    def test_history_and_exact_answer_retry_survive_restart(self):
        request = self.question()
        body = {"answers": {"q": {"answers": ["native.rs"]}}}
        self.runtime.answer(request["id"], body)
        responses = len(self.runtime.server.responses)
        self.assertTrue(self.runtime.answer(request["id"], body)["replayed"])
        self.assertEqual(len(self.runtime.server.responses), responses)
        with self.assertRaisesRegex(ValueError, "different answer"):
            self.runtime.answer(request["id"], {"answers": {"q": {"answers": ["other.rs"]}}})
        row = self.history()[0]
        self.assertEqual(row["answerHistory"][0]["answer"], ["native.rs"])
        self.assertEqual(row["answerHistory"][0]["question"], "Which file?")
        self.assertEqual(row["answeredBy"], "user")
        self.assertGreaterEqual(row["answeredAt"], row["createdAt"])
        self.runtime.close()
        self.runtime = Runtime(self.root, FakeServer)
        self.assertEqual(self.history()[0], row)
        self.assertTrue(self.runtime.answer(request["id"], body)["replayed"])

    def test_secret_answer_reaches_codex_but_never_history(self):
        request = self.question(isSecret=True)
        body = {"answers": {"q": {"answers": ["secret-example-4938"]}}}
        self.runtime.answer(request["id"], body)
        self.assertEqual(self.runtime.server.responses[-1]["result"], body)
        self.assertNotIn("secret-example-4938", json.dumps(self.history()))
        with self.runtime.db() as db:
            stored = db.execute("SELECT record FROM runtime_requests WHERE id=?", (request["id"],)).fetchone()[0]
        self.assertNotIn("secret-example-4938", stored)

    def test_lost_write_cannot_be_repeated_and_keeps_uncertainty(self):
        request = self.question()
        body = {"answers": {"q": {"answers": ["native.rs"]}}}
        original = self.runtime.server.write
        def partial_write(value):
            original(value)
            raise OSError("pipe closed after write")
        self.runtime.server.write = partial_write
        with self.assertRaises(OSError):
            self.runtime.answer(request["id"], body)
        responses = len(self.runtime.server.responses)
        with self.assertRaisesRegex(ValueError, "uncertain"):
            self.runtime.answer(request["id"], body)
        self.assertEqual(len(self.runtime.server.responses), responses)
        self.assertEqual(self.history()[0]["status"], "uncertain")
        self.assertEqual(self.history()[0]["answerHistory"][0]["answer"], ["native.rs"])
        self.runtime.close()
        self.runtime = Runtime(self.root, FakeServer)
        self.assertEqual(self.history()[0]["status"], "uncertain")

    def test_process_loss_after_answer_commit_stays_uncertain(self):
        request = self.question()
        body = {"answers": {"q": {"answers": ["native.rs"]}}}
        class ProcessLost(BaseException):
            pass
        def interrupted_write(value):
            raise ProcessLost()
        self.runtime.server.write = interrupted_write
        with self.assertRaises(ProcessLost):
            self.runtime.answer(request["id"], body)
        self.assertEqual(self.history()[0]["status"], "answering")
        self.runtime.close()
        self.runtime = Runtime(self.root, FakeServer)
        self.assertEqual(self.history()[0]["status"], "uncertain")
        with self.assertRaisesRegex(ValueError, "uncertain"):
            self.runtime.answer(request["id"], body)

    def test_scope_and_approval_boundary(self):
        self.question()
        other = self.runtime.new_lead({})
        self.assertEqual(self.runtime.question_history(other["id"])["items"], [])
        with self.assertRaises(ValueError):
            self.runtime.question_history(None)
        self.runtime.request({"id": 103, "method": "item/commandExecution/requestApproval", "params": {"threadId": self.lead["threadId"]}})
        approval = next(r for r in self.runtime.snapshot()["requests"] if r["method"].endswith("requestApproval"))
        with self.assertRaisesRegex(ValueError, "Only a question"):
            self.runtime.defer_question(approval["id"])
        self.assertEqual(len(self.history()), 1)

    def test_mcp_secret_field_redacted_and_decline_recorded(self):
        self.runtime.request({"id": 104, "method": "mcpServer/elicitation/request", "params": {
            "threadId": self.lead["threadId"], "mode": "form", "requestedSchema": {"properties": {
                "token": {"type": "string", "title": "Access token", "format": "password"}}}}})
        request = self.runtime.snapshot()["requests"][-1]
        self.runtime.answer(request["id"], {"decision": "accept", "content": {"token": "another-secret"}})
        self.assertNotIn("another-secret", json.dumps(self.history()))
        self.assertEqual(self.history()[0]["answerHistory"][0]["answer"], "[redacted]")

    def test_async_answer_creates_only_one_durable_user_event(self):
        self.runtime.notification({"method": "item/completed", "params": {"threadId": self.lead["threadId"],
            "item": {"id": "async-choice", "type": "agentMessage", "text": "Choose", "questions": [{"title": "Scope?", "options": ["One", "All"]}]}}})
        request = self.runtime.snapshot()["requests"][-1]
        self.runtime.defer_question(request["id"])
        body = {"answers": {"0": {"answers": ["One"]}}}
        self.runtime.answer(request["id"], body)
        self.runtime.answer(request["id"], body)
        with self.runtime.db() as db:
            count = db.execute("SELECT count(*) FROM runtime_events WHERE id=?", (request["id"] + ":answer",)).fetchone()[0]
        self.assertEqual(count, 1)
        self.assertFalse(self.history()[0]["deferred"])


if __name__ == "__main__":
    unittest.main()
