#!/usr/bin/env python3
"""Role injection, user-contact boundaries, and unchanged native permissions."""
import importlib.util
import json
from pathlib import Path
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("role_fixture", Path(__file__).with_name("workspace-contract.py"))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)


class RoleSkillsContract(unittest.TestCase):
    setUp = f.WorkspaceContract.setUp
    tearDown = f.WorkspaceContract.tearDown
    lead = f.WorkspaceContract.lead
    worker = f.WorkspaceContract.worker
    agent_update = f.WorkspaceContract.agent_update
    start = f.WorkspaceContract.start
    tool = f.WorkspaceContract.tool
    events = f.WorkspaceContract.events

    def test_server_identity_selects_skill_and_visible_tools(self):
        lead = self.lead()
        worker = self.worker(lead, role="orchestrator")
        for actor, name, forbidden in [(lead, "codex-orchestrator", "# Codex Studio subagent"),
                                       (worker, "codex-subagent", "# Codex Studio orchestrator")]:
            params = self.runtime.new_thread_params(actor)
            self.assertIn("[Studio role skill: " + name + "]", params["developerInstructions"])
            self.assertNotIn(forbidden, params["developerInstructions"])
            advertised = {d["name"] for d in params["dynamicTools"]}
            context = self.runtime.model_context(actor["id"], {"topic": "tools"})
            self.assertEqual(advertised, {d["name"] for d in context["content"]})
            self.assertEqual("orchestration_user_task" in advertised, actor["isLead"])
            self.assertEqual("orchestration_speak" in advertised, actor["isLead"])
            self.assertIn("orchestration_complaint", advertised)
        self.assertIn("orchestration_user_task", {d["name"] for d in self.runtime.tool_definitions()})

    def test_real_start_and_resume_receive_role_skill(self):
        lead = self.start(self.lead())
        worker = self.start(self.worker(lead))
        starts = [p for method, p in self.runtime.server.calls if method == "thread/start"]
        self.assertIn("[Studio role skill: codex-orchestrator]", starts[0]["developerInstructions"])
        self.assertIn("[Studio role skill: codex-subagent]", starts[1]["developerInstructions"])
        self.assertIn("orchestration_user_task", {d["name"] for d in starts[0]["dynamicTools"]})
        self.assertNotIn("orchestration_user_task", {d["name"] for d in starts[1]["dynamicTools"]})
        self.assertNotIn("orchestration_speak", {d["name"] for d in starts[1]["dynamicTools"]})
        turns = [p for method, p in self.runtime.server.calls if method == "turn/start"]
        for actor, role in [(lead, "codex-orchestrator"), (worker, "codex-subagent")]:
            params = next(p for p in turns if p["threadId"] == actor["threadId"])
            self.assertIn("[Studio role skill: " + role + "]", json.dumps(params["input"]))
        self.runtime.server.complete(worker["threadId"], worker["turnId"])
        self.runtime.loaded.discard(worker["id"])
        self.runtime.prepare(self.runtime.agent(worker["id"]))
        resumes = [p for method, p in self.runtime.server.calls if method == "thread/resume"]
        self.assertEqual(resumes[-1]["threadId"], worker["threadId"])
        self.assertIn("[Studio role skill: codex-subagent]", resumes[-1]["developerInstructions"])

    def test_existing_thread_updates_and_compaction_use_delivery_manifest(self):
        actor = self.worker(self.lead(), threadId="existing-native-thread")
        def context(event, delivered=False, **changes):
            with self.runtime.lock, self.runtime.db() as db:
                current = {**self.runtime.agent(actor["id"], db), **changes}
                self.runtime.enqueue(db, current, "user", "Continue", event)
                text = self.runtime.model_turn_context(db, current, event)
                if delivered:
                    db.execute("UPDATE runtime_events SET status='delivered' WHERE id=?", (event,))
                return text
        first = context("uncertain")
        self.assertIn("[Studio role skill: codex-subagent]", first)
        self.assertIn("[Studio role skill: codex-subagent]", context("delivered", True))
        self.assertNotIn("[Studio role skill:", context("unchanged", True))
        guidance = self.runtime.role_guidance(actor)
        with patch.object(self.runtime, "role_guidance", return_value=guidance + "\nNew role instruction"):
            self.assertIn("New role instruction", context("updated", True))
            self.assertNotIn("New role instruction", context("same-update", True))
            self.assertIn("New role instruction", context("compacted", compactions=1))

    def test_worker_request_stays_with_lead_until_explicit_forward(self):
        lead = self.agent_update(self.lead(), autoWake=True)
        worker = self.worker(lead)
        request = self.runtime.complaint(worker["id"], {"action": "submit", "text": "Need a decision", "recipient": "user"}, "request")
        self.assertEqual(request["recipient"], "lead")
        self.assertFalse(any(c["recipient"] == "user" for c in self.runtime.snapshot()["complaints"]))
        with self.assertRaisesRegex(ValueError, "Unknown managed agent"):
            self.runtime.chat_message(worker["id"], "user", "Bypass lead", "bypass")
        forwarded = self.runtime.complaint(lead["id"], {"action": "submit", "text": "Please decide the scope"}, "forward")
        self.assertEqual(forwarded["recipient"], "user")
        self.assertEqual(forwarded["author"], lead["id"])
        with self.assertRaisesRegex(ValueError, "assigned to the user"):
            self.runtime.complaint(lead["id"], {"action": "respond", "complaint_id": forwarded["id"], "text": "I decide", "status": "resolved"}, "self-answer")

    def test_native_worker_question_rejected_but_permission_preserved(self):
        lead = self.start(self.lead())
        worker = self.start(self.worker(lead))
        self.runtime.request({"id": "worker-question", "method": "item/tool/requestUserInput", "params": {
            "threadId": worker["threadId"], "questions": [{"id": "q", "question": "Choose scope"}]}})
        reply = self.runtime.server.responses[-1]
        self.assertEqual(reply["id"], "worker-question")
        self.assertIn("orchestration_message target=lead", reply["error"]["message"])
        self.assertFalse(self.runtime.snapshot()["requests"])
        self.runtime.request({"id": "permission", "method": "item/commandExecution/requestApproval", "params": {
            "threadId": worker["threadId"], "itemId": "cmd", "command": "restricted-command"}})
        request = self.runtime.snapshot()["requests"][-1]
        self.assertEqual(request["rpcId"], "permission")
        self.assertEqual(request["status"], "pending")
        self.runtime.request({"id": "lead-question", "method": "item/tool/requestUserInput", "params": {
            "threadId": lead["threadId"], "questions": [{"id": "q", "question": "Choose scope"}]}})
        self.assertTrue(any(r["rpcId"] == "lead-question" for r in self.runtime.snapshot()["requests"]))

    def test_structured_worker_question_routes_once_to_lead(self):
        lead = self.start(self.lead())
        worker = self.start(self.worker(lead))
        message = {"method": "item/completed", "params": {"threadId": worker["threadId"], "turnId": worker["turnId"],
            "item": {"id": "structured-question", "type": "agentMessage", "text": "I need the scope.",
                     "questions": [{"title": "Which scope? " + "Full context " * 1200, "options": ["One", "Last complete option"]}]}}}
        self.runtime.notification(message)
        self.runtime.notification(message)
        self.assertFalse(self.runtime.snapshot()["requests"])
        records = self.runtime.snapshot()["complaints"]
        self.assertEqual(len(records), 1)
        self.assertEqual(records[0]["recipient"], "lead")
        self.assertEqual(records[0]["author"], worker["id"])
        detail = self.runtime.complaint_detail(records[0]["id"])
        self.assertGreater(len(detail["text"]), 12000)
        questions = json.loads(detail["text"].split("\n", 1)[1])
        self.assertEqual(questions[0]["options"][-1]["label"], "Last complete option")
        self.assertEqual(len(self.events(lead, "complaint")), 1)
        message["params"]["threadId"] = lead["threadId"]
        message["params"]["turnId"] = lead["turnId"]
        self.runtime.notification(message)
        request = self.runtime.snapshot()["requests"][-1]
        self.assertEqual(request["agent"], lead["id"])
        self.assertEqual(request["method"], "agent/asyncQuestion")

    def test_legacy_worker_question_cannot_send_a_new_answer(self):
        worker = self.worker(self.lead())
        record = {"id": "old-question", "method": "agent/asyncQuestion", "agent": worker["id"],
                  "epoch": worker["epoch"], "status": "pending", "params": {"questions": [{"id": "q", "question": "Choose scope"}]}}
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.put(db, "requests", record)
        answer = {"answers": {"q": {"answers": ["One file"]}}}
        with self.assertRaisesRegex(ValueError, "Only the orchestrator can ask"):
            self.runtime.answer(record["id"], answer)
        self.assertFalse(any("One file" in event["text"] for event in self.events(worker)))
        from codex_questions import record_answer
        record_answer(record, answer)
        record["status"] = "answered"
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.put(db, "requests", record)
        self.assertTrue(self.runtime.answer(record["id"], answer)["replayed"])
        record["status"] = "uncertain"
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.put(db, "requests", record)
        with self.assertRaisesRegex(ValueError, "uncertain"):
            self.runtime.answer(record["id"], answer)

    def test_missing_role_skill_fails_visibly(self):
        lead = self.lead()
        with patch.object(Path, "read_text", side_effect=FileNotFoundError("missing")):
            with self.assertRaisesRegex(ValueError, "role skill codex-orchestrator is missing"):
                self.runtime.role_guidance(lead)
        with patch.object(Path, "read_text", return_value=""):
            with self.assertRaisesRegex(ValueError, "role skill codex-orchestrator is empty"):
                self.runtime.role_guidance(lead)


if __name__ == "__main__":
    unittest.main()
