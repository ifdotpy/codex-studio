#!/usr/bin/env python3
"""User-action ownership, review, retries, and durable wake contracts."""

import importlib.util
import json
from pathlib import Path
from concurrent.futures import ThreadPoolExecutor
import unittest
import uuid
import threading
import urllib.request
import urllib.error

spec = importlib.util.spec_from_file_location(
    "workspace_fixture", Path(__file__).with_name("workspace-contract.py")
)
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)


class UserTasksContract(unittest.TestCase):
    setUp = f.WorkspaceContract.setUp
    tearDown = f.WorkspaceContract.tearDown
    lead = f.WorkspaceContract.lead
    worker = f.WorkspaceContract.worker
    agent_update = f.WorkspaceContract.agent_update
    start = f.WorkspaceContract.start
    events = f.WorkspaceContract.events
    tool = f.WorkspaceContract.tool

    def create(self, agent, **data):
        return self.runtime.user_task_action(
            agent["id"],
            {
                "action": "create",
                "title": "Connect the test account",
                "criteria": "The account appears in settings",
                **data,
            },
        )

    def complete(self, task, **data):
        return self.runtime.complete_user_task(
            {
                "id": str(uuid.uuid4()),
                "task_id": task["id"],
                "version": task["version"],
                **data,
            }
        )

    def action(self, actor, task, action, **data):
        return self.runtime.user_task_action(
            actor["id"],
            {
                "action": action,
                "task_id": task["id"],
                "version": task["version"],
                **data,
            },
        )

    def test_check_wakes_finished_owner_and_dispatches_review(self):
        a = self.start(self.lead())
        task = self.create(a)
        self.runtime.server.complete(a["threadId"], a["turnId"])
        self.assertFalse(self.runtime.agent(a["id"])["inFlight"])
        checked = self.complete(task, note="Test account is visible")
        self.assertEqual(
            (checked["status"], checked["delivery"]), ("review", "pending")
        )
        self.runtime.dispatch()
        f.eventually(lambda: self.runtime.agent(a["id"]).get("inFlight"))
        f.eventually(
            lambda: len([c for c in self.runtime.server.calls if c[0] == "turn/start"])
            == 2
        )
        params = [p for m, p in self.runtime.server.calls if m == "turn/start"][-1]
        self.assertIn(task["id"], json.dumps(params))
        self.assertIn("Test account is visible", json.dumps(params))
        self.assertIn("accept or return", json.dumps(params))

    def test_return_second_check_accept_and_history(self):
        a = self.lead()
        task = self.create(a)
        review = self.complete(task)
        with self.assertRaises(ValueError):
            self.action(a, review, "return", reason="")
        returned = self.action(
            a, review, "return", reason="Connect the sandbox account, not production"
        )
        self.assertEqual(returned["status"], "open")
        again = self.complete(returned, note="Sandbox now visible")
        accepted = self.action(a, again, "accept", reason="Verified sandbox account")
        self.assertEqual(accepted["status"], "accepted")
        self.assertEqual(
            [h["action"] for h in accepted["history"]],
            ["create", "complete", "return", "complete", "accept"],
        )
        self.assertEqual(len(self.events(a, "user_task_completed")), 2)
        reopened = self.action(
            a, accepted, "return", reason="The account was disconnected. Reconnect it"
        )
        self.assertEqual(reopened["status"], "open")

    def test_only_owner_and_lead_manage_tasks(self):
        lead = self.lead()
        owner = self.worker(lead)
        peer = self.worker(lead)
        other = self.lead("Other")
        task = self.create(owner)
        for actor in [peer, other]:
            with self.assertRaises(ValueError):
                self.action(actor, task, "cancel", reason="No longer needed")
        self.assertEqual(len(self.runtime.user_tasks(peer["id"])["items"]), 1)
        self.assertEqual(self.runtime.user_tasks(other["id"])["items"], [])
        reviewed = self.complete(task)
        accepted = self.action(
            lead, reviewed, "accept", reason="Lead verified the result"
        )
        self.assertEqual(accepted["agent"], owner["id"])
        self.assertEqual(len(self.events(owner, "user_task_completed")), 1)
        self.assertEqual(len(self.events(lead, "user_task_completed")), 0)

    def test_concurrent_retry_commits_one_event(self):
        a = self.lead()
        task = self.create(a)
        request = {
            "id": str(uuid.uuid4()),
            "task_id": task["id"],
            "version": 1,
            "note": "Done",
        }
        with ThreadPoolExecutor(max_workers=8) as pool:
            values = list(
                pool.map(lambda _: self.runtime.complete_user_task(request), range(12))
            )
        self.assertTrue(all(v == values[0] for v in values))
        self.assertEqual(len(self.events(a, "user_task_completed")), 1)
        with self.assertRaises(ValueError):
            self.runtime.complete_user_task({**request, "note": "Changed"})
        with self.assertRaises(ValueError):
            self.complete(task)
        self.assertEqual(len(self.runtime.user_tasks()["items"][0]["history"]), 2)

    def test_agent_retry_and_stale_decisions(self):
        a = self.lead()
        request = {
            "action": "create",
            "title": "Upload sample",
            "criteria": "A file is attached",
        }
        key = str(uuid.uuid4())
        task = self.runtime.user_task_action(a["id"], request, key)
        self.assertEqual(self.runtime.user_task_action(a["id"], request, key), task)
        with self.assertRaises(ValueError):
            self.runtime.user_task_action(a["id"], {**request, "title": "Other"}, key)
        with self.assertRaises(ValueError):
            self.action(a, task, "accept", reason="Too early")
        review = self.complete(task)
        with self.assertRaises(ValueError):
            self.action(a, task, "accept", reason="Old version")
        with self.assertRaises(ValueError):
            self.action(a, review, "update", title="Changed instructions")
        returned = self.action(a, review, "return", reason="Upload a different sample")
        with self.assertRaises(ValueError):
            self.action(a, review, "accept", reason="Stale approval")
        updated = self.action(a, returned, "update", criteria="A PNG file is attached")
        self.assertEqual(updated["criteria"], "A PNG file is attached")

    def test_stopped_owner_stays_stopped_and_resume_reads_review(self):
        a = self.lead()
        task = self.create(a)
        self.runtime.stop(a["id"])
        review = self.complete(task)
        self.assertEqual(review["delivery"], "cancelled")
        self.runtime.dispatch()
        self.assertFalse(self.runtime.agent(a["id"])["autoWake"])
        self.assertFalse(self.runtime.agent(a["id"])["inFlight"])
        with self.assertRaises(ValueError):
            self.action(a, review, "accept", reason="Old caller")
        self.start(a, "Resume and review the account")
        params = [p for m, p in self.runtime.server.calls if m == "turn/start"][-1]
        self.assertIn(task["id"], json.dumps(params))
        self.assertIn("User tasks awaiting your review", json.dumps(params))

    def test_restart_keeps_tasks_receipts_and_pending_event(self):
        a = self.lead()
        task = self.create(a)
        request = {"id": str(uuid.uuid4()), "task_id": task["id"], "version": 1}
        result = self.runtime.complete_user_task(request)
        self.runtime.close()
        self.runtime = f.ControlledRuntime(self.state, f.WorkspaceServer)
        self.assertEqual(self.runtime.complete_user_task(request), result)
        self.assertEqual(self.runtime.user_tasks()["items"][0]["status"], "review")
        self.assertEqual(len(self.events(a, "user_task_completed")), 1)
        self.runtime.dispatch()
        f.eventually(lambda: self.runtime.agent(a["id"]).get("inFlight"))

    def test_deleted_owner_hidden_and_old_epoch_rejected(self):
        a = self.lead()
        task = self.create(a)
        self.runtime.stop(a["id"])
        self.agent_update(a, autoWake=True)
        with self.assertRaises(ValueError):
            self.runtime.user_task_action(
                a["id"],
                {"action": "create", "title": "Stale", "criteria": "No"},
                epoch=a["epoch"],
            )
        self.runtime.delete_conversation(a["id"])
        self.assertEqual(self.runtime.user_tasks()["items"], [])
        with self.assertRaises(ValueError):
            self.complete(task)

    def test_native_tool_and_legacy_workspace_fallback(self):
        a = self.lead()
        result = self.tool(
            a,
            "orchestration_user_task",
            {
                "action": "create",
                "title": "Connect account",
                "criteria": "Account visible",
            },
        )
        self.assertTrue(result["success"])
        second = self.tool(
            a,
            "orchestration_send",
            {
                "agent_id": "workspace",
                "text": json.dumps(
                    {"tool": "orchestration_user_task", "arguments": {"action": "list"}}
                ),
            },
        )
        self.assertTrue(second["success"])
        self.assertIn("Connect account", json.dumps(second))
        schemas = self.tool(a, "orchestration_status", {})
        self.assertIn("orchestration_user_task", json.dumps(schemas))

    def test_http_completion_requires_token_and_preserves_agent_control(self):
        from codex_canvas import Canvas, make_server

        canvas = Canvas(self.state)
        canvas.runtime = self.runtime
        server = make_server(canvas)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        url = f"http://127.0.0.1:{server.server_port}"

        def request(path, body=None, headers=None):
            req = urllib.request.Request(
                url + path,
                data=json.dumps(body).encode() if body is not None else None,
                headers=headers or {},
            )
            with urllib.request.urlopen(req, timeout=5) as response:
                return json.load(response)

        try:
            a = self.lead()
            task = self.create(a)
            self.assertEqual(
                request("/api/user-tasks?agent=" + a["id"])["items"][0]["id"],
                task["id"],
            )
            token = request("/api/state")["token"]
            body = {
                "id": str(uuid.uuid4()),
                "task_id": task["id"],
                "version": 1,
                "status": "accepted",
                "agent": "fake",
                "note": "Verified",
            }
            with self.assertRaises(urllib.error.HTTPError) as denied:
                request("/api/user-tasks/complete", body)
            self.assertEqual(denied.exception.code, 403)
            headers = {"X-Canvas-Token": token, "Content-Type": "application/json"}
            with self.assertRaises(urllib.error.HTTPError) as cross:
                request(
                    "/api/user-tasks/complete",
                    body,
                    {**headers, "Origin": "https://outside.invalid"},
                )
            self.assertEqual(cross.exception.code, 403)
            result = request("/api/user-tasks/complete", body, headers)
            self.assertEqual((result["agent"], result["status"]), (a["id"], "review"))
            self.assertEqual(request("/api/user-tasks/complete", body, headers), result)
            with self.assertRaises(urllib.error.HTTPError) as missing:
                request("/api/user-tasks", {"action": "accept"}, headers)
            self.assertEqual(missing.exception.code, 404)
        finally:
            server.shutdown()
            server.server_close()
            thread.join()

    def test_inbox_and_snapshot_show_only_open_tasks(self):
        a = self.lead()
        task = self.create(a)
        self.assertEqual(self.runtime.snapshot()["userTasks"][0]["id"], task["id"])
        self.assertTrue(
            any(
                i["id"] == task["id"]
                for i in self.runtime.workspace_snapshot()["inbox"]
            )
        )
        self.complete(task)
        self.assertFalse(
            any(
                i["id"] == task["id"]
                for i in self.runtime.workspace_snapshot()["inbox"]
            )
        )


if __name__ == "__main__":
    unittest.main(verbosity=2)
