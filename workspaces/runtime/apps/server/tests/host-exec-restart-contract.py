#!/usr/bin/env python3
"""A host_exec call that a Studio restart interrupts ends with its real guest result, once."""

# Support direct execution without runner-provided PYTHONPATH.
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import copy
import importlib.util
import json
import time
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
from codex_layout import SERVER_TESTS_ROOT
import codex_host_exec
from codex_host_exec import RESUME_ATTEMPTS, resume, resume_tick, run_tool
from codex_linux_vm import LinuxVMError
from codex_tool_requests import RequestMixin
from codex_tool_response_recovery import recover

_spec = importlib.util.spec_from_file_location("tool_response_recovery_contract",
                                               SERVER_TESTS_ROOT / "tool-response-recovery-contract.py")
_fixture = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_fixture)


class BackendStopped(BaseException):
    """The old Studio process ends inside the guest wait; no code after it runs."""


class Guest:
    """The guest service contract for host.exec: one durable receipt per request ID. The same ID
    with the same parameters returns the saved answer; it never starts the command again."""

    def __init__(self, state_dir):
        self.state_dir = state_dir
        self.receipts = {}
        self.requests = []
        self.runs = 0
        self.reachable = True

    def stream(self, method, params, *, request_id, timeout):
        assert method == "host.exec" and timeout > 0
        self.requests.append((request_id, copy.deepcopy(params)))
        if not self.reachable:
            raise LinuxVMError("The Linux VM connection closed before its response.", uncertain=True)
        saved = self.receipts.get(request_id)
        if saved is None:
            self.runs += 1
            self.receipts[request_id] = {"params": copy.deepcopy(params), "answer": None}
            raise BackendStopped()
        if saved["params"] != params:
            raise LinuxVMError("Linux guest host.exec: id_conflict", code="id_conflict")
        answer = saved["answer"]
        if answer is None:
            raise LinuxVMError("The guest command still runs.", uncertain=True)
        if "error" in answer:
            raise LinuxVMError("Linux guest host.exec: " + json.dumps(answer["error"]),
                               code=answer["error"]["code"])
        yield {"id": request_id, "result": answer["result"]}


class InterruptedHostExec(unittest.TestCase):
    # The saved-response fixture: its runtime surface, without its own tests.
    _Saved = _fixture.SavedResponseRecovery
    agent_connection, server_for = _Saved.agent_connection, _Saved.server_for
    db, read_db, agent, connection_current = _Saved.db, _Saved.read_db, _Saved.agent, _Saved.connection_current
    reply, put, save = _Saved.reply, _Saved.put, _Saved.save
    del _Saved
    tool_request = RequestMixin.tool_request
    tool_result = RequestMixin.tool_result
    finish_tool_request = RequestMixin.finish_tool_request
    setup_tool_requests = RequestMixin.setup_tool_requests

    def setUp(self):
        _fixture.SavedResponseRecovery.setUp(self)
        self.key = "exact-request"
        self.agent_record.update(executionMode="vm", cwd="/line", activeTools=[
            {"id": "toolu_native", "type": "mcpToolCall", "name": "mcp__studio__host_exec"}])
        self.record = {"id": self.key, "agent": "lead", "threadId": "thread", "turnId": "turn", "epoch": 0,
                       "accountKey": "default", "connectionId": "old-backend", "callId": "call",
                       "rpcId": self.rpc, "tool": "host_exec", "signature": "signature", "readOnly": False,
                       "stage": "running", "outcome": "pending", "cancelRequested": False,
                       "created": 10, "updated": 10}
        self.save()
        with self.db() as db:
            db.execute("CREATE TABLE runtime_tool_results(id TEXT PRIMARY KEY, result TEXT)")
        self.guest = Guest(self.root)
        patch("codex_linux_workspaces.client", return_value=self.guest).start()
        patch("codex_host_exec.ensure_service").start()

    def start_and_restart(self):
        """The call reaches the guest, Studio stops, and the next Studio marks the receipt."""
        actor = copy.deepcopy(self.agent_record)
        with self.assertRaises(BackendStopped):
            run_tool(self, actor, {"action": "execute", "command": "make", "request_id": "build"}, self.key)
        with self.db() as db:
            self.setup_tool_requests(db)
        self.assertEqual(self.receipt()["stage"], "interrupted")

    def receipt(self):
        with self.db() as db:
            return self.tool_request(self.key, db)

    def finish_guest(self, answer):
        (request_id, _), = self.guest.requests[:1]
        self.guest.receipts[request_id]["answer"] = answer

    def deliver(self):
        # The saved answer goes to the waiter only after the normal response had 15 s.
        with patch("codex_tool_response_recovery.time.time", return_value=time.time() + 60):
            return recover(self, "lead")

    def delivered_text(self):
        self.assertEqual(len(self.responses), 1)
        return "".join(item["text"] for item in self.responses[0]["result"]["contentItems"])

    def test_restart_returns_the_finished_guest_result_without_a_second_run(self):
        self.start_and_restart()
        # The placeholder waits: the saved guest request can still give the real result.
        self.deliver()
        self.assertEqual(self.responses, [])
        operation = self.guest.requests[0][1]["operationId"]
        self.finish_guest({"result": {"operationId": operation, "state": "exited", "exitCode": 0, "collected": True}})
        self.assertEqual(resume(self, self.key), "settled")
        self.assertEqual(self.receipt()["stage"], "completed")
        self.assertEqual(self.deliver()["status"], "tool_response_delivered")
        text = self.delivered_text()
        self.assertIn('"exitCode": 0', text)
        self.assertIn("nothing was repeated", text)
        self.assertIn(operation, text)
        # The resume sent the same guest request: one command run, the same identity and content.
        self.assertEqual(self.guest.runs, 1)
        self.assertEqual(self.guest.requests[1], self.guest.requests[0])
        self.deliver()
        self.assertEqual(len(self.responses), 1)

    def test_an_explicit_guest_error_reaches_the_waiter(self):
        self.start_and_restart()
        self.finish_guest({"error": {"code": "busy", "message": "All project slots are leased"}})
        self.assertEqual(resume(self, self.key), "settled")
        self.assertEqual(self.receipt()["stage"], "failed")
        self.deliver()
        self.assertIn("All project slots are leased", self.delivered_text())
        self.assertEqual(self.guest.runs, 1)

    def test_an_unreachable_guest_ends_with_the_unknown_outcome_after_bounded_attempts(self):
        self.start_and_restart()
        self.guest.reachable = False
        for _ in range(RESUME_ATTEMPTS - 1):
            self.assertEqual(resume(self, self.key), "postponed")
            self.deliver()
            self.assertEqual(self.responses, [])
        self.assertEqual(resume(self, self.key), "postponed")
        self.assertEqual(self.receipt()["hostExecResume"]["state"], "failed")
        self.deliver()
        text = self.delivered_text()
        self.assertIn('"outcome": "unknown"', text)
        self.assertIn("Nothing was repeated", text)
        self.assertEqual(self.guest.runs, 1)

    def test_the_timer_resumes_each_waiting_receipt_and_then_stops_scanning(self):
        self.start_and_restart()
        operation = self.guest.requests[0][1]["operationId"]
        self.finish_guest({"result": {"operationId": operation, "state": "exited", "exitCode": 3}})
        resume_tick(self)
        state = self.__dict__["_host_exec_resume"]
        for job in list(state["jobs"].values()):
            job.join(_fixture.HANG_GUARD_SECONDS)
        self.assertEqual(self.receipt()["stage"], "completed")
        with patch.object(codex_host_exec.time, "monotonic", return_value=time.monotonic() + 3600):
            resume_tick(self)
        self.assertTrue(state["done"])
        self.assertEqual(len(self.guest.requests), 2)

    def test_a_call_without_a_saved_guest_request_keeps_the_unknown_answer(self):
        # Receipts from before this change have no saved request: the waiter gets the answer.
        self.record.update(stage="interrupted", outcome="unknown", updated=time.time() - 60, result={
            "success": False, "contentItems": [{"type": "inputText", "text": "outcome unknown"}]})
        self.save()
        self.assertEqual(resume(self, self.key), "superseded")
        self.deliver()
        self.assertEqual(self.delivered_text(), "outcome unknown")


if __name__ == "__main__":
    unittest.main()
