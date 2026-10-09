#!/usr/bin/env python3
"""Idle native subscription release, with a fake Codex app-server."""

# Support direct execution without runner-provided PYTHONPATH.
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()


import importlib.util
import concurrent.futures
import copy
import json
from pathlib import Path
import sys
import tempfile
import threading
import time
from types import SimpleNamespace
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
sys.path.insert(0, str(SERVER_SOURCE_ROOT))
from codex_agent_management import manage_agent
from codex_native_release import IDLE_SECONDS, _retry_reset, reconcile_unknown, release_agent, tick
from codex_native_sweep import sweep
from codex_runtime import PreparationPending, ResponseTimeout, Runtime

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

    def waiting_reset(self, lead, worker, method="thread/read"):
        original = self.rt.server.call

        def timeout(read_method, params, timeout=60):
            if read_method == method:
                self.rt.server.calls.append((read_method, params))
                raise ResponseTimeout(read_method + " response timed out; outcome unknown")
            return original(read_method, params, timeout)

        with patch.object(self.rt.server, "call", timeout):
            result = manage_agent(self.rt, lead["id"], {
                "action": "reset_tools", "agent_id": worker["id"], "reason": "Reset cells"},
                epoch=self.rt.agent(lead["id"])["epoch"])
        self.assertEqual(result["status"], "waiting")
        with self.rt.lock, self.rt.db() as db:
            current = self.rt.agent(worker["id"], db)
            saved = current["nativeRelease"]
            saved["nextAttemptAt"] = 0
            self.rt.put(db, "agents", current)
        return saved

    def release_calls(self, before):
        return [(method, params) for method, params in self.rt.server.calls[before:]
                if method in {"thread/read", "thread/backgroundTerminals/list", "thread/queue/list",
                              "thread/unsubscribe"}]

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

    def retained_released_lead(self):
        lead = self.lead()
        self.rt.server.supervisor_mode = True
        self.rt.server.proc = SimpleNamespace(root=self.rt.root, handle="account:default", generation=7)
        self.age(lead, IDLE_SECONDS + 60)
        self.assertEqual(release_agent(self.rt, lead["id"])["status"], "released")
        self.rt.connection_ids["default"] = "retained-reconnect"
        self.rt.server._notify = lambda message: self.rt.notification(message, "default", "retained-reconnect")
        return self.rt.agent(lead["id"])

    def test_delayed_release_close_after_retained_reconnect_delivers_input_once(self):
        lead = self.retained_released_lead()
        resume = concurrent.futures.Future()
        original_submit = self.rt.server.submit
        def submit(method, params):
            if method == "thread/resume":
                self.rt.server.calls.append((method, params))
                return resume
            return original_submit(method, params)
        self.rt.server.submit = submit
        self.rt.send(lead["id"], "Continue after retained reconnect", "retained-release-race")
        self.rt.dispatch()
        fixture.eventually(lambda: any(method == "thread/resume" for method, _ in self.rt.server.calls))
        operation = self.rt.preparations[lead["id"]]
        self.rt.server.notify({"method": "thread/closed", "params": {"threadId": lead["threadId"]}})
        fixture.eventually(lambda: self.rt.agent(lead["id"])["nativeRelease"].get("closedAt"))
        self.assertFalse(self.rt.agent(lead["id"])["startAttempt"]["submitted"])
        self.assertFalse(any(method == "turn/start" and params.get("clientUserMessageId") == "retained-release-race"
                             for method, params in self.rt.server.calls))
        resume.set_result({"thread": {"id": lead["threadId"]}, "model": lead["model"]})
        fixture.eventually(lambda: operation["future"].done())
        self.assertIsNone(operation["future"].exception())
        self.assertFalse(operation.get("unloaded"))
        self.assertEqual(operation.get("nativeReleaseId"), lead["nativeRelease"]["id"])
        self.assertEqual(lead["nativeRelease"]["supervisorIdentity"],
                         {"stateDir": str(self.rt.root.resolve()), "handle": "account:default", "generation": 7})
        fixture.eventually(lambda: self.rt.delivery_receipt("retained-release-race")["status"] == "delivered")
        submitted = [params for method, params in self.rt.server.calls if method == "turn/start"
                     and params.get("clientUserMessageId") == "retained-release-race"]
        self.assertEqual(len(submitted), 1)
        self.assertEqual(self.rt.agent(lead["id"])["threadId"], lead["threadId"])
        self.assertEqual(self.rt.agent(lead["id"])["status"], "running")

    def test_reconnect_close_cannot_use_release_from_another_child_or_agent_scope(self):
        lead = self.retained_released_lead()
        initial_turns = sum(method == "turn/start" for method, _ in self.rt.server.calls)
        original_submit = self.rt.server.submit
        cases = (("supervisor", "generation", 8), ("supervisor", "generation", True),
                 ("supervisor", "handle", "account:other"), ("supervisor", "root", self.rt.root / "other"),
                 ("release", "accountKey", "other"), ("release", "targetEpoch", lead["epoch"] + 1),
                 ("release", "targetEpoch", False), ("release", "threadId", "other-thread"),
                 ("release", "targetRootId", "other-root"), ("release", "targetParentId", "other-parent"),
                 ("release", "supervisorIdentity", None), ("release", "closedAt", 0))
        self.rt.preparation_wait_seconds = .01
        for target, field, value in cases:
            with self.subTest(target=target, field=field, value=value):
                self.rt.server.proc = SimpleNamespace(root=self.rt.root, handle="account:default", generation=7)
                current = copy.deepcopy(lead)
                if target == "supervisor":
                    setattr(self.rt.server.proc, field, value)
                else:
                    current["nativeRelease"][field] = value
                with self.rt.lock, self.rt.db() as db:
                    self.rt.put(db, "agents", current)
                    self.rt.loaded.discard(lead["id"])
                resume = concurrent.futures.Future()
                def submit(method, params):
                    if method == "thread/resume":
                        return resume
                    return original_submit(method, params)
                self.rt.server.submit = submit
                with self.assertRaises(PreparationPending):
                    self.rt.prepare(self.rt.agent(lead["id"]))
                operation = self.rt.preparations[lead["id"]]
                self.assertIsNone(operation.get("nativeReleaseId"))
                self.rt.server.notify({"method": "thread/closed", "params": {"threadId": lead["threadId"]}})
                fixture.eventually(lambda: bool(operation.get("unloaded")))
                resume.set_result({"thread": {"id": lead["threadId"]}, "model": lead["model"]})
                fixture.eventually(lambda: operation["future"].done())
                self.assertEqual(str(operation["future"].exception()),
                                 "Thread preparation belongs to an earlier agent state")
        self.assertEqual(sum(method == "turn/start" for method, _ in self.rt.server.calls), initial_turns)

    def test_stop_before_retained_resume_receipt_keeps_input_cancelled(self):
        lead = self.retained_released_lead()
        resume = concurrent.futures.Future()
        original_submit = self.rt.server.submit
        def submit(method, params):
            if method == "thread/resume":
                self.rt.server.calls.append((method, params))
                return resume
            return original_submit(method, params)
        self.rt.server.submit = submit
        self.rt.send(lead["id"], "This input must stay cancelled", "stopped-retained-resume")
        self.rt.dispatch()
        fixture.eventually(lambda: any(method == "thread/resume" for method, _ in self.rt.server.calls))
        operation = self.rt.preparations[lead["id"]]
        self.assertEqual(operation.get("nativeReleaseId"), lead["nativeRelease"]["id"])
        self.rt.stop(lead["id"])
        self.rt.server.notify({"method": "thread/closed", "params": {"threadId": lead["threadId"]}})
        resume.set_result({"thread": {"id": lead["threadId"]}, "model": lead["model"]})
        fixture.eventually(lambda: operation["future"].done())
        self.assertIsInstance(operation["future"].exception(), ValueError)
        fixture.eventually(lambda: self.rt.delivery_receipt("stopped-retained-resume")["status"] == "cancelled")
        actual = self.rt.agent(lead["id"])
        self.assertEqual(actual["epoch"], lead["epoch"] + 1)
        self.assertEqual(actual["status"], "paused")
        self.assertFalse(actual["autoWake"])
        self.assertNotIn(lead["id"], self.rt.loaded)
        self.assertFalse(any(method == "turn/start" and params.get("clientUserMessageId") == "stopped-retained-resume"
                             for method, params in self.rt.server.calls))

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

    def test_not_loaded_reset_settles_exact_closed_blocked_inspection(self):
        lead = self.lead()
        worker = self.worker(lead)
        saved = self.waiting_reset(lead, worker)
        old_error = "thread/read response timed out; outcome unknown"
        with self.rt.lock, self.rt.db() as db:
            current = self.rt.agent(worker["id"], db)
            current["nativeRelease"].update(phase="blocked", resetPending=False, error=old_error)
            self.rt.put(db, "agents", current)
            self.rt.put(db, "tool_requests", {"id": "historical-tool", "agent": worker["id"],
                                               "stage": "failed", "outcome": "unknown"})
        self.rt.server.closed_threads.add(worker["threadId"])
        self.rt.server.notify({"method": "thread/closed", "params": {"threadId": worker["threadId"]}})
        fixture.eventually(lambda: self.rt.agent(worker["id"])["nativeRelease"].get("closedAt"))
        closed = self.rt.agent(worker["id"])["nativeRelease"]
        before = len(self.rt.server.calls)
        result = manage_agent(self.rt, lead["id"], {
            "action": "reset_tools", "agent_id": worker["id"], "reason": "Reset after native closure"},
            epoch=lead["epoch"])
        self.assertEqual(result["status"], "not_loaded")
        settled = self.rt.agent(worker["id"])["nativeRelease"]
        self.assertEqual(settled["id"], saved["id"])
        for key in ("targetEpoch", "threadId", "accountKey", "connectionId", "at", "closedAt",
                    "targetRootId", "targetParentId", "resetBy", "resetActorEpoch", "resetActorScope"):
            self.assertEqual(settled[key], closed[key], key)
        self.assertEqual(settled["phase"], "released")
        self.assertEqual(settled["nativeStatus"], "notLoaded")
        self.assertIsNone(settled["error"])
        self.assertEqual(settled["inspectionError"], old_error)
        self.assertFalse(settled["resetPending"])
        self.assertEqual(self.release_calls(before), [])
        repeat = release_agent(self.rt, worker["id"], reason="Closure is already known")
        self.assertEqual(repeat["status"], "not_loaded")
        self.assertEqual(self.release_calls(before), [])
        self.assertEqual(self.rt.agent(worker["id"])["nativeRelease"], settled)
        with self.rt.read_db() as db:
            historical = json.loads(db.execute("SELECT record FROM runtime_tool_requests WHERE id='historical-tool'").fetchone()[0])
        self.assertEqual(historical["outcome"], "unknown")
        self.rt.send(worker["id"], "Continue once after closure", "not-loaded-reset-once")
        fixture.eventually(lambda: self.rt.delivery_receipt("not-loaded-reset-once")["status"] == "delivered")
        calls = [method for method, params in self.rt.server.calls[before:]
                 if params.get("threadId") == worker["threadId"]]
        self.assertEqual(calls.count("thread/unsubscribe"), 0)
        self.assertEqual(calls.count("thread/resume"), 1)
        self.assertEqual(calls.count("turn/start"), 1)
        self.assertIsNone(self.rt.agent(worker["id"])["nativeRelease"]["error"])

    def test_not_loaded_reset_preserves_unproven_or_changed_release(self):
        lead = self.lead()
        worker = self.worker(lead)
        self.waiting_reset(lead, worker)
        with self.rt.lock, self.rt.db() as db:
            current = self.rt.agent(worker["id"], db)
            current["nativeRelease"].update(phase="blocked", resetPending=False,
                                            error="Original inspection outcome unknown")
            self.rt.put(db, "agents", current)
        self.rt.server.notify({"method": "thread/closed", "params": {"threadId": worker["threadId"]}})
        fixture.eventually(lambda: self.rt.agent(worker["id"])["nativeRelease"].get("closedAt"))
        original = self.rt.agent(worker["id"])
        cases = (("release", "closedAt", None), ("release", "closedAt", True),
                 ("release", "closedAt", float("inf")),
                 ("release", "closedAt", original["nativeRelease"]["at"] - 1),
                 ("release", "at", None), ("release", "id", None),
                 ("release", "submittedAt", time.time()),
                 ("release", "submittedAt", 0),
                 ("release", "resetPending", True), ("release", "targetEpoch", worker["epoch"] + 1),
                 ("release", "threadId", "another-thread"),
                 ("release", "accountKey", "another-account"),
                 ("release", "connectionId", "another-connection"),
                 ("release", "targetRootId", "another-root"),
                 ("release", "targetParentId", "another-parent"),
                 ("agent", "inFlight", True), ("agent", "activeTools", [{"id": "active-tool"}]),
                 ("agent", "nativeFailureHold", True))
        before = len(self.rt.server.calls)
        for scope, field, value in cases:
            with self.subTest(scope=scope, field=field):
                current = json.loads(json.dumps(original))
                (current if scope == "agent" else current["nativeRelease"])[field] = value
                with self.rt.lock, self.rt.db() as db:
                    self.rt.put(db, "agents", current)
                self.assertEqual(release_agent(self.rt, worker["id"], reason="Keep exact proof")["status"], "not_loaded")
                actual = self.rt.agent(worker["id"])
                if field == "nativeFailureHold":
                    self.assertTrue(actual["nativeFailureHold"])
                else:
                    self.assertEqual(actual["nativeRelease"], current["nativeRelease"])
                self.assertEqual(self.release_calls(before), [])

    def test_not_loaded_reset_retires_unsubmitted_inspection_without_claiming_closure(self):
        lead = self.lead()
        worker = self.worker(lead)
        saved = self.waiting_reset(lead, worker)
        old_error = "thread/read response timed out; outcome unknown"
        with self.rt.lock, self.rt.db() as db:
            current = self.rt.agent(worker["id"], db)
            current["nativeRelease"].update(phase="blocked", resetPending=False, error=old_error)
            current.update(status="failed", error="Keep the current native failure", nativeFailureHold=True)
            self.rt.put(db, "agents", current)
            self.rt.loaded.discard(worker["id"])
            self.rt.put(db, "tool_requests", {"id": "old-command-unknown", "agent": worker["id"],
                "stage": "failed", "outcome": "unknown"})
        before = len(self.rt.server.calls)
        result = manage_agent(self.rt, lead["id"], {
            "action": "reset_tools", "agent_id": worker["id"], "reason": "Retire the old inspection"},
            epoch=lead["epoch"])
        self.assertEqual(result["status"], "not_loaded")
        settled = self.rt.agent(worker["id"])["nativeRelease"]
        self.assertIsNone(settled["phase"])
        self.assertEqual(settled["inspectionPhase"], "superseded")
        self.assertIsNone(settled["error"])
        self.assertEqual(settled["inspectionError"], old_error)
        self.assertEqual(self.rt.agent(worker["id"])["error"], "Keep the current native failure")
        self.assertTrue(self.rt.agent(worker["id"])["nativeFailureHold"])
        self.assertEqual(self.rt.agent(worker["id"])["status"], "failed")
        for name in ("id", "at", "targetEpoch", "targetRootId", "targetParentId", "threadId",
                     "accountKey", "connectionId", "resetBy", "resetActorEpoch", "resetActorScope"):
            self.assertEqual(settled[name], saved[name], name)
        for name in ("submittedAt", "closedAt", "releasedAt", "nativeStatus"):
            self.assertNotIn(name, settled, name)
        self.assertEqual(self.release_calls(before), [])
        with self.rt.read_db() as db:
            unknown = json.loads(db.execute("SELECT record FROM runtime_tool_requests WHERE id=?",
                ("old-command-unknown",)).fetchone()[0])
        self.assertEqual(unknown["outcome"], "unknown")
        self.assertEqual(release_agent(self.rt, worker["id"], reason="Repeat inspection retirement"),
                         {"status": "not_loaded"})
        self.assertEqual(self.rt.agent(worker["id"])["nativeRelease"], settled)
        self.rt.send(worker["id"], "Continue after inspection retirement", "superseded-inspection-once")
        fixture.eventually(lambda: self.rt.delivery_receipt("superseded-inspection-once")["status"] == "delivered")
        resumed = self.rt.agent(worker["id"])["nativeRelease"]
        self.assertEqual(resumed["phase"], "resumed")
        self.assertIsNone(resumed["error"])
        self.assertEqual(resumed["id"], saved["id"])
        self.assertEqual(resumed["inspectionError"], old_error)
        for name in ("submittedAt", "closedAt", "releasedAt", "nativeStatus"):
            self.assertNotIn(name, resumed, name)
        calls = [method for method, params in self.rt.server.calls[before:]
                 if params.get("threadId") == worker["threadId"]]
        self.assertEqual(calls.count("thread/resume"), 1)
        self.assertEqual(calls.count("turn/start"), 1)
        self.assertEqual(calls.count("thread/unsubscribe"), 0)

    def test_confirmed_resume_retires_unsubmitted_inspection_error_without_claiming_closure(self):
        lead = self.lead()
        worker = self.worker(lead)
        saved = self.waiting_reset(lead, worker)
        old_error = "thread/read response timed out; outcome unknown"
        with self.rt.lock, self.rt.db() as db:
            current = self.rt.agent(worker["id"], db)
            current["nativeRelease"].update(phase="blocked", resetPending=False, error=old_error)
            self.rt.put(db, "agents", current)
            self.rt.loaded.discard(worker["id"])
        before = len(self.rt.server.calls)
        self.rt.send(worker["id"], "Continue current work", "resume-unsubmitted-inspection")
        fixture.eventually(lambda: self.rt.delivery_receipt("resume-unsubmitted-inspection")["status"] == "delivered")
        resumed = self.rt.agent(worker["id"])["nativeRelease"]
        self.assertEqual(resumed["phase"], "resumed")
        self.assertIsNone(resumed["error"])
        self.assertEqual(resumed["inspectionError"], old_error)
        self.assertEqual(resumed["id"], saved["id"])
        for name in ("submittedAt", "closedAt", "releasedAt", "nativeStatus"):
            self.assertNotIn(name, resumed, name)
        calls = [method for method, params in self.rt.server.calls[before:]
                 if params.get("threadId") == worker["threadId"]]
        self.assertEqual(calls.count("thread/resume"), 1)
        self.assertEqual(calls.count("turn/start"), 1)
        self.assertEqual(calls.count("thread/unsubscribe"), 0)

    def test_unsubmitted_inspection_retirement_preserves_missing_or_unsafe_scope(self):
        lead = self.lead()
        worker = self.worker(lead)
        self.waiting_reset(lead, worker)
        with self.rt.lock, self.rt.db() as db:
            current = self.rt.agent(worker["id"], db)
            current["nativeRelease"].update(phase="blocked", resetPending=False,
                                           error="Keep the original inspection error")
            self.rt.put(db, "agents", current)
            self.rt.loaded.discard(worker["id"])
        original = self.rt.agent(worker["id"])
        missing = object()
        cases = [("release", field, missing) for field in
                 ("id", "at", "targetEpoch", "targetRootId", "targetParentId", "threadId",
                  "accountKey", "connectionId")]
        cases += [("release", "targetEpoch", False), ("release", "targetEpoch", 1),
                  ("release", "threadId", "another-thread"),
                  ("release", "accountKey", "another-account"),
                  ("release", "connectionId", "another-connection"),
                  ("release", "targetRootId", "another-root"),
                  ("release", "targetParentId", "another-parent"),
                  ("release", "at", float("inf")), ("release", "at", True),
                  ("release", "closedAt", 0), ("release", "submittedAt", 0),
                  ("release", "resetPending", True), ("release", "phase", "unknown"),
                  ("release", "phase", "unsubscribing"),
                  ("agent", "autoWake", False), ("agent", "deletedAt", time.time())]
        before = len(self.rt.server.calls)
        for scope, field, value in cases:
            with self.subTest(scope=scope, field=field):
                current = json.loads(json.dumps(original))
                target = current if scope == "agent" else current["nativeRelease"]
                if value is missing:
                    target.pop(field)
                else:
                    target[field] = value
                with self.rt.lock, self.rt.db() as db:
                    self.rt.put(db, "agents", current)
                result = release_agent(self.rt, worker["id"], reason="Only the current inspection")
                self.assertIn(result["status"], {"not_loaded", "unknown"})
                self.assertEqual(self.rt.agent(worker["id"])["nativeRelease"], current["nativeRelease"])
                self.assertEqual(self.release_calls(before), [])
        with self.rt.lock, self.rt.db() as db:
            self.rt.put(db, "agents", original)
        server = self.rt.servers.pop("default")
        try:
            self.assertEqual(release_agent(self.rt, worker["id"], reason="Offline keeps history"),
                             {"status": "not_loaded"})
            self.assertEqual(self.rt.agent(worker["id"])["nativeRelease"], original["nativeRelease"])
        finally:
            self.rt.servers["default"] = server

    def test_resume_does_not_retire_a_different_or_submitted_release(self):
        lead = self.lead()
        worker = self.worker(lead)
        self.waiting_reset(lead, worker)
        with self.rt.lock, self.rt.db() as db:
            current = self.rt.agent(worker["id"], db)
            current["nativeRelease"].update(phase="blocked", resetPending=False,
                                           error="Another operation still has its error")
            current["error"] = "Keep the current agent failure separately"
            self.rt.put(db, "agents", current)
        original = self.rt.agent(worker["id"])
        for field, value in (("targetEpoch", 1), ("threadId", "another-thread"),
                             ("connectionId", "another-connection"), ("submittedAt", time.time())):
            with self.subTest(field=field):
                current = json.loads(json.dumps(original))
                current["nativeRelease"][field] = value
                with self.rt.lock, self.rt.db() as db:
                    self.rt.put(db, "agents", current)
                    self.rt.loaded.discard(worker["id"])
                event_id = "different-inspection-" + field
                self.rt.send(worker["id"], "Continue with one new input", event_id)
                fixture.eventually(lambda: self.rt.delivery_receipt(event_id)["status"] == "delivered")
                actual = self.rt.agent(worker["id"])
                self.assertEqual(actual["nativeRelease"], current["nativeRelease"])
                self.idle(worker)

    def test_stop_before_resume_receipt_keeps_inspection_and_does_not_submit_input(self):
        lead = self.lead()
        worker = self.worker(lead)
        self.waiting_reset(lead, worker)
        with self.rt.lock, self.rt.db() as db:
            current = self.rt.agent(worker["id"], db)
            current["nativeRelease"].update(phase="blocked", resetPending=False,
                                           error="Keep the stopped inspection")
            self.rt.put(db, "agents", current)
            self.rt.loaded.discard(worker["id"])
        saved = self.rt.agent(worker["id"])["nativeRelease"]
        resume = concurrent.futures.Future()
        submit = self.rt.server.submit
        def gated(method, params):
            if method == "thread/resume" and params.get("threadId") == worker["threadId"]:
                self.rt.server.calls.append((method, params))
                return resume
            return submit(method, params)
        with patch.object(self.rt.server, "submit", gated):
            self.rt.send(worker["id"], "This input must remain cancelled", "stopped-inspection-resume")
            fixture.eventually(lambda: worker["id"] in self.rt.preparations)
            operation = self.rt.preparations[worker["id"]]
            self.rt.stop(worker["id"])
            resume.set_result({"thread": {"id": worker["threadId"]}, "model": worker["model"]})
            fixture.eventually(lambda: operation["future"].done())
        actual = self.rt.agent(worker["id"])
        self.assertEqual(actual["nativeRelease"], saved)
        self.assertFalse(actual["autoWake"])
        self.assertEqual(actual["epoch"], worker["epoch"] + 1)
        self.assertNotIn("closedAt", actual["nativeRelease"])
        self.assertFalse(any(method == "turn/start" and params.get("clientUserMessageId") == "stopped-inspection-resume"
                             for method, params in self.rt.server.calls))

    def test_not_loaded_reset_preserves_stop_and_unknown_input(self):
        lead = self.lead()
        worker = self.worker(lead)
        self.waiting_reset(lead, worker)
        with self.rt.lock, self.rt.db() as db:
            current = self.rt.agent(worker["id"], db)
            current["nativeRelease"].update(phase="blocked", resetPending=False,
                                            error="Original inspection outcome unknown")
            self.rt.put(db, "agents", current)
        self.rt.server.notify({"method": "thread/closed", "params": {"threadId": worker["threadId"]}})
        fixture.eventually(lambda: self.rt.agent(worker["id"])["nativeRelease"].get("closedAt"))
        original = self.rt.agent(worker["id"])
        with self.rt.lock, self.rt.db() as db:
            db.execute("INSERT INTO runtime_events VALUES (?,?,?,?,?,?,?,?,?)", (
                "unsettled-input", worker["id"], "user", "Fixture", "uncertain", time.time(),
                worker["epoch"], "old-turn", "Original uncertain input"))
        before = len(self.rt.server.calls)
        self.assertEqual(release_agent(self.rt, worker["id"], reason="Keep uncertain input")["status"], "not_loaded")
        self.assertEqual(self.rt.agent(worker["id"])["nativeRelease"], original["nativeRelease"])
        with self.rt.read_db() as db:
            self.assertEqual(db.execute("SELECT status,error FROM runtime_events WHERE id='unsettled-input'").fetchone()[:],
                             ("uncertain", "Original uncertain input"))
        self.rt.stop(worker["id"])
        stopped = self.rt.agent(worker["id"])
        self.assertEqual(release_agent(self.rt, worker["id"], reason="Keep explicit stop")["status"], "not_loaded")
        self.assertEqual(self.rt.agent(worker["id"])["nativeRelease"], stopped["nativeRelease"])
        self.assertFalse(self.rt.agent(worker["id"])["autoWake"])
        self.assertEqual(self.release_calls(before), [])

    def test_read_timeout_retains_reset_and_retries_full_preflight_once(self):
        lead = self.lead()
        worker = self.worker(lead)
        original = self.rt.server.call
        failed = False

        def call(method, params, timeout=60):
            nonlocal failed
            if method == "thread/read" and not failed:
                failed = True
                self.rt.server.calls.append((method, params))
                raise ResponseTimeout("thread/read response timed out; outcome unknown")
            return original(method, params, timeout)

        before = len(self.rt.server.calls)
        with patch.object(self.rt.server, "call", call):
            result = manage_agent(self.rt, lead["id"], {
                "action": "reset_tools", "agent_id": worker["id"], "reason": "Reset cells"},
                epoch=lead["epoch"])
            self.assertEqual(result["status"], "waiting")
            saved = self.rt.agent(worker["id"])["nativeRelease"]
            self.assertEqual(saved["phase"], "checking")
            self.assertEqual(saved["inspectionPhase"], "inspection_wait")
            self.assertTrue(saved["resetPending"])
            self.assertNotIn("submittedAt", saved)
            self.assertEqual(saved["resetActorEpoch"], lead["epoch"])
            self.assertEqual(saved["targetEpoch"], worker["epoch"])
            self.assertGreaterEqual(saved["nextAttemptAt"], saved["at"] + 30)
            self.assertFalse(any(method == "thread/unsubscribe"
                                 for method, _ in self.rt.server.calls[before:]))
            _retry_reset(self.rt, worker["id"])
            self.assertEqual(len(self.release_calls(before)), 1)
            with self.rt.lock, self.rt.db() as db:
                current = self.rt.agent(worker["id"], db)
                current["nativeRelease"]["nextAttemptAt"] = 0
                self.rt.put(db, "agents", current)
            _retry_reset(self.rt, worker["id"])
        release = self.rt.agent(worker["id"])["nativeRelease"]
        self.assertEqual(release["id"], saved["id"])
        self.assertEqual(release["at"], saved["at"])
        self.assertEqual(release["resetReason"], "Reset cells")
        self.assertEqual(release["resetBy"], lead["id"])
        self.assertEqual(release["phase"], "released")
        self.assertEqual([method for method, _ in self.release_calls(before)], [
            "thread/read", "thread/read", "thread/backgroundTerminals/list", "thread/queue/list",
            "thread/unsubscribe"])
        _retry_reset(self.rt, worker["id"])
        self.assertEqual(sum(method == "thread/unsubscribe"
                             for method, _ in self.rt.server.calls[before:]), 1)

    def test_each_read_only_preflight_timeout_repeats_all_checks(self):
        lead = self.lead()
        for method in ("thread/backgroundTerminals/list", "thread/queue/list"):
            with self.subTest(method=method):
                worker = self.worker(lead)
                saved = self.waiting_reset(lead, worker, method)
                before = len(self.rt.server.calls)
                _retry_reset(self.rt, worker["id"])
                self.assertEqual([call[0] for call in self.release_calls(before)], [
                    "thread/read", "thread/backgroundTerminals/list", "thread/queue/list",
                    "thread/unsubscribe"])
                self.assertEqual(self.rt.agent(worker["id"])["nativeRelease"]["id"], saved["id"])

    def test_inspection_retry_rejects_changed_scope_before_native_calls(self):
        cases = (("target", "epoch", 1), ("target", "threadId", "replacement-thread"),
                 ("target", "accountKey", "replacement-account"),
                 ("target", "rootId", "replacement-root"), ("target", "parentId", "replacement-parent"),
                 ("target", "inFlight", True), ("actor", "epoch", 1),
                 ("actor", "autoWake", False), ("actor", "threadId", "replacement-actor-thread"),
                 ("actor", "accountKey", "replacement-actor-account"),
                 ("actor", "rootId", "replacement-actor-root"), ("connection", "default", "replacement-connection"))
        for owner, field, value in cases:
            with self.subTest(owner=owner, field=field):
                lead = self.lead()
                worker = self.worker(lead)
                saved = self.waiting_reset(lead, worker)
                before = len(self.rt.server.calls)
                old_connection = self.rt.connection_ids["default"]
                if owner == "connection":
                    self.rt.connection_ids[field] = value
                else:
                    with self.rt.lock, self.rt.db() as db:
                        key = lead["id"] if owner == "actor" else worker["id"]
                        current = self.rt.agent(key, db)
                        current[field] = value
                        self.rt.put(db, "agents", current)
                try:
                    _retry_reset(self.rt, worker["id"])
                    release = self.rt.agent(worker["id"])["nativeRelease"]
                    self.assertEqual(release["id"], saved["id"])
                    self.assertEqual(release["phase"], "blocked")
                    self.assertFalse(release["resetPending"])
                    self.assertEqual(self.release_calls(before), [])
                finally:
                    self.rt.connection_ids["default"] = old_connection

    def test_stop_during_retry_preflight_prevents_unsubscribe(self):
        lead = self.lead()
        worker = self.worker(lead)
        saved = self.waiting_reset(lead, worker)
        original = self.rt.server.call
        before = len(self.rt.server.calls)

        def stop_after_read(method, params, timeout=60):
            result = original(method, params, timeout)
            if method == "thread/queue/list":
                self.rt.stop(worker["id"])
            return result

        with patch.object(self.rt.server, "call", stop_after_read):
            _retry_reset(self.rt, worker["id"])
        current = self.rt.agent(worker["id"])
        self.assertEqual(current["nativeRelease"]["id"], saved["id"])
        self.assertFalse(current["nativeRelease"]["resetPending"])
        self.assertFalse(any(method == "thread/unsubscribe"
                             for method, _ in self.rt.server.calls[before:]))

    def test_retry_preserves_native_command_and_pending_input_blockers(self):
        lead = self.lead()
        for kind in ("terminal", "queue", "unknown_input", "unknown_task"):
            with self.subTest(kind=kind):
                worker = self.worker(lead)
                self.waiting_reset(lead, worker)
                before = len(self.rt.server.calls)
                if kind == "terminal":
                    self.rt.server.terminals = [{"processId": "still-running"}]
                elif kind == "queue":
                    self.rt.server.queue = [{"id": "accepted-input"}]
                else:
                    with self.rt.lock, self.rt.db() as db:
                        if kind == "unknown_input":
                            db.execute("INSERT INTO runtime_events VALUES (?,?,?,?,?,?,?,?,?)", (
                                "unknown-reset-" + worker["id"], worker["id"], "user", "x", "uncertain",
                                time.time(), worker["epoch"], None, None))
                        else:
                            self.rt.put(db, "tasks", {"id": "unknown-reset-" + worker["id"],
                                                       "agent": worker["id"], "status": "unknown"})
                try:
                    _retry_reset(self.rt, worker["id"])
                    self.assertEqual(self.rt.agent(worker["id"])["nativeRelease"]["phase"], "blocked")
                    self.assertFalse(any(method == "thread/unsubscribe"
                                         for method, _ in self.rt.server.calls[before:]))
                    if kind in {"unknown_input", "unknown_task"}:
                        self.assertEqual(self.release_calls(before), [])
                finally:
                    self.rt.server.terminals = []
                    self.rt.server.queue = []

    def test_reset_does_not_replace_unknown_unsubscribe_or_reopen_old_blocked(self):
        lead = self.lead()
        worker = self.worker(lead)
        self.rt.server.unsubscribe_fail_once = True
        result = manage_agent(self.rt, lead["id"], {
            "action": "reset_tools", "agent_id": worker["id"], "reason": "Reset cells"})
        self.assertEqual(result["status"], "unknown")
        saved = self.rt.agent(worker["id"])["nativeRelease"]
        before = len(self.rt.server.calls)
        again = manage_agent(self.rt, lead["id"], {
            "action": "reset_tools", "agent_id": worker["id"], "reason": "Second reset"})
        self.assertEqual(again["status"], "unknown")
        self.assertEqual(self.rt.agent(worker["id"])["nativeRelease"], saved)
        self.assertEqual(self.release_calls(before), [])
        _retry_reset(self.rt, worker["id"])
        self.assertEqual(self.rt.agent(worker["id"])["nativeRelease"], saved)
        self.assertEqual(self.release_calls(before), [])
        with self.rt.lock, self.rt.db() as db:
            current = self.rt.agent(worker["id"], db)
            current["nativeRelease"] = {"id": "old-blocked", "phase": "blocked", "resetPending": False,
                                        "error": "thread/read response timed out; outcome unknown"}
            self.rt.put(db, "agents", current)
        _retry_reset(self.rt, worker["id"])
        self.assertEqual(self.release_calls(before), [])
        self.assertEqual(self.rt.agent(worker["id"])["nativeRelease"]["id"], "old-blocked")

    def test_captured_unknown_release_closes_then_send_does_not_repeat_unsubscribe(self):
        lead = self.lead()
        worker = self.worker(lead)
        self.rt.server.unsubscribe_fail_once = True
        result = manage_agent(self.rt, lead["id"], {
            "action": "reset_tools", "agent_id": worker["id"], "reason": "Reset cells"})
        self.assertEqual(result["status"], "unknown")
        saved = self.rt.agent(worker["id"])["nativeRelease"]
        self.rt.server.closed_threads.add(worker["threadId"])
        self.rt.server.notify({"method": "thread/closed", "params": {"threadId": worker["threadId"]}})
        fixture.eventually(lambda: self.rt.agent(worker["id"])["nativeRelease"].get("closedAt"))
        closed = self.rt.agent(worker["id"])["nativeRelease"]
        self.assertFalse(closed["resetPending"])
        before = len(self.rt.server.calls)
        self.rt.send(worker["id"], "Continue after exact native closure", "closed-reset-once")
        fixture.eventually(lambda: self.rt.delivery_receipt("closed-reset-once")["status"] == "delivered")
        methods = [method for method, params in self.rt.server.calls[before:]
                   if params.get("threadId") == worker["threadId"]]
        self.assertEqual(methods.count("thread/unsubscribe"), 0)
        self.assertEqual(methods.count("thread/resume"), 1)
        self.assertEqual(methods.count("turn/start"), 1)
        self.assertEqual(self.rt.agent(worker["id"])["nativeRelease"]["id"], saved["id"])
        self.assertEqual(self.rt.agent(worker["id"])["nativeRelease"]["closedAt"], closed["closedAt"])

    def test_captured_unknown_release_keeps_hold_without_closed_proof(self):
        lead = self.lead()
        worker = self.worker(lead)
        self.rt.server.unsubscribe_fail_once = True
        result = manage_agent(self.rt, lead["id"], {
            "action": "reset_tools", "agent_id": worker["id"], "reason": "Reset cells"})
        self.assertEqual(result["status"], "unknown")
        saved = self.rt.agent(worker["id"])["nativeRelease"]
        before = len(self.rt.server.calls)
        with self.assertRaises(ResponseTimeout):
            reconcile_unknown(self.rt, self.rt.agent(worker["id"]))
        self.assertEqual(self.release_calls(before), [])
        self.assertEqual(self.rt.agent(worker["id"])["nativeRelease"], saved)

    def test_captured_closed_release_rejects_changed_scope_and_invalid_proof(self):
        lead = self.lead()
        worker = self.worker(lead)
        self.rt.server.unsubscribe_fail_once = True
        manage_agent(self.rt, lead["id"], {
            "action": "reset_tools", "agent_id": worker["id"], "reason": "Reset cells"})
        original = self.rt.agent(worker["id"])
        cases = (("agent", "epoch", worker["epoch"] + 1),
                 ("agent", "threadId", "replacement-thread"),
                 ("agent", "accountKey", "replacement-account"),
                 ("agent", "rootId", "replacement-root"),
                 ("agent", "parentId", "replacement-parent"),
                 ("release", "connectionId", "replacement-connection"),
                 ("release", "closedAt", True),
                 ("release", "closedAt", original["nativeRelease"]["at"] - 1),
                 ("release", "resetPending", True),
                 ("release", "id", None))
        before = len(self.rt.server.calls)
        for scope, field, value in cases:
            with self.subTest(scope=scope, field=field):
                current = json.loads(json.dumps(original))
                current["nativeRelease"].update(closedAt=time.time(), resetPending=False)
                (current if scope == "agent" else current["nativeRelease"])[field] = value
                with self.rt.lock, self.rt.db() as db:
                    self.rt.put(db, "agents", current)
                with self.assertRaises(ResponseTimeout):
                    reconcile_unknown(self.rt, current)
                self.assertEqual(self.rt.agent(worker["id"])["nativeRelease"], current["nativeRelease"])
                self.assertEqual(self.release_calls(before), [])

    def test_captured_closed_release_does_not_replace_a_newer_receipt(self):
        lead = self.lead()
        worker = self.worker(lead)
        self.rt.server.unsubscribe_fail_once = True
        manage_agent(self.rt, lead["id"], {
            "action": "reset_tools", "agent_id": worker["id"], "reason": "Reset cells"})
        stale = self.rt.agent(worker["id"])
        with self.rt.lock, self.rt.db() as db:
            current = self.rt.agent(worker["id"], db)
            current["nativeRelease"].update(id="replacement-release", closedAt=time.time(), resetPending=False)
            self.rt.put(db, "agents", current)
        before = len(self.rt.server.calls)
        with self.assertRaises(ResponseTimeout):
            reconcile_unknown(self.rt, stale)
        self.assertEqual(self.rt.agent(worker["id"])["nativeRelease"], current["nativeRelease"])
        self.assertEqual(self.release_calls(before), [])

    def test_read_retry_never_overwrites_a_submitted_receipt(self):
        lead = self.lead()
        worker = self.worker(lead)
        saved = self.waiting_reset(lead, worker)
        original = self.rt.server.call
        receipt = []

        def receipt_during_read(method, params, timeout=60):
            if method == "thread/read":
                with self.rt.lock, self.rt.db() as db:
                    current = self.rt.agent(worker["id"], db)
                    current["nativeRelease"].update(phase="unknown", submittedAt=time.time(),
                                                    error="original unsubscribe outcome unknown")
                    receipt.append(dict(current["nativeRelease"]))
                    self.rt.put(db, "agents", current)
                raise ResponseTimeout("late read timeout")
            return original(method, params, timeout)

        with patch.object(self.rt.server, "call", receipt_during_read):
            _retry_reset(self.rt, worker["id"])
        self.assertEqual(self.rt.agent(worker["id"])["nativeRelease"], receipt[0])
        self.assertEqual(receipt[0]["id"], saved["id"])

    def test_read_retry_has_bounded_backoff_and_keeps_original_request(self):
        lead = self.lead()
        worker = self.worker(lead)
        saved = self.waiting_reset(lead, worker)
        original = self.rt.server.call

        def timeout(method, params, timeout=60):
            if method == "thread/read":
                raise ResponseTimeout("read response pending")
            return original(method, params, timeout)

        with patch.object(self.rt.server, "call", timeout):
            for failure in range(2, 8):
                began = time.time()
                _retry_reset(self.rt, worker["id"])
                current = self.rt.agent(worker["id"])["nativeRelease"]
                self.assertEqual(current["id"], saved["id"])
                self.assertEqual(current["inspectionFailures"], failure)
                delay = min(300, 30 * 2 ** min(failure - 1, 4))
                self.assertGreaterEqual(current["nextAttemptAt"], began + delay)
                self.assertLess(current["nextAttemptAt"], time.time() + delay + 1)
                with self.rt.lock, self.rt.db() as db:
                    agent = self.rt.agent(worker["id"], db)
                    agent["nativeRelease"]["nextAttemptAt"] = 0
                    self.rt.put(db, "agents", agent)
        self.assertFalse(any(method == "thread/unsubscribe" for method, _ in self.rt.server.calls))

    def test_tick_retries_at_most_two_exact_reset_requests(self):
        lead = self.lead()
        workers = [self.worker(lead) for _ in range(3)]
        saved = {worker["id"]: self.waiting_reset(lead, worker) for worker in workers}
        before = len(self.rt.server.calls)
        tick(self.rt, now=time.time() + 31)
        fixture.eventually(lambda: len([method for method, _ in self.rt.server.calls[before:]
                                        if method == "thread/unsubscribe"]) == 2)
        fixture.eventually(lambda: not self.rt._native_release_pending)
        self.assertEqual(sum(method == "thread/unsubscribe"
                             for method, _ in self.rt.server.calls[before:]), 2)
        releases = [self.rt.agent(worker["id"])["nativeRelease"] for worker in workers]
        self.assertEqual(sum(release["phase"] == "checking" for release in releases), 1)
        for worker, release in zip(workers, releases):
            self.assertEqual(release["id"], saved[worker["id"]]["id"])

    def test_sweep_releases_untracked_loaded_thread(self):
        self.rt.connect()
        self.rt.server.loaded_threads.add("orphan")
        self.assertEqual(sweep(self.rt), 1)
        self.assertNotIn("orphan", self.rt.server.loaded_threads)
        self.assertEqual([p["threadId"] for m, p in self.rt.server.calls if m == "thread/unsubscribe"],
                         ["orphan"])

    def test_sweep_reads_provider_without_native_authentication(self):
        self.rt.connect()
        self.rt.server.loaded_threads.add("orphan")
        with patch.object(self.rt.accounts, "get", side_effect=AssertionError("Do not probe native authentication")):
            self.assertEqual(sweep(self.rt), 1)
        self.assertNotIn("orphan", self.rt.server.loaded_threads)

    def test_sweep_keeps_claude_connection_without_authentication(self):
        self.rt.connect()
        with self.rt.accounts.lock:
            self.rt.accounts.data["accounts"]["default"]["provider"] = "claude"
        before = list(self.rt.server.calls)
        with patch.object(self.rt.accounts, "get", side_effect=AssertionError("Do not probe native authentication")):
            self.assertEqual(sweep(self.rt), 0)
        self.assertEqual(self.rt.server.calls, before)

    def test_sweep_registry_wait_does_not_hold_the_runtime_lock(self):
        self.rt.connect()
        entered, release = threading.Event(), threading.Event()
        original_lock = self.rt.accounts.lock
        runtime_lock_available = []
        errors = []
        class RegistryGate:
            def __enter__(self):
                entered.set()
                if not release.wait(3):
                    raise TimeoutError("The registry gate did not release")
                return original_lock.__enter__()
            def __exit__(self, *args):
                return original_lock.__exit__(*args)
        def inspect():
            try:
                sweep(self.rt)
            except BaseException as error:
                errors.append(error)
        with patch.object(self.rt.accounts, "lock", RegistryGate()):
            worker = threading.Thread(target=inspect)
            worker.start()
            try:
                self.assertTrue(entered.wait(2))
                acquired = self.rt.lock.acquire(timeout=.5)
                runtime_lock_available.append(acquired)
                if acquired:
                    self.rt.lock.release()
            finally:
                release.set()
                worker.join(4)
        self.assertEqual(runtime_lock_available, [True])
        self.assertFalse(worker.is_alive())
        self.assertEqual(errors, [])

    def test_sweep_ignores_connection_without_a_registry_identity(self):
        self.rt.connect()
        with self.rt.accounts.lock:
            row = self.rt.accounts.data["accounts"].pop("default")
        try:
            before = list(self.rt.server.calls)
            self.assertEqual(sweep(self.rt), 0)
            self.assertEqual(self.rt.server.calls, before)
        finally:
            with self.rt.accounts.lock:
                self.rt.accounts.data["accounts"]["default"] = row

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
        with self.rt.lock:
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
