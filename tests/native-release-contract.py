#!/usr/bin/env python3
"""Idle native subscription release, with a fake Codex app-server."""

import importlib.util
import json
from pathlib import Path
import sys
import tempfile
import time
import unittest

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
from codex_agent_management import manage_agent
from codex_native_release import IDLE_SECONDS, release_agent, tick
from codex_runtime import Runtime

spec = importlib.util.spec_from_file_location("runtime_fixture", Path(__file__).with_name("runtime-contract.py"))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class ReleaseServer(fixture.FakeServer):
    def __init__(self, root, notify, request, died):
        super().__init__(root, notify, request, died)
        self.terminals = []
        self.queue = []
        self.closed_threads = set()
        self.unsubscribe_fail_once = False

    def call(self, method, params, timeout=60):
        if method == "thread/read":
            self.calls.append((method, params))
            tid = params["threadId"]
            status = ("notLoaded" if tid in self.closed_threads else
                      "active" if tid in self.active_turns else "idle")
            return {"thread": {"id": tid, "status": {"type": status}}}
        if method == "thread/backgroundTerminals/list":
            self.calls.append((method, params))
            return {"data": self.terminals, "nextCursor": None}
        if method == "thread/queue/list":
            self.calls.append((method, params))
            return {"data": self.queue, "nextCursor": None}
        if method == "thread/unsubscribe":
            self.calls.append((method, params))
            if self.unsubscribe_fail_once:
                self.unsubscribe_fail_once = False
                raise TimeoutError("unsubscribe response lost")
            return {"status": "unsubscribed"}
        return super().call(method, params, timeout)


class NativeReleaseContract(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.rt = Runtime(Path(self.tmp.name), ReleaseServer)

    def tearDown(self):
        self.rt.close()
        self.tmp.cleanup()

    def lead(self):
        lead = self.rt.create({"name": "Lead", "cwd": self.tmp.name, "prompt": "Coordinate"})
        fixture.eventually(lambda: self.rt.agent(lead["id"])["status"] == "running")
        self.idle(lead)
        return self.rt.agent(lead["id"])

    def worker(self, lead):
        worker = self.rt.create({"name": "Worker", "prompt": "Work", "role": "reviewer"}, lead["id"])
        fixture.eventually(lambda: self.rt.agent(worker["id"])["status"] == "running")
        self.idle(worker)
        return self.rt.agent(worker["id"])

    def idle(self, agent):
        current = self.rt.agent(agent["id"])
        self.rt.server.active_turns.pop(current["threadId"], None)
        with self.rt.lock, self.rt.db() as db:
            current = self.rt.agent(agent["id"], db)
            current.update(inFlight=False, turnId=None, status="completed", activeTools=[])
            self.rt.put(db, "agents", current)
            db.execute("UPDATE runtime_events SET status='delivered' WHERE agent=?", (agent["id"],))

    def age(self, agent, seconds):
        with self.rt.lock, self.rt.db() as db:
            current = self.rt.agent(agent["id"], db)
            current["lastEvent"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(time.time() - seconds))
            self.rt.put(db, "agents", current)

    def test_release_timing_and_resume(self):
        lead = self.lead()
        self.assertIn(lead["id"], self.rt.loaded)
        self.age(lead, IDLE_SECONDS - 60)
        self.assertEqual(release_agent(self.rt, lead["id"])["status"], "not_due")
        self.assertFalse(any(method == "thread/unsubscribe" for method, _ in self.rt.server.calls))
        self.age(lead, IDLE_SECONDS + 60)
        tick(self.rt, now=time.time() + 31)
        fixture.eventually(lambda: self.rt.agent(lead["id"]).get("nativeRelease", {}).get("phase") == "released")
        self.assertNotIn(lead["id"], self.rt.loaded)
        self.assertEqual(self.rt.agent(lead["id"])["nativeRelease"]["nativeStatus"], "unsubscribed")
        self.rt.send(lead["id"], "Continue", "release-resume")
        fixture.eventually(lambda: any(method == "thread/resume" for method, _ in self.rt.server.calls))
        fixture.eventually(lambda: self.rt.agent(lead["id"]).get("nativeRelease", {}).get("phase") == "resumed")

    def test_tick_limits_native_release_requests(self):
        leads = [self.lead() for _ in range(3)]
        for lead in leads:
            self.age(lead, IDLE_SECONDS + 60)
        tick(self.rt, now=time.time() + 31)
        fixture.eventually(lambda: len([method for method, _ in self.rt.server.calls
                                        if method == "thread/unsubscribe"]) == 2)
        fixture.eventually(lambda: len([lead for lead in leads if lead["id"] in self.rt.loaded]) == 1)
        self.assertEqual(len([lead for lead in leads if lead["id"] in self.rt.loaded]), 1)

    def test_busy_and_unknown_work_excludes_release(self):
        lead = self.lead()
        self.age(lead, IDLE_SECONDS + 60)
        with self.rt.lock, self.rt.db() as db:
            agent = self.rt.agent(lead["id"], db)
            agent["inFlight"] = True
            self.rt.put(db, "agents", agent)
        self.assertEqual(release_agent(self.rt, lead["id"])["status"], "blocked")
        with self.rt.lock, self.rt.db() as db:
            agent = self.rt.agent(lead["id"], db)
            agent["inFlight"] = False
            self.rt.put(db, "agents", agent)
            db.execute("INSERT INTO runtime_events VALUES (?,?,?,?,?,?,?,?,?)",
                       ("unknown-release", lead["id"], "user", "x", "uncertain", time.time(), agent["epoch"], None, None))
        self.assertEqual(release_agent(self.rt, lead["id"])["reason"], "pending or unknown input")
        with self.rt.db() as db:
            db.execute("DELETE FROM runtime_events WHERE id='unknown-release'")
        self.rt.server.terminals = [{"processId": "open"}]
        self.assertEqual(release_agent(self.rt, lead["id"])["reason"], "native background command is active")
        self.assertFalse(any(method == "thread/unsubscribe" for method, _ in self.rt.server.calls))

    def test_studio_task_monitor_request_and_native_queue_exclude_release(self):
        lead = self.lead()
        self.age(lead, IDLE_SECONDS + 60)
        cases = (("runtime_tasks", {"id": "task", "agent": lead["id"], "status": "running"}),
                 ("runtime_monitors", {"id": "monitor", "agent": lead["id"], "status": "running"}),
                 ("runtime_requests", {"id": "request", "agent": lead["id"], "status": "pending"}),
                 ("runtime_tool_requests", {"id": "tool", "agent": lead["id"],
                                            "stage": "interrupted", "outcome": "unknown"}),
                 ("runtime_work", {"id": "work", "owner": lead["id"], "status": "ready"}))
        for table, row in cases:
            with self.rt.db() as db:
                db.execute(f"INSERT INTO {table} VALUES (?,?)", (row["id"], json.dumps(row)))
            self.assertEqual(release_agent(self.rt, lead["id"])["status"], "blocked", table)
            with self.rt.db() as db:
                db.execute(f"DELETE FROM {table} WHERE id=?", (row["id"],))
        self.rt.server.queue = [{"id": "queued-native"}]
        self.assertEqual(release_agent(self.rt, lead["id"])["reason"], "native input is pending")
        self.assertFalse(any(method == "thread/unsubscribe" for method, _ in self.rt.server.calls))

    def test_lost_unsubscribe_response_retries_only_unsubscribe_before_resume(self):
        lead = self.lead()
        self.age(lead, IDLE_SECONDS + 60)
        self.rt.server.unsubscribe_fail_once = True
        self.assertEqual(release_agent(self.rt, lead["id"])["status"], "unknown")
        self.assertIn(lead["id"], self.rt.loaded)
        before = len(self.rt.server.calls)
        self.rt.send(lead["id"], "Continue", "release-unknown")
        fixture.eventually(lambda: any(method == "thread/resume" for method, _ in self.rt.server.calls[before:]))
        methods = [method for method, _ in self.rt.server.calls[before:]]
        self.assertLess(methods.index("thread/unsubscribe"), methods.index("thread/resume"))
        fixture.eventually(lambda: any(method == "turn/start" for method, _ in self.rt.server.calls[before:]))
        methods = [method for method, _ in self.rt.server.calls[before:]]
        self.assertEqual(methods.count("turn/start"), 1)

    def test_reset_requires_reason_and_native_closure_before_resume(self):
        lead = self.lead()
        worker = self.worker(lead)
        with self.assertRaisesRegex(ValueError, "tool reset reason"):
            manage_agent(self.rt, lead["id"], {"action": "reset_tools", "agent_id": worker["id"]})
        result = manage_agent(self.rt, lead["id"], {"action": "reset_tools", "agent_id": worker["id"],
                                                        "reason": "Too many active cells"})
        self.assertEqual(result["status"], "released")
        self.assertTrue(self.rt.agent(worker["id"])["nativeRelease"]["resetPending"])
        self.assertNotIn(worker["id"], self.rt.loaded)
        self.rt.send(worker["id"], "Continue", "reset-wait")
        self.rt.dispatch()
        self.assertFalse(any(method == "thread/resume" and params.get("threadId") == worker["threadId"]
                             for method, params in self.rt.server.calls))
        self.rt.server.closed_threads.add(worker["threadId"])
        self.rt.server.notify({"method": "thread/closed", "params": {"threadId": worker["threadId"]}})
        fixture.eventually(lambda: not self.rt.agent(worker["id"])["nativeRelease"]["resetPending"])
        self.rt.dispatch()
        fixture.eventually(lambda: any(method == "thread/resume" and params.get("threadId") == worker["threadId"]
                               for method, params in self.rt.server.calls))

    def test_reset_adopts_an_already_released_idle_worker(self):
        lead = self.lead()
        worker = self.worker(lead)
        self.age(worker, IDLE_SECONDS + 60)
        self.assertEqual(release_agent(self.rt, worker["id"])["status"], "released")
        result = manage_agent(self.rt, lead["id"], {"action": "reset_tools", "agent_id": worker["id"],
                                                        "reason": "Restart exhausted cells"})
        self.assertEqual(result["status"], "released")
        self.assertTrue(self.rt.agent(worker["id"])["nativeRelease"]["resetPending"])


if __name__ == "__main__":
    unittest.main()
