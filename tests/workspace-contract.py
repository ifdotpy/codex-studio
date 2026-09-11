#!/usr/bin/env python3
"""Workspace behavior contracts. All state, Git repositories, and providers are isolated."""

import base64
from concurrent.futures import ThreadPoolExecutor
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from unittest.mock import patch
import uuid

sys.dont_write_bytecode = True
HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location(
    "runtime_contract_fixture", HERE / "runtime-contract.py"
)
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
Runtime, eventually = fixture.Runtime, fixture.eventually
from codex_native_errors import NativeRpcError
from codex_shell import monitor_command


class WorkspaceServer(fixture.FakeServer):
    def __init__(self, *args):
        super().__init__(*args)
        self.steer_error = None
        self.fork_error = None
        self.command_result = {
            "exitCode": 0,
            "stdout": '{"wakeAgent":false}\n',
            "stderr": "",
        }

    def call(self, method, params, timeout=60):
        if method in {
            "thread/fork",
            "turn/steer",
            "skills/list",
            "mcpServerStatus/list",
            "command/exec/write",
            "command/exec/resize",
        }:
            self.calls.append((method, params))
            if method == "turn/steer":
                if self.steer_error:
                    raise self.steer_error
                return {"turnId": params["expectedTurnId"]}
            if method == "thread/fork":
                if self.fork_error:
                    raise self.fork_error
                self.seq += 1
                return {"thread": {"id": f"fork-{self.seq}"}}
            if method == "skills/list":
                return {
                    "data": [
                        {
                            "cwd": params["cwds"][0],
                            "skills": [{"name": "fixture-skill"}],
                        }
                    ]
                }
            if method == "mcpServerStatus/list":
                return {
                    "data": [
                        {
                            "name": "fixture-mcp",
                            "tools": {"inspect": {"name": "inspect"}},
                        }
                    ]
                }
            return {}
        if method == "command/exec" and params["command"] == monitor_command(
            self, "fixture-rule-command", params["cwd"],
            config={"features": {"shell_snapshot": False}},
        ):
            self.calls.append((method, params))
            self.notify(
                {
                    "method": "command/exec/outputDelta",
                    "params": {
                        "processId": params["processId"],
                        "stream": "stdout",
                        "deltaBase64": base64.b64encode(
                            self.command_result["stdout"].encode()
                        ).decode(),
                    },
                }
            )
            return self.command_result
        return super().call(method, params, timeout)


class ControlledRuntime(Runtime):
    """Keep the real dispatch and rule methods, but let each test advance its clock."""

    def schedule(self):
        while not self.closed:
            self.changed.wait(0.05)
            self.changed.clear()


class WorkspaceContract(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="codex-workspace-contract-")
        self.root = Path(self.tmp.name)
        self.project = self.root / "project"
        self.project.mkdir()
        self.state = self.root / "state"
        self.environment = patch.dict(
            os.environ, {"CODEX_BOARD_STATE_DIR": str(self.root / "board")}
        )
        self.environment.start()
        self.runtime = ControlledRuntime(self.state, WorkspaceServer)

    def tearDown(self):
        self.runtime.close()
        self.environment.stop()
        self.tmp.cleanup()

    def lead(self, name="Lead", cwd=None):
        return self.runtime.create(
            {"name": name, "cwd": str(cwd or self.project), "prompt": ""},
            draft=True,
            defer=True,
        )

    def worker(self, lead, name="Worker", **changes):
        worker = self.runtime.create(
            {"name": name, "role": "reviewer", "prompt": "Inspect"},
            lead["id"],
            defer=True,
        )
        return self.agent_update(worker, status="idle", autoWake=True, **changes)

    def agent_update(self, agent, **changes):
        with self.runtime.lock, self.runtime.db() as db:
            current = self.runtime.agent(agent["id"], db)
            current.update(changes)
            self.runtime.put(db, "agents", current)
        return current

    def start(self, agent, text="Start work", assets=None):
        self.runtime.send(agent["id"], text, assets=assets)
        self.runtime.dispatch()
        eventually(lambda: self.runtime.agent(agent["id"]).get("turnId"))
        return self.runtime.agent(agent["id"])

    def events(self, agent, kind=None):
        with self.runtime.db() as db:
            rows = [
                dict(r)
                for r in db.execute(
                    "SELECT * FROM runtime_events WHERE agent=? ORDER BY created",
                    (agent["id"],),
                )
            ]
        return [r for r in rows if kind is None or r["kind"] == kind]

    def work(self, lead, title="Task", **data):
        return self.runtime.work_action(
            lead["id"], {"action": "create", "title": title, **data}, actor=lead["id"]
        )

    def action(self, actor, task, action, **data):
        return self.runtime.work_action(
            actor["id"],
            {"action": action, "task_id": task["id"], **data},
            actor=actor["id"],
        )

    def tool(self, agent, tool, arguments):
        agent = self.runtime.prepare(self.runtime.agent(agent["id"]))
        request_id = str(uuid.uuid4())
        self.runtime.dynamic(
            {
                "id": request_id,
                "params": {
                    "threadId": agent["threadId"],
                    "callId": request_id,
                    "tool": tool,
                    "arguments": arguments,
                },
            }
        )
        return next(
            r["result"] for r in self.runtime.server.responses if r["id"] == request_id
        )

    def rule_record(self, key):
        return next(r for r in self.runtime.rules()["rules"] if r["id"] == key)

    def due(self, rule):
        with self.runtime.lock, self.runtime.db() as db:
            current = self.rule_record(rule["id"])
            current["nextAt"] = time.time() - 1
            self.runtime.put(db, "rules", current)
        self.runtime.rules_tick()

    def git(self, cwd, *args):
        result = subprocess.run(
            ["git", "-C", str(cwd), *args], text=True, capture_output=True, check=True
        )
        return result.stdout.strip()

    def git_project(self):
        self.git(self.project, "init", "-q")
        self.git(self.project, "config", "user.name", "Fixture")
        self.git(self.project, "config", "user.email", "fixture@invalid.local")
        (self.project / "tracked.txt").write_text("base\n")
        self.git(self.project, "add", "tracked.txt")
        self.git(self.project, "commit", "-qm", "Fixture base")
        return self.lead()

    def isolated_worker(self):
        lead = self.git_project()
        path = self.root / "worker"
        self.git(
            self.project, "worktree", "add", "-q", "-b", "fixture-worker", str(path)
        )
        return self.worker(lead, cwd=str(path), worktree=True, worktreeReady=True), path

    def test_monitor_snapshot_bounds_history_and_keeps_all_active_checks(self):
        lead = self.lead()
        with self.runtime.lock, self.runtime.db() as db:
            for number in range(105):
                self.runtime.put(
                    db,
                    "monitors",
                    {
                        "id": f"history-{number}",
                        "agent": lead["id"],
                        "cwd": lead["cwd"],
                        "status": "failed",
                        "created": number,
                        "ruleId": "condition",
                        "command": "check",
                        "exitCode": 1,
                    },
                )
            self.runtime.put(
                db,
                "monitors",
                {
                    "id": "old-active",
                    "agent": lead["id"],
                    "cwd": lead["cwd"],
                    "status": "running",
                    "created": -1,
                    "command": "watch",
                },
            )
        records = self.runtime.snapshot()["monitors"]
        self.assertEqual(len(records), 101)
        self.assertIn("old-active", {r["id"] for r in records})
        self.assertNotIn("history-0", {r["id"] for r in records})
        self.assertFalse(
            any(
                r["kind"] == "monitor"
                for r in self.runtime.workspace_snapshot()["inbox"]
            )
        )
        with self.runtime.db() as db:
            self.assertEqual(len(self.runtime.records(db, "monitors")), 106)

    def test_workspace_scope_includes_only_the_selected_lead_tree(self):
        lead = self.lead()
        other = self.lead("Other chat in the same project")
        worker = self.worker(lead)
        child_project = self.root / "child-project"
        child_project.mkdir()
        child = self.worker(worker, "Nested worker", cwd=str(child_project))
        owners = [lead, worker, child, other]
        for owner in owners:
            user_task = {
                "action": "create", "title": "User action", "criteria": "Check the result",
            }
            if owner["isLead"]:
                self.runtime.user_task_action(owner["id"], user_task)
            else:
                with self.assertRaisesRegex(ValueError, "Only the orchestrator"):
                    self.runtime.user_task_action(owner["id"], user_task)
                with self.runtime.db() as db:
                    self.assertFalse(any(task["agent"] == owner["id"]
                        for task in self.runtime.records(db, "user_tasks")))
            self.runtime.complaint(owner["id"], {
                "action": "submit", "text": "This chat needs a response",
            }, "complaint-" + owner["id"], user=True)
            with self.runtime.lock, self.runtime.db() as db:
                for table, record in [
                    ("requests", {"status": "pending", "method": "item/tool/requestUserInput"}),
                    ("monitors", {"status": "failed", "command": "check", "created": 1, "exitCode": 1}),
                    ("rules", {"name": "Check rule", "kind": "interval", "error": "Check failed"}),
                    ("annotations", {}),
                    ("checkpoints", {}),
                ]:
                    self.runtime.put(db, table, {
                        "id": table + "-" + owner["id"], "agent": owner["id"],
                        "rootId": owner["rootId"], **record,
                    })
                self.runtime.put(db, "plans", {
                    "id": owner["id"], "rootId": owner["rootId"],
                })
            self.agent_update(owner, status="failed")
        for owner in [lead, other]:
            work = self.work(owner)
            with self.runtime.lock, self.runtime.db() as db:
                self.runtime.put(db, "work", {**work, "status": "review"})

        global_state = self.runtime.workspace_snapshot()
        team_ids = {lead["id"], worker["id"], child["id"]}
        for selected in [lead, worker, child]:
            with self.subTest(selected=selected["name"]):
                scoped = self.runtime.workspace_snapshot(selected["id"])
                self.assertEqual(scoped["inbox"], [
                    item for item in global_state["inbox"] if item["agent"] in team_ids
                ])
                self.assertEqual({item["kind"] for item in scoped["inbox"]}, {
                    "request", "complaint", "user_task", "work", "agent", "monitor", "rule",
                })
                for field in ["annotations", "rules", "monitors"]:
                    self.assertEqual({item["agent"] for item in scoped[field]}, team_ids)
                self.assertEqual({item["id"] for item in scoped["plans"]}, team_ids)
                self.assertEqual({item["rootId"] for item in scoped["work"]}, {lead["id"]})
                self.assertEqual({item["agent"] for item in scoped["checkpoints"]}, {selected["id"]})
        self.assertEqual({item["agent"] for item in global_state["inbox"]}, {
            owner["id"] for owner in owners
        })
        other_state = self.runtime.workspace_snapshot(other["id"])
        self.assertEqual({item["agent"] for item in other_state["inbox"]}, {other["id"]})

    def test_workspace_background_history_limit_applies_within_the_chat(self):
        lead = self.lead()
        worker = self.worker(lead)
        other = self.lead("Other chat")
        with self.runtime.lock, self.runtime.db() as db:
            for owner, offset in [(worker, 0), (other, 1000)]:
                for table in ["tasks", "monitors"]:
                    for number in range(105):
                        self.runtime.put(db, table, {
                            "id": f"{table}-{owner['id']}-{number}", "agent": owner["id"],
                            "status": "failed", "created": offset + number,
                            "command": "check", "exitCode": 1,
                            "tail": "large output", "arguments": {"input": "large"}, "error": "failed",
                        })
                    self.runtime.put(db, table, {
                        "id": table + "-active-" + owner["id"], "agent": owner["id"],
                        "status": "running", "created": -1,
                    })
        scoped = self.runtime.workspace_snapshot(lead["id"])
        for field in ["tasks", "monitors"]:
            self.assertEqual(len(scoped[field]), 101)
            self.assertEqual({item["agent"] for item in scoped[field]}, {worker["id"]})
            keys = {item["id"] for item in scoped[field]}
            self.assertIn(field + "-active-" + worker["id"], keys)
            self.assertIn(f"{field}-{worker['id']}-5", keys)
            self.assertNotIn(f"{field}-{worker['id']}-4", keys)
        self.assertEqual(len([item for item in scoped["inbox"] if item["kind"] == "monitor"]), 100)
        self.assertEqual(scoped["tasksHistoryLimit"], 100)
        self.assertFalse(any({"tail", "arguments", "error"} & item.keys() for item in scoped["tasks"]))
        global_state = self.runtime.workspace_snapshot()
        runtime_state = self.runtime.snapshot()
        for field in ["tasks", "monitors"]:
            self.assertEqual(global_state[field], runtime_state[field])
            self.assertEqual(len(global_state[field]), 102)
            self.assertEqual({item["agent"] for item in global_state[field] if item["status"] != "running"}, {other["id"]})

    def test_workspace_rejects_unknown_or_deleted_chat_scope(self):
        with self.assertRaises(ValueError):
            self.runtime.workspace_snapshot("unknown-chat")
        lead = self.lead()
        self.agent_update(lead, deletedAt=time.time())
        with self.assertRaisesRegex(ValueError, "deleted"):
            self.runtime.workspace_snapshot(lead["id"])

    def test_reported_changes_are_per_agent_and_restore_the_full_saved_diff(self):
        lead = self.git_project()
        other = self.lead("Other chat in the same directory")
        (self.project / "tracked.txt").write_text("Shared directory change\n")
        self.assertIn("Shared directory change", self.runtime.changes(other["id"])["diff"])
        diff = "--- a/owned.txt\n+++ b/owned.txt\n@@ -1 +1 @@\n-before\n+" + "x" * 25000 + "\n"
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.item(db, lead["id"], "turn/diff/updated", "output", json.dumps({
                "threadId": "fixture-thread", "turnId": "owned-turn", "diff": diff,
            }), "Changes")
        reported = self.runtime.changes(lead["id"], scope="chat")
        self.assertEqual(reported["diff"], diff)
        self.assertEqual(reported["patch"], diff)
        self.assertEqual(reported["files"], [{"path": "owned.txt", "status": "M"}])
        self.assertEqual(reported["turnId"], "owned-turn")
        self.assertIsNotNone(reported["reportedAt"])
        self.assertFalse(reported["truncated"])
        empty = self.runtime.changes(other["id"], scope="chat")
        self.assertEqual(empty, {
            "scope": "chat", "git": True, "files": [], "diff": "", "patch": "",
            "truncated": False, "turnId": None, "reportedAt": None,
        })
        with self.assertRaisesRegex(ValueError, "Unknown changes scope"):
            self.runtime.changes(other["id"], scope="unknown")

    def test_reported_changes_decode_git_paths_without_treating_content_as_headers(self):
        diff = (
            '--- a/real.txt\n+++ b/real.txt\n@@ -1 +1 @@\n--- a/fake.txt\n+++ b/fake.txt\n'
            '--- /dev/null\n+++ "b/caf\\303\\251\\tname.txt"\n@@ -0,0 +1 @@\n+new\n'
            '--- "a/quote\\\"file.txt"\n+++ /dev/null\n@@ -1 +0,0 @@\n-old\n'
            '--- "a/unsupported\\q.txt"\n+++ "b/unsupported\\q.txt"\n'
        )
        self.assertEqual(self.runtime.reported_change_files(diff), [
            {"path": "real.txt", "status": "M"},
            {"path": "café\tname.txt", "status": "A"},
            {"path": 'quote"file.txt', "status": "D"},
        ])

    def test_reported_changes_bound_output_and_reject_missing_full_payload(self):
        lead = self.lead()
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.item(db, lead["id"], "turn/diff/updated", "output", json.dumps({
                "turnId": "large-turn", "diff": "x" * 300001,
            }), "Changes")
        reported = self.runtime.changes(lead["id"], scope="chat")
        self.assertEqual(len(reported["diff"]), 300000)
        self.assertTrue(reported["truncated"])
        with self.runtime.lock, self.runtime.db() as db:
            db.execute("DELETE FROM runtime_search WHERE id=?", (lead["id"] + ":turn/diff/updated",))
        with self.assertRaisesRegex(ValueError, "complete reported changes are unavailable"):
            self.runtime.changes(lead["id"], scope="chat")

    def test_reported_change_annotation_keeps_turn_context_and_retry_identity(self):
        lead = self.lead()
        data = {"id": "reported-comment", "path": "file.txt", "line": 2,
                "text": "Check this line", "turnId": "reported-turn"}
        note = self.runtime.annotate(lead["id"], data)
        self.assertEqual(note["turnId"], "reported-turn")
        self.assertEqual(self.runtime.annotate(lead["id"], data), note)
        events = self.events(lead, "user")
        self.assertEqual(len(events), 1)
        self.assertIn("(reported turn reported-turn)", events[0]["text"])
        with self.assertRaisesRegex(ValueError, "different content"):
            self.runtime.annotate(lead["id"], {**data, "turnId": "other-turn"})

    def test_atomic_claim_has_one_winner_under_concurrent_workers(self):
        lead = self.lead()
        workers = [self.worker(lead, f"Worker {i}") for i in range(12)]
        task = self.work(lead)
        gate = threading.Barrier(len(workers))

        def claim(worker):
            gate.wait(5)
            try:
                return self.action(worker, task, "claim")["owner"]
            except ValueError as error:
                self.assertIn("Another agent owns", str(error))
                return None

        with ThreadPoolExecutor(max_workers=len(workers)) as pool:
            outcomes = list(pool.map(claim, workers))
        winners = [key for key in outcomes if key]
        self.assertEqual(len(winners), 1)
        item = self.runtime.work_action(lead["id"], {})["items"][0]
        self.assertEqual((item["owner"], item["status"]), (winners[0], "running"))

    def test_dependencies_require_review_acceptance_and_reject_cycles(self):
        lead = self.lead()
        worker = self.worker(lead)
        first = self.work(lead, "First")
        second = self.work(
            lead, "Second", dependencies=[first["id"]], owner=worker["id"]
        )
        with self.assertRaisesRegex(ValueError, "cycle"):
            self.action(lead, first, "update", dependencies=[second["id"]])
        with self.assertRaisesRegex(ValueError, "not ready"):
            self.action(worker, second, "claim")
        with self.assertRaisesRegex(ValueError, "Submit a result"):
            self.action(lead, first, "accept", result="Looks good")
        self.action(worker, first, "claim")
        with self.assertRaisesRegex(ValueError, "test evidence"):
            self.action(worker, first, "submit", result="Done", revision="abc")
        first = self.action(
            worker,
            first,
            "submit",
            result="Done",
            checks="unit suite passed",
            revision="abc",
        )
        self.assertEqual(first["status"], "review")
        with self.assertRaisesRegex(ValueError, "not ready"):
            self.action(worker, second, "claim")
        with self.assertRaisesRegex(ValueError, "Only the lead"):
            self.action(worker, first, "accept", result="Self approval")
        self.action(lead, first, "accept", result="Reviewed diff and tests")
        self.assertEqual(self.action(worker, second, "claim")["status"], "running")
        self.assertEqual(len(self.events(worker, "work_ready")), 1)
        with self.assertRaisesRegex(ValueError, "accepted work"):
            self.action(lead, first, "update", title="Rewrite history")

    def test_work_mutation_receipts_and_versions_prevent_duplicate_or_stale_writes(
        self,
    ):
        lead = self.lead()
        body = {"action": "create", "title": "Idempotent"}
        first = self.runtime.work_action(lead["id"], body, "request-id", lead["id"])
        self.assertEqual(
            first, self.runtime.work_action(lead["id"], body, "request-id", lead["id"])
        )
        self.assertEqual(len(self.runtime.work_action(lead["id"], {})["items"]), 1)
        with self.assertRaisesRegex(ValueError, "different content"):
            self.runtime.work_action(
                lead["id"], {**body, "title": "Different"}, "request-id", lead["id"]
            )
        self.action(lead, first, "update", title="New", version=first["version"])
        with self.assertRaisesRegex(ValueError, "changed"):
            self.action(lead, first, "update", title="Stale", version=first["version"])

    def test_tools_cannot_read_other_team_work_or_private_rooms(self):
        lead, other = self.lead(), self.lead("Other")
        peer, outsider = self.worker(lead), self.worker(lead, "Outsider")
        secret = self.work(other, "ultrasecret work")
        result = self.tool(
            peer, "orchestration_result", {"action": "read", "task_id": secret["id"]}
        )
        self.assertFalse(result["success"])
        self.runtime.chat_message(
            lead["id"], peer["id"], "ultrasecret private chat", "private-msg"
        )
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.item(
                db, other["id"], "secret", "assistant", "ultrasecret other team"
            )
            self.runtime.item(
                db, lead["id"], "public", "assistant", "ultrasecret shared team result"
            )
        participant = self.runtime.search_work("ultrasecret", peer["id"])["results"]
        outsider_hits = self.runtime.search_work("ultrasecret", outsider["id"])[
            "results"
        ]
        self.assertTrue(any(r["type"] == "room" for r in participant))
        self.assertFalse(any(r["type"] == "room" for r in outsider_hits))
        self.assertFalse(any(r["agent"] == other["id"] for r in participant))
        self.assertTrue(
            any(
                r["type"] == "work"
                for r in self.runtime.search_work("ultrasecret")["results"]
            )
        )
        with self.assertRaisesRegex(ValueError, "another team"):
            self.runtime.work_action(other["id"], {"action": "list"}, actor=peer["id"])

    def test_search_updates_replaced_item_and_survives_restart(self):
        lead = self.lead()
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.item(db, lead["id"], "mutable", "assistant", "oldneedle")
            self.runtime.item(db, lead["id"], "mutable", "assistant", "newneedle")
        self.assertEqual(self.runtime.search_work("oldneedle")["results"], [])
        self.runtime.close()
        self.runtime = ControlledRuntime(self.state, WorkspaceServer)
        self.assertEqual(len(self.runtime.search_work("newneedle")["results"]), 1)
        self.assertEqual(self.runtime.search_work('" OR *')["results"], [])

    def test_upload_validation_receipts_and_image_inputs(self):
        lead = self.lead()
        body = {
            "agent": lead["id"],
            "id": str(uuid.uuid4()),
            "name": "../../image.png",
            "base64": base64.b64encode(b"\x89PNG\r\n\x1a\nfixture").decode(),
        }
        asset = self.runtime.upload_asset(body)
        self.assertEqual(asset["name"], "image.png")
        self.assertNotIn("path", asset)
        self.assertEqual(self.runtime.upload_asset(body), asset)
        with self.assertRaisesRegex(ValueError, "different content"):
            self.runtime.upload_asset(
                {**body, "base64": base64.b64encode(b"changed").decode()}
            )
        for content in ("not base64!", ""):
            with self.assertRaises(ValueError):
                self.runtime.upload_asset(
                    {"agent": lead["id"], "name": "x", "base64": content}
                )
        fake = self.runtime.upload_asset(
            {
                "agent": lead["id"],
                "name": "fake.png",
                "base64": base64.b64encode(b"plain text").decode(),
            }
        )
        self.assertFalse(fake["image"])
        self.assertEqual(fake["mime"], "application/octet-stream")
        inputs = self.runtime.message_inputs(lead["id"], "Inspect image", [asset["id"]])
        self.assertEqual(inputs[1]["type"], "localImage")
        self.assertEqual(
            Path(inputs[1]["path"]).read_bytes(), b"\x89PNG\r\n\x1a\nfixture"
        )
        with self.assertRaisesRegex(ValueError, "another conversation"):
            self.runtime.message_inputs(self.lead("Other")["id"], "Read", [asset["id"]])

    def test_workspace_reads_allow_external_absolute_relative_and_symlink_paths(self):
        lead = self.lead()
        secret = self.root / "secret.txt"
        secret.write_text("secret")
        (self.project / "escape").symlink_to(secret)
        (self.project / "allowed.txt").write_text("allowed")
        self.assertEqual(
            self.runtime.file_content(lead["id"], "allowed.txt")[0], b"allowed"
        )
        for path in ("../secret.txt", "escape", str(secret)):
            self.assertEqual(self.runtime.file_content(lead["id"], path)[0], b"secret")

    def test_queue_attachments_persist_and_dispatch_after_restart(self):
        lead = self.lead()
        asset = self.runtime.upload_asset(
            {
                "agent": lead["id"],
                "name": "report.txt",
                "base64": base64.b64encode(b"evidence").decode(),
            }
        )
        first = self.runtime.send(
            lead["id"], "Read report", "message-asset", assets=[asset["id"]]
        )
        repeated = self.runtime.send(
            lead["id"], "Read report", "message-asset", assets=[asset["id"]]
        )
        self.assertEqual(first["id"], repeated["id"])
        self.assertEqual(len(self.events(lead)), 1)
        with self.assertRaisesRegex(ValueError, "different content"):
            self.runtime.send(lead["id"], "Read report", "message-asset", assets=[])
        self.runtime.close()
        self.runtime = ControlledRuntime(self.state, WorkspaceServer)
        self.runtime.dispatch()
        eventually(lambda: self.runtime.agent(lead["id"]).get("turnId"))
        turn = next(p for m, p in self.runtime.server.calls if m == "turn/start")
        self.assertIn("Attached file: report.txt", turn["input"][1]["text"])
        messages = self.runtime.transcript(lead["id"])["items"]
        self.assertTrue(
            any(
                m.get("assets", [{}])[0].get("id") == asset["id"]
                for m in messages
                if m.get("assets")
            )
        )

    def test_queue_edit_cancel_reorder_and_conflict(self):
        lead = self.lead()
        for number in range(3):
            self.runtime.send(lead["id"], f"Message {number}", f"queued-{number}")
        self.runtime.queue_action(
            lead["id"], {"action": "first", "message_id": "queued-2"}
        )
        self.assertEqual(
            self.runtime.queue_action(lead["id"])["items"][0]["id"], "queued-2"
        )
        self.runtime.queue_action(
            lead["id"],
            {
                "action": "edit",
                "message_id": "queued-0",
                "expectedText": "Message 0",
                "text": "Edited",
            },
        )
        with self.assertRaisesRegex(ValueError, "changed"):
            self.runtime.queue_action(
                lead["id"],
                {
                    "action": "edit",
                    "message_id": "queued-0",
                    "expectedText": "Message 0",
                    "text": "Stale",
                },
            )
        self.runtime.queue_action(
            lead["id"], {"action": "cancel", "message_id": "queued-1"}
        )
        self.runtime.dispatch()
        eventually(lambda: self.runtime.agent(lead["id"]).get("turnId"))
        text = next(p for m, p in self.runtime.server.calls if m == "turn/start")[
            "input"
        ][0]["text"]
        self.assertTrue(text.startswith("Message 2\n\nEdited"))
        self.assertNotIn("Message 1", text)
        with self.assertRaisesRegex(ValueError, "already left"):
            self.runtime.queue_action(
                lead["id"], {"action": "cancel", "message_id": "queued-0"}
            )

    def test_steer_uses_exact_turn_and_receipt_without_a_second_turn(self):
        lead = self.start(self.lead())
        outcome = self.runtime.send(
            lead["id"], "Correction", "steer-id", delivery="steer"
        )
        self.assertEqual(outcome["status"], "delivered")
        self.runtime.send(lead["id"], "Correction", "steer-id", delivery="steer")
        steers = [p for m, p in self.runtime.server.calls if m == "turn/steer"]
        self.assertEqual(len(steers), 1)
        self.assertEqual(steers[0]["expectedTurnId"], lead["turnId"])
        self.assertEqual(
            sum(m == "turn/start" for m, _ in self.runtime.server.calls), 1
        )
        self.assertEqual(self.runtime.queue_action(lead["id"])["items"], [])

    def test_steer_timeout_remains_uncertain_and_never_falls_back_to_queue(self):
        lead = self.start(self.lead())
        self.runtime.server.steer_error = TimeoutError(
            "Steer response timed out; outcome unknown"
        )
        with self.assertRaises(TimeoutError):
            self.runtime.send(
                lead["id"], "Correction", "uncertain-steer", delivery="steer"
            )
        retry = self.runtime.send(
            lead["id"], "Correction", "uncertain-steer", delivery="steer"
        )
        self.assertEqual(retry["status"], "uncertain")
        self.assertEqual(self.runtime.queue_action(lead["id"])["items"], [])
        self.runtime.server.complete(lead["threadId"], lead["turnId"])
        self.runtime.dispatch()
        self.assertEqual(
            sum(m == "turn/start" for m, _ in self.runtime.server.calls), 1
        )
        with self.assertRaisesRegex(ValueError, "no active turn"):
            self.runtime.send(lead["id"], "Late correction", delivery="steer")

    def test_file_watch_does_not_infer_until_the_file_changes(self):
        lead = self.lead()
        watched = self.project / "watched.txt"
        watched.write_text("initial")
        rule = self.runtime.rules(
            {"agent": lead["id"], "name": "File", "kind": "file", "path": "watched.txt"}
        )
        self.due(rule)
        self.assertEqual(self.rule_record(rule["id"])["checks"], 0)
        self.assertEqual(self.events(lead, "rule"), [])
        self.assertIsNone(self.runtime.server)
        watched.write_text("changed content")
        self.due(rule)
        eventually(lambda: self.rule_record(rule["id"])["wakes"] == 1)
        self.assertEqual(len(self.events(lead, "rule")), 1)
        self.assertIsNone(
            self.runtime.server,
            "A watch should queue the event without calling a model",
        )

    def test_script_rule_suppresses_false_output_and_nonzero_exit(self):
        lead = self.runtime.prepare(self.lead())
        self.agent_update(lead, approvalPolicy="never")
        rule = self.runtime.rules(
            {"agent": lead["id"], "name": "Script", "command": "fixture-rule-command"}
        )
        self.due(rule)
        eventually(lambda: self.rule_record(rule["id"]).get("lastFinished"))
        self.assertEqual(self.rule_record(rule["id"])["wakes"], 0)
        self.assertEqual(self.events(lead, "monitor_exit"), [])
        self.assertEqual(self.events(lead, "rule"), [])
        self.assertFalse(any(m == "turn/start" for m, _ in self.runtime.server.calls))
        self.runtime.server.command_result = {
            "exitCode": 7,
            "stdout": "changed\n",
            "stderr": "",
        }
        self.due(rule)
        eventually(lambda: self.rule_record(rule["id"]).get("lastExitCode") == 7)
        self.assertEqual(self.rule_record(rule["id"])["wakes"], 0)
        self.runtime.server.command_result = {
            "exitCode": 0,
            "stdout": '{"wakeAgent":true}\n',
            "stderr": "",
        }
        self.due(rule)
        eventually(lambda: self.rule_record(rule["id"])["wakes"] == 1)
        self.assertEqual(len(self.events(lead, "rule")), 1)

    def test_stop_and_restart_do_not_repeat_uncertain_rules(self):
        lead = self.lead()
        rule = self.runtime.rules({"agent": lead["id"], "name": "Rule"})
        self.runtime.stop(lead["id"])
        self.due(rule)
        self.assertEqual(self.rule_record(rule["id"])["status"], "paused")
        self.assertEqual(self.events(lead, "rule"), [])
        self.agent_update(lead, autoWake=True, status="idle")
        rule = self.runtime.rules(
            {"action": "resume", "agent": lead["id"], "id": rule["id"]}
        )
        with self.runtime.lock, self.runtime.db() as db:
            rule.update(inFlight=True, nextAt=time.time() - 1)
            self.runtime.put(db, "rules", rule)
        self.runtime.close()
        self.runtime = ControlledRuntime(self.state, WorkspaceServer)
        self.runtime.rules_tick()
        recovered = self.rule_record(rule["id"])
        self.assertEqual(recovered["status"], "paused")
        self.assertFalse(recovered["inFlight"])
        self.assertIn("Outcome unknown", recovered["error"])
        self.assertIsNone(self.runtime.server)
        self.assertEqual(self.events(lead, "rule"), [])

    def test_checkpoint_capture_preserves_the_real_index_and_blocks_main_restore(self):
        lead = self.git_project()
        (self.project / "tracked.txt").write_text("staged\n")
        self.git(self.project, "add", "tracked.txt")
        index = self.git(self.project, "write-tree")
        (self.project / "tracked.txt").write_text("unstaged\n")
        checkpoint = self.runtime.checkpoint_capture(lead["id"])
        self.assertEqual(self.git(self.project, "write-tree"), index)
        preview = self.runtime.checkpoint_preview(lead["id"], checkpoint["id"])
        self.assertFalse(preview["canRestore"])
        with self.assertRaisesRegex(ValueError, "isolated"):
            self.runtime.restore_checkpoint(
                lead["id"],
                {
                    "checkpoint_id": checkpoint["id"],
                    "expectedTree": preview["expectedTree"],
                },
            )
        self.assertEqual((self.project / "tracked.txt").read_text(), "unstaged\n")

    def test_checkpoint_restore_rejects_stale_preview_and_provider_failure_before_files(
        self,
    ):
        worker, path = self.isolated_worker()
        self.agent_update(worker, threadId="original-thread")
        checkpoint = self.runtime.checkpoint_capture(
            worker["id"], turn_id="completed-turn"
        )
        (path / "tracked.txt").write_text("first edit\n")
        preview = self.runtime.checkpoint_preview(worker["id"], checkpoint["id"])
        (path / "tracked.txt").write_text("second edit\n")
        with self.assertRaisesRegex(ValueError, "Files changed after"):
            self.runtime.restore_checkpoint(
                worker["id"],
                {
                    "checkpoint_id": checkpoint["id"],
                    "expectedTree": preview["expectedTree"],
                },
            )
        preview = self.runtime.checkpoint_preview(worker["id"], checkpoint["id"])
        self.runtime.connect().fork_error = NativeRpcError(
            {"code": -32000, "message": "Provider rejected fork"}
        )
        with self.assertRaisesRegex(RuntimeError, "Provider rejected"):
            self.runtime.restore_checkpoint(
                worker["id"],
                {
                    "checkpoint_id": checkpoint["id"],
                    "expectedTree": preview["expectedTree"],
                },
            )
        self.assertEqual((path / "tracked.txt").read_text(), "second edit\n")
        self.assertIsNone(self.runtime.agent(worker["id"]).get("workspaceOperation"))

    def test_checkpoint_restore_recreates_exact_isolated_files_and_keeps_recovery_point(
        self,
    ):
        worker, path = self.isolated_worker()
        self.agent_update(
            worker, threadId="original-thread", lastCompletedTurn="completed-turn"
        )
        checkpoint = self.runtime.checkpoint_capture(worker["id"])
        (path / "tracked.txt").write_text("new edit\n")
        (path / "new-untracked.txt").write_text("new file\n")
        preview = self.runtime.checkpoint_preview(worker["id"], checkpoint["id"])
        self.assertIn("new-untracked.txt", preview["diff"])
        result = self.runtime.restore_checkpoint(
            worker["id"],
            {
                "checkpoint_id": checkpoint["id"],
                "expectedTree": preview["expectedTree"],
            },
        )
        self.assertEqual(result["status"], "restored")
        self.assertEqual((path / "tracked.txt").read_text(), "base\n")
        self.assertFalse((path / "new-untracked.txt").exists())
        agent = self.runtime.agent(worker["id"])
        self.assertFalse(agent["autoWake"])
        self.assertNotEqual(agent["threadId"], "original-thread")
        self.assertEqual(agent["lastCompletedTurn"], "completed-turn")
        forks = [p for m, p in self.runtime.server.calls if m == "thread/fork"]
        self.assertEqual(forks[0]["lastTurnId"], "completed-turn")
        with self.runtime.db() as db:
            saved = self.runtime.records(db, "checkpoints")
        recovery = next(c for c in saved if c["label"] == "Before restore")
        self.assertEqual(
            self.git(path, "show", recovery["tree"] + ":new-untracked.txt"), "new file"
        )
        self.assertEqual((self.project / "tracked.txt").read_text(), "base\n")

    def test_multiple_queued_attachment_messages_keep_each_valid_input(self):
        lead = self.lead()
        assets = [
            self.runtime.upload_asset(
                {
                    "agent": lead["id"],
                    "name": f"file-{i}.txt",
                    "base64": base64.b64encode(f"file {i}".encode()).decode(),
                }
            )["id"]
            for i in range(10)
        ]
        self.runtime.send(lead["id"], "First files", "files-first", assets=assets[:5])
        self.runtime.send(lead["id"], "Second files", "files-second", assets=assets[5:])
        self.runtime.dispatch()
        eventually(
            lambda: self.runtime.agent(lead["id"]).get("turnId")
            or self.runtime.agent(lead["id"])["status"] == "failed"
        )
        self.assertIsNotNone(
            self.runtime.agent(lead["id"]).get("turnId"),
            self.runtime.agent(lead["id"]).get("error"),
        )
        turns = [p for m, p in self.runtime.server.calls if m == "turn/start"]
        first_inputs = json.dumps(turns[0]["input"])
        self.assertIn("file-0.txt", first_inputs)
        # Implementations may split valid messages into separate turns to honor provider limits.
        if "file-9.txt" not in first_inputs:
            current = self.runtime.agent(lead["id"])
            self.runtime.server.complete(current["threadId"], current["turnId"])
            self.runtime.dispatch()
            eventually(
                lambda: len(
                    [p for m, p in self.runtime.server.calls if m == "turn/start"]
                )
                == 2
            )
        all_inputs = json.dumps(
            [p["input"] for m, p in self.runtime.server.calls if m == "turn/start"]
        )
        for i in range(10):
            self.assertIn(f"file-{i}.txt", all_inputs)

    def test_private_room_content_does_not_leak_through_recipient_transcript_search(
        self,
    ):
        lead = self.lead()
        recipient, outsider = self.worker(lead), self.worker(lead, "Outsider")
        self.runtime.chat_message(
            lead["id"], recipient["id"], "privatecanary", "private-canary"
        )
        self.runtime.dispatch()
        eventually(lambda: self.runtime.agent(recipient["id"]).get("turnId"))
        self.assertTrue(
            self.runtime.search_work("privatecanary", recipient["id"])["results"]
        )
        self.assertEqual(
            self.runtime.search_work("privatecanary", outsider["id"])["results"], []
        )
        response = self.tool(
            outsider, "orchestration_search", {"query": "privatecanary"}
        )
        self.assertTrue(response["success"])
        self.assertEqual(json.loads(response["contentItems"][0]["text"])["results"], [])

    def test_interactive_monitor_input_uses_connection_id_and_enforces_ownership(self):
        lead = self.runtime.prepare(self.lead())
        peer = self.worker(lead)
        monitor = self.runtime.monitor(
            lead["id"],
            {"command": "hold interactive", "interactive": True},
            approved=True,
        )
        eventually(
            lambda: next(
                m
                for m in self.runtime.snapshot()["monitors"]
                if m["id"] == monitor["id"]
            )["status"]
            == "running"
        )
        self.runtime.monitor_input(monitor["id"], {"text": "hello\n"}, lead["id"])
        write = next(
            p for m, p in self.runtime.server.calls if m == "command/exec/write"
        )
        self.assertEqual(write["processId"], monitor["id"])
        self.assertEqual(base64.b64decode(write["deltaBase64"]), b"hello\n")
        self.runtime.monitor_input(monitor["id"], {"rows": 40, "cols": 120}, lead["id"])
        resize = next(
            p for m, p in self.runtime.server.calls if m == "command/exec/resize"
        )
        self.assertEqual(resize["size"], {"rows": 40, "cols": 120})
        with self.assertRaisesRegex(ValueError, "another agent"):
            self.runtime.monitor_input(monitor["id"], {"text": "injected"}, peer["id"])
        self.runtime.cancel_monitor(monitor["id"], lead["id"])
        with self.assertRaisesRegex(ValueError, "not active"):
            self.runtime.monitor_input(monitor["id"], {"text": "late"}, lead["id"])

    def test_resource_registry_is_shared_and_tool_cannot_impersonate_a_peer(self):
        lead = self.lead()
        first, second = self.worker(lead), self.worker(lead, "Second")
        claimed = self.runtime.resource_action(
            {"action": "claim", "resource": "fixture-slot"}, first["id"]
        )
        self.assertTrue(claimed["ok"])
        self.assertEqual(
            Path(claimed["path"]).resolve(),
            (self.root / "board" / "codex-board.json").resolve(),
        )
        self.assertEqual(
            claimed["state"]["claims"]["fixture-slot"]["worker"], first["id"]
        )
        busy = self.runtime.resource_action(
            {"action": "claim", "resource": "fixture-slot"}, second["id"]
        )
        self.assertFalse(busy["ok"])
        response = self.tool(
            second,
            "orchestration_resource",
            {"action": "release", "resource": "fixture-slot", "agent": first["id"]},
        )
        result = (
            json.loads(response["contentItems"][0]["text"])
            if response["success"]
            else {}
        )
        self.assertFalse(
            result.get("ok", False),
            "A peer released a resource using another worker identity",
        )
        self.assertEqual(
            self.runtime.resource_action()["state"]["claims"]["fixture-slot"]["worker"],
            first["id"],
        )
        released = self.runtime.resource_action(
            {"action": "release", "resource": "fixture-slot"}, first["id"]
        )
        self.assertTrue(released["ok"])
        self.assertNotIn("fixture-slot", released["state"]["claims"])

    def test_event_rule_deduplicates_same_event_and_stop_prevents_late_wake(self):
        lead = self.lead()
        rule = self.runtime.rules(
            {
                "agent": lead["id"],
                "name": "Review event",
                "kind": "event",
                "event": "work_review",
            }
        )
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.enqueue(
                db,
                self.runtime.agent(lead["id"], db),
                "work_review",
                "Result ready",
                "same-event",
            )
        eventually(lambda: self.rule_record(rule["id"])["wakes"] == 1)
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.enqueue(
                db,
                self.runtime.agent(lead["id"], db),
                "work_review",
                "Result ready",
                "same-event",
            )
        self.assertEqual(self.rule_record(rule["id"])["wakes"], 1)
        self.runtime.stop(lead["id"])
        self.runtime.rule_finished(rule["id"], 0, None, "Late result")
        self.assertEqual(self.rule_record(rule["id"])["wakes"], 1)
        self.assertFalse(
            any(r["status"] == "pending" for r in self.events(lead, "rule"))
        )

    def test_branch_from_finished_message_is_native_and_idempotent_under_concurrency(
        self,
    ):
        lead = self.start(self.lead())
        self.runtime.notification(
            {
                "method": "item/completed",
                "params": {
                    "threadId": lead["threadId"],
                    "turnId": lead["turnId"],
                    "item": {
                        "id": "branch-answer",
                        "type": "agentMessage",
                        "text": "Branch point",
                    },
                },
            }
        )
        self.runtime.server.complete(lead["threadId"], lead["turnId"])
        message = next(
            i
            for i in self.runtime.transcript(lead["id"])["items"]
            if i["text"] == "Branch point"
        )
        request = {"id": str(uuid.uuid4()), "message_id": message["id"]}
        with ThreadPoolExecutor(max_workers=6) as pool:
            outcomes = list(
                pool.map(
                    lambda _: self.runtime.branch_conversation(lead["id"], request),
                    range(6),
                )
            )
        self.assertEqual(len({v["id"] for v in outcomes}), 1)
        self.assertEqual(
            sum(m == "thread/fork" for m, _ in self.runtime.server.calls), 1
        )
        branch = outcomes[0]
        self.assertTrue(branch["isLead"])
        self.assertEqual(branch["forkedFrom"], lead["id"])
        self.assertEqual(branch["status"], "idle")
        self.assertEqual(
            self.runtime.transcript(branch["id"])["items"][-1]["text"],
            "Result with evidence",
        )
        # Native forks include the complete selected turn, including its final answer.
        self.assertIn(
            "Branch point",
            [item["text"] for item in self.runtime.transcript(branch["id"])["items"]],
        )
        fork = next(p for m, p in self.runtime.server.calls if m == "thread/fork")
        self.assertEqual(fork["lastTurnId"], lead["turnId"])

    def test_old_turn_cannot_mutate_work_after_stop_and_resume(self):
        original = self.start(self.lead())
        self.runtime.stop(original["id"])
        current = self.start(original, "Resume task")
        self.assertNotEqual(current["turnId"], original["turnId"])
        request_id = "stale-work-call"
        self.runtime.dynamic(
            {
                "id": request_id,
                "params": {
                    "threadId": original["threadId"],
                    "turnId": original["turnId"],
                    "callId": request_id,
                    "tool": "orchestration_task",
                    "arguments": {"action": "create", "title": "Stale mutation"},
                },
            }
        )
        response = next(
            r["result"] for r in self.runtime.server.responses if r["id"] == request_id
        )
        self.assertFalse(response["success"])
        self.assertEqual(self.runtime.work_action(current["id"], {})["items"], [])

    def test_pause_rule_cancels_its_command_without_waking_agent(self):
        lead = self.runtime.prepare(self.lead())
        self.agent_update(lead, approvalPolicy="never")
        rule = self.runtime.rules(
            {"agent": lead["id"], "name": "Held check", "command": "held-rule-check"}
        )
        self.due(rule)
        eventually(
            lambda: any(
                m.get("ruleId") == rule["id"] and m["status"] == "running"
                for m in self.runtime.snapshot()["monitors"]
            )
        )
        self.runtime.rules({"action": "pause", "agent": lead["id"], "id": rule["id"]})
        eventually(lambda: any(
            m.get("ruleId") == rule["id"] and m["status"] == "cancelled"
            for m in self.runtime.snapshot()["monitors"]
        ))
        monitor = next(
            m
            for m in self.runtime.snapshot()["monitors"]
            if m.get("ruleId") == rule["id"]
        )
        self.assertEqual(monitor["status"], "cancelled")
        self.assertEqual(self.rule_record(rule["id"])["status"], "paused")
        self.assertFalse(self.rule_record(rule["id"])["inFlight"])
        self.assertEqual(self.events(lead, "rule"), [])
        self.assertEqual(self.events(lead, "monitor_exit"), [])
        self.assertFalse(any(m == "turn/start" for m, _ in self.runtime.server.calls))

    def test_plan_versions_and_capability_inventory_use_provider_data(self):
        lead = self.lead()
        first = self.runtime.plan_action(lead["id"])
        plan = self.runtime.plan_action(
            lead["id"], {"version": first["version"], "text": "1. Verify the change"}
        )
        with self.assertRaisesRegex(ValueError, "changed"):
            self.runtime.plan_action(
                lead["id"], {"version": first["version"], "text": "Stale"}
            )
        lead = self.start(lead)
        turn = next(p for m, p in self.runtime.server.calls if m == "turn/start")
        self.assertIn(plan["text"], turn["input"][0]["text"])
        inventory = self.runtime.capabilities(lead["id"])
        self.assertEqual(inventory["skills"][0]["skills"][0]["name"], "fixture-skill")
        self.assertEqual(inventory["servers"][0]["name"], "fixture-mcp")
        names = {t["name"] for t in inventory["managed"]}
        self.assertTrue(
            {"orchestration_task", "orchestration_watch", "orchestration_resource"}
            <= names
        )
        self.assertEqual(inventory["errors"], [])

    def test_capability_refresh_discovers_a_new_skill_in_the_same_directory(self):
        lead = self.start(self.lead())
        server = self.runtime.server
        original_call = server.call
        installed = ["existing-skill"]
        cached = []

        def call(method, params, timeout=60):
            if method != "skills/list":
                return original_call(method, params, timeout)
            if params.get("forceReload") or not cached:
                cached[:] = installed
            return {"data": [{"cwd": params["cwds"][0],
                              "skills": [{"name": name} for name in cached]}]}

        with patch.object(server, "call", side_effect=call):
            first = self.runtime.capabilities(lead["id"])
            self.assertEqual(len(first["skills"][0]["skills"]), 1)
            installed.append("new-skill")
            self.runtime.capability_cache[lead["id"]]["at"] = 0
            refreshed = self.runtime.capabilities(lead["id"])
        self.assertEqual([skill["name"] for skill in refreshed["skills"][0]["skills"]],
                         ["existing-skill", "new-skill"])


if __name__ == "__main__":
    unittest.main(verbosity=2)
