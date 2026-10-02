#!/usr/bin/env python3
"""Idle native subscription release, with a fake Codex app-server."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()


import importlib.util
import concurrent.futures
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
from codex_native_sweep import sweep
from codex_runtime import PreparationPending, Runtime

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
        self.loaded_threads = set()
        self.hold_unsubscribe = False
        self.unresolved_unsubscribe = None

    def submit(self, method, params):
        if method == "thread/unsubscribe" and self.hold_unsubscribe:
            self.calls.append((method, params))
            self.unresolved_unsubscribe = concurrent.futures.Future()
            return self.unresolved_unsubscribe
        return super().submit(method, params)

    def wait(self, submitted, timeout=60):
        if submitted is self.unresolved_unsubscribe and not submitted.done():
            raise TimeoutError("unsubscribe response pending")
        return super().wait(submitted, timeout)

    def call(self, method, params, timeout=60):
        if method == "thread/loaded/list":
            self.calls.append((method, params))
            return {"data": sorted(self.loaded_threads), "nextCursor": None}
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
            self.loaded_threads.discard(params["threadId"])
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
        # turn/started can set turnId before the turn/start reply binds the batch.
        fixture.eventually(lambda: bool((self.rt.agent(lead["id"]).get("startAttempt") or {}).get("turnId")))
        fixture.eventually(lambda: not self.rt.preparations.get(lead["id"])
                           or self.rt.preparations[lead["id"]]["future"].done())
        self.idle(lead)
        return self.rt.agent(lead["id"])

    def worker(self, lead):
        worker = self.rt.create({"name": "Worker", "prompt": "Work", "role": "reviewer"}, lead["id"])
        fixture.eventually(lambda: self.rt.agent(worker["id"])["status"] == "running")
        fixture.eventually(lambda: bool((self.rt.agent(worker["id"]).get("startAttempt") or {}).get("turnId")))
        fixture.eventually(lambda: not self.rt.preparations.get(worker["id"])
                           or self.rt.preparations[worker["id"]]["future"].done())
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

    def test_delayed_release_close_does_not_fail_new_preparation(self):
        lead = self.lead()
        self.age(lead, IDLE_SECONDS + 60)
        self.assertEqual(release_agent(self.rt, lead["id"])["status"], "released")

        resume = concurrent.futures.Future()
        original_submit = self.rt.server.submit

        def submit(method, params):
            if method == "thread/resume":
                self.rt.server.calls.append((method, params))
                return resume
            return original_submit(method, params)

        self.rt.server.submit = submit
        self.rt.send(lead["id"], "Continue after release", "release-race")
        self.rt.dispatch()
        fixture.eventually(lambda: any(method == "thread/resume" for method, _ in self.rt.server.calls))
        operation = self.rt.preparations[lead["id"]]
        current = self.rt.agent(lead["id"])
        self.assertTrue(self.rt.operation_current(current, operation))
        self.assertEqual(current["prepareAttempt"], operation["id"])
        self.assertEqual(current["nativeRelease"]["phase"], "released")

        # The unsubscribe's close notification can arrive after resume starts.
        self.rt.server.notify({"method": "thread/closed", "params": {"threadId": lead["threadId"]}})
        fixture.eventually(lambda: self.rt.agent(lead["id"]).get("nativeRelease", {}).get("closedAt"))
        self.assertFalse(operation.get("unloaded"))
        self.assertFalse(self.rt.agent(lead["id"])["startAttempt"]["submitted"])
        self.assertFalse(any(method == "turn/start" and params.get("clientUserMessageId") == "release-race"
                             for method, params in self.rt.server.calls))
        resume.set_result({"thread": {"id": lead["threadId"]}, "model": lead["model"],
                           "sandbox": {"type": "readOnly"}, "approvalPolicy": "on-request"})

        fixture.eventually(lambda: self.rt.delivery_receipt("release-race")["status"] == "delivered")
        self.assertEqual(self.rt.agent(lead["id"])["status"], "running")
        self.assertTrue(self.rt.agent(lead["id"])["startAttempt"]["submitted"])
        submitted = [params for method, params in self.rt.server.calls
                     if method == "turn/start" and params.get("clientUserMessageId") == "release-race"]
        self.assertEqual(len(submitted), 1)

    def test_release_starting_during_preparation_is_blocked(self):
        lead = self.lead()
        self.rt._native_release_last_scan = time.time()
        with self.rt.lock, self.rt.db() as db:
            current = self.rt.agent(lead["id"], db)
            current["nativeRelease"] = {"id": "prior-release", "phase": "released",
                "threadId": current["threadId"], "accountKey": "default",
                "connectionId": self.rt.connection_ids["default"]}
            self.rt.put(db, "agents", current)
            self.rt.loaded.discard(lead["id"])
        self.rt.preparation_wait_seconds = 0.05
        resume = concurrent.futures.Future()
        original_submit = self.rt.server.submit

        def submit(method, params):
            if method == "thread/resume":
                self.rt.server.calls.append((method, params))
                return resume
            return original_submit(method, params)

        self.rt.server.submit = submit
        with self.assertRaises(PreparationPending):
            self.rt.prepare(self.rt.agent(lead["id"]))
        fixture.eventually(lambda: any(method == "thread/resume" for method, _ in self.rt.server.calls))

        result = release_agent(self.rt, lead["id"], reason="prepare race")
        self.assertEqual(result["status"], "blocked")
        self.assertEqual(result["reason"], "thread preparation")
        self.assertFalse(any(method == "thread/unsubscribe" for method, _ in self.rt.server.calls))

        resume.set_result({"thread": {"id": lead["threadId"]}, "model": lead["model"],
                           "sandbox": {"type": "readOnly"}, "approvalPolicy": "on-request"})
        self.rt.send(lead["id"], "Continue while release checks", "prepare-release-race")
        self.rt.dispatch()
        fixture.eventually(lambda: self.rt.delivery_receipt("prepare-release-race")["status"] == "delivered")

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

    def test_studio_work_and_monitors_release_then_resume(self):
        lead = self.lead()
        self.age(lead, IDLE_SECONDS + 60)
        cases = (("runtime_monitors", {"id": "monitor", "agent": lead["id"], "status": "running"}),
                 ("runtime_work", {"id": "work", "owner": lead["id"], "status": "ready"}))
        for table, row in cases:
            with self.rt.db() as db:
                db.execute(f"INSERT INTO {table} VALUES (?,?)", (row["id"], json.dumps(row)))
        self.assertEqual(release_agent(self.rt, lead["id"])["status"], "released")
        self.assertNotIn(lead["id"], self.rt.loaded)
        self.rt.send(lead["id"], "Continue", "work-resume")
        fixture.eventually(lambda: any(method == "thread/resume" for method, _ in self.rt.server.calls))

    def test_pending_request_and_unknown_tool_exclude_release(self):
        lead = self.lead()
        self.age(lead, IDLE_SECONDS + 60)
        cases = (("runtime_tasks", {"id": "task", "agent": lead["id"], "status": "running"}),
                 ("runtime_requests", {"id": "request", "agent": lead["id"], "status": "pending"}),
                 ("runtime_tool_requests", {"id": "tool", "agent": lead["id"],
                                            "stage": "running", "outcome": "unknown"}))
        for table, row in cases:
            with self.rt.db() as db:
                db.execute(f"INSERT INTO {table} VALUES (?,?)", (row["id"], json.dumps(row)))
            self.assertEqual(release_agent(self.rt, lead["id"])["status"], "blocked", table)
            with self.rt.db() as db:
                db.execute(f"DELETE FROM {table} WHERE id=?", (row["id"],))
        self.rt.server.queue = [{"id": "queued-native"}]
        self.assertEqual(release_agent(self.rt, lead["id"])["reason"], "native input is pending")
        self.assertFalse(any(method == "thread/unsubscribe" for method, _ in self.rt.server.calls))

    def test_failed_unknown_tool_history_does_not_keep_thread_loaded(self):
        lead = self.lead()
        self.age(lead, IDLE_SECONDS + 60)
        with self.rt.db() as db:
            db.execute("INSERT INTO runtime_tool_requests VALUES (?,?)",
                       ("old-tool", json.dumps({"id": "old-tool", "agent": lead["id"],
                                                "stage": "failed", "outcome": "unknown"})))
        self.assertEqual(release_agent(self.rt, lead["id"])["status"], "released")

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

    def test_sweep_releases_untracked_loaded_thread(self):
        self.rt.connect()
        self.rt.server.loaded_threads.add("orphan")
        self.assertEqual(sweep(self.rt), 1)
        self.assertNotIn("orphan", self.rt.server.loaded_threads)
        self.assertEqual([p["threadId"] for m, p in self.rt.server.calls if m == "thread/unsubscribe"],
                         ["orphan"])

    def test_sweep_releases_old_archived_agent_left_in_loaded_cache(self):
        lead = self.lead()
        self.age(lead, IDLE_SECONDS + 60)
        with self.rt.lock, self.rt.db() as db:
            current = self.rt.agent(lead["id"], db)
            current["deletedAt"] = time.time() - IDLE_SECONDS - 60
            current["agentArchive"] = {"at": current["deletedAt"]}
            self.rt.put(db, "agents", current)
        self.rt.server.loaded_threads.add(lead["threadId"])
        self.assertIn(lead["id"], self.rt.loaded)
        self.assertEqual(sweep(self.rt), 1)
        self.assertNotIn(lead["threadId"], self.rt.server.loaded_threads)

    def test_sweep_keeps_recent_agent_missing_from_loaded_cache(self):
        lead = self.lead()
        self.rt.loaded.discard(lead["id"])
        self.rt.server.loaded_threads.add(lead["threadId"])
        self.assertEqual(sweep(self.rt), 0)
        self.age(lead, IDLE_SECONDS + 60)
        self.assertEqual(sweep(self.rt), 1)

    def test_sweep_keeps_active_and_repair_threads(self):
        lead = self.lead()
        self.rt.server.loaded_threads.update({lead["threadId"], "repair-fork", "active-orphan"})
        self.rt.server.active_turns["active-orphan"] = {"id": "turn", "status": "inProgress"}
        with self.rt.lock, self.rt.db() as db:
            current = self.rt.agent(lead["id"], db)
            current["contextRepair"] = {"phase": "submitted"}
            self.rt.put(db, "agents", current)
        self.assertEqual(sweep(self.rt), 0)
        with self.rt.lock, self.rt.db() as db:
            current = self.rt.agent(lead["id"], db)
            current["contextRepair"] = {"phase": "completed"}
            self.rt.put(db, "agents", current)
        self.assertEqual(sweep(self.rt), 1)
        self.assertEqual([p["threadId"] for m, p in self.rt.server.calls if m == "thread/unsubscribe"],
                         ["repair-fork"])

    def test_sweep_keeps_repair_source_during_cleanup(self):
        lead = self.lead()
        self.rt.server.loaded_threads.add("old-source")
        with self.rt.lock, self.rt.db() as db:
            current = self.rt.agent(lead["id"], db)
            current["contextRepair"] = {"phase": "completed", "sourceCleanup": {
                "phase": "submitted", "threadId": "old-source", "accountKey": "default"}}
            self.rt.put(db, "agents", current)
        self.assertEqual(sweep(self.rt), 0)
        self.assertFalse(any(m == "thread/unsubscribe" for m, _ in self.rt.server.calls))

    def test_sweep_keeps_native_tool_fork_account(self):
        self.rt.connect()
        self.rt.server.loaded_threads.add("tool-fork")
        self.rt._native_tools_refreshing = {"default"}
        self.assertEqual(sweep(self.rt), 0)
        self.rt._native_tools_refreshing.clear()
        self.assertEqual(sweep(self.rt), 1)

    def test_sweep_is_bounded_per_tick(self):
        self.rt.connect()
        self.rt.server.loaded_threads.update({"orphan-1", "orphan-2", "orphan-3"})
        self.assertEqual(sweep(self.rt), 2)
        self.assertEqual(len(self.rt.server.loaded_threads), 1)
        self.assertEqual(sweep(self.rt), 1)

    def test_sweep_keeps_background_terminal_and_native_queue(self):
        self.rt.connect()
        self.rt.server.loaded_threads.add("orphan")
        self.rt.server.terminals = [{"processId": "terminal"}]
        self.assertEqual(sweep(self.rt), 0)
        self.rt.server.terminals = []
        self.rt.server.queue = [{"id": "input"}]
        self.assertEqual(sweep(self.rt), 0)
        self.assertFalse(any(m == "thread/unsubscribe" for m, _ in self.rt.server.calls))

    def test_sweep_waits_for_exact_lost_receipt(self):
        self.rt.connect()
        self.rt.server.loaded_threads.add("orphan")
        self.rt.server.hold_unsubscribe = True
        self.assertEqual(sweep(self.rt), 1)
        self.assertEqual(sweep(self.rt), 0)
        self.assertEqual(len([m for m, _ in self.rt.server.calls if m == "thread/unsubscribe"]), 1)
        self.rt.server.unresolved_unsubscribe.set_result({"status": "unsubscribed"})
        self.assertEqual(sweep(self.rt), 0)
        self.assertEqual(len([m for m, _ in self.rt.server.calls if m == "thread/unsubscribe"]), 1)

    def test_sweep_retries_idempotent_unsubscribe_after_failed_receipt(self):
        self.rt.connect()
        self.rt.server.loaded_threads.add("orphan")
        self.rt.server.unsubscribe_fail_once = True
        self.assertEqual(sweep(self.rt), 1)
        self.assertEqual(sweep(self.rt), 1)
        methods = [m for m, _ in self.rt.server.calls]
        self.assertEqual(methods.count("thread/unsubscribe"), 2)
        self.assertNotIn("thread/resume", methods)
        self.assertEqual(sweep(self.rt), 0)
        with self.rt.db() as db:
            phase = db.execute("SELECT json_extract(record,'$.phase') FROM runtime_native_sweeps").fetchone()[0]
        self.assertEqual(phase, "closed")

    def test_sweep_retries_pending_unsubscribe_only_after_recheck(self):
        self.rt.connect()
        self.rt.server.loaded_threads.add("orphan")
        self.rt.server.hold_unsubscribe = True
        self.assertEqual(sweep(self.rt), 1)
        self.assertEqual(sweep(self.rt), 0)
        self.assertEqual(sweep(self.rt, now=time.time() + 91), 1)
        self.assertEqual(len([m for m, _ in self.rt.server.calls if m == "thread/unsubscribe"]), 2)


if __name__ == "__main__":
    unittest.main()
