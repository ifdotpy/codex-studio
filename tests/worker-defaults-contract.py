#!/usr/bin/env python3
"""User defaults, worker overrides and native execution parameters. No model calls."""

import copy
import importlib.util
import json
from pathlib import Path
import tempfile
import threading
import unittest
import uuid

spec = importlib.util.spec_from_file_location(
    "runtime_fixture", Path(__file__).with_name("runtime-contract.py")
)
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)


def model(name, levels=("low", "high"), default="low", fast=True):
    return {
        "model": name,
        "defaultReasoningEffort": default,
        "supportedReasoningEfforts": [{"reasoningEffort": x} for x in levels],
        "serviceTiers": [{"id": "priority"}] if fast else [],
    }


CATALOG = {
    "data": [
        model("gpt-6-astra"),
        model("gpt-5.6-sol"),
        model("worker"),
        model("small", ("medium",), "medium", False),
    ]
}


class ControlledRuntime(f.Runtime):
    def schedule(self):
        while not self.closed:
            self.changed.wait(0.05)
            self.changed.clear()


class WorkerDefaults(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="worker-defaults-")
        self.root = Path(self.tmp.name)
        self.runtime = ControlledRuntime(self.root, f.FakeServer)
        self.runtime.catalog = lambda account="default": copy.deepcopy(CATALOG)
        self.lead = self.runtime.create(
            {"name": "Lead", "prompt": "", "cwd": str(self.root)}, draft=True
        )

    def tearDown(self):
        self.runtime.close()
        self.tmp.cleanup()

    def settings(self, **values):
        return self.runtime.conversation_settings(self.lead["id"], values)

    def defaults(self, model=None, effort=None, fast=False):
        return self.settings(
            worker_defaults={"model": model, "effort": effort, "fast_mode": fast}
        )

    def worker(self, parent=None, **values):
        return self.runtime.create(
            {"name": "Worker", "prompt": "Review", "role": "reviewer", **values},
            parent=parent or self.lead["id"],
            defer=True,
        )

    def test_latest_root_defaults_not_intermediate_execution(self):
        self.settings(effort="high", fast_mode=True)
        worker = self.worker(model="worker", effort="high", fast_mode=True)
        with self.runtime.lock, self.runtime.db() as db:
            worker.update(autoWake=True, role="implementer")
            self.runtime.put(db, "agents", worker)
        child = self.worker(parent=worker["id"])
        self.assertEqual(
            (child["model"], child["effort"], child["fastMode"]),
            ("gpt-6-astra", None, False),
        )
        self.defaults("worker", "high", True)
        next_child = self.worker(parent=worker["id"])
        self.assertEqual(
            (next_child["model"], next_child["effort"], next_child["fastMode"]),
            ("worker", "high", True),
        )
        self.assertEqual(self.runtime.agent(child["id"])["model"], "gpt-6-astra")

    def test_explicit_false_null_and_model_fallback(self):
        self.defaults("worker", "high", True)
        child = self.worker(fast_mode=False, effort=None)
        self.assertEqual(
            (child["effort"], child["nativeEffort"], child["fastMode"]),
            (None, "low", False),
        )
        child = self.worker(model="small", fast_mode=False)
        self.assertEqual((child["effort"], child["nativeEffort"]), (None, "medium"))
        with self.assertRaisesRegex(ValueError, "reasoning level"):
            self.worker(model="small", effort="high", fast_mode=False)
        with self.assertRaisesRegex(ValueError, "Fast mode"):
            self.worker(model="small")

    def test_profile_explicit_values_override_defaults_null_does_not(self):
        self.defaults("worker", "high", True)
        profile = self.runtime.profiles(
            {
                "name": "Review",
                "model": "gpt-5.6-sol",
                "role": "reviewer",
                "effort": None,
            }
        )
        child = self.worker(profile_id=profile["id"])
        self.assertEqual(
            (child["model"], child["effort"], child["fastMode"]),
            ("gpt-5.6-sol", "high", True),
        )
        self.runtime.profiles({**profile, "effort": "low"})
        self.assertEqual(self.worker(profile_id=profile["id"])["effort"], "low")
        child = self.worker(
            profile_id=profile["id"], model="worker", effort=None, fast_mode=False
        )
        self.assertEqual(
            (child["model"], child["effort"], child["fastMode"]),
            ("worker", None, False),
        )

    def test_defaults_update_during_turn_preserves_loaded_session(self):
        with self.runtime.lock, self.runtime.db() as db:
            a = self.runtime.agent(self.lead["id"], db)
            a.update(status="running", inFlight=True)
            self.runtime.put(db, "agents", a)
            self.runtime.loaded.add(a["id"])
        changed = self.defaults("worker", "high", True)
        self.assertTrue(changed["inFlight"])
        self.assertIn(a["id"], self.runtime.loaded)
        for values in [
            {"effort": "high"},
            {"fast_mode": True},
            {"model": "gpt-5.6-sol"},
            {
                "effort": "high",
                "worker_defaults": {"model": None, "effort": None, "fast_mode": False},
            },
        ]:
            with self.assertRaisesRegex(ValueError, "Wait for this turn"):
                self.settings(**values)

    def test_invalid_types_modes_and_worker_privileges(self):
        invalid = [
            {"model": []},
            {"model": "hidden"},
            {"effort": 2},
            {"effort": "extreme"},
            {"fast_mode": "true"},
            {"fast_mode": 1},
            {"fast_mode": None},
        ]
        original = self.runtime.agent(self.lead["id"])
        for values in invalid:
            with self.subTest(values=values), self.assertRaises(ValueError):
                self.settings(**values)
        for value in [
            None,
            [],
            {},
            {"model": None, "effort": None, "fast_mode": False, "extra": 1},
            {"model": [], "effort": None, "fast_mode": False},
            {"model": "small", "effort": None, "fast_mode": True},
        ]:
            with self.subTest(value=value), self.assertRaises(ValueError):
                self.settings(worker_defaults=value)
        self.assertEqual(self.runtime.agent(self.lead["id"]), original)
        child = self.worker()
        for values in [
            {"worker_defaults": {"model": None, "effort": None, "fast_mode": False}},
            {"dangerously_skip_rules": True},
            {"cwd": str(self.root)},
            {"isLead": True},
        ]:
            with self.assertRaisesRegex(ValueError, "Only a lead"):
                self.runtime.conversation_settings(child["id"], values)
        with self.assertRaisesRegex(ValueError, "Only the user"):
            self.worker(
                worker_defaults={"model": None, "effort": None, "fast_mode": False}
            )
        with self.assertRaisesRegex(ValueError, "parent account"):
            self.worker(account_key="another-account")

    def test_native_start_resume_and_turn_receive_preferences(self):
        self.settings(effort="high", fast_mode=True)
        lead = self.runtime.prepare(self.runtime.agent(self.lead["id"]))
        first = [
            params
            for method, params in self.runtime.server.calls
            if method == "thread/start"
        ][-1]
        self.assertEqual(
            (first["serviceTier"], first["config"]["model_reasoning_effort"]),
            ("priority", "high"),
        )
        self.assertIs(first["config"]["features.fast_mode"], True)
        self.settings(effort=None, fast_mode=False)
        self.runtime.send(lead["id"], "Continue")
        self.runtime.dispatch()
        f.eventually(lambda: self.runtime.agent(lead["id"])["status"] == "running")
        resume = [
            params
            for method, params in self.runtime.server.calls
            if method == "thread/resume"
        ][-1]
        turn = [
            params
            for method, params in self.runtime.server.calls
            if method == "turn/start"
        ][-1]
        self.assertEqual(
            (resume["serviceTier"], resume["config"]["model_reasoning_effort"]),
            ("default", "low"),
        )
        self.assertEqual((turn["serviceTier"], turn["effort"]), ("default", "low"))
        self.assertEqual(resume["threadId"], lead["threadId"])
        self.assertIsNone(self.runtime.agent(lead["id"])["effort"])

    def test_defaults_persist_and_copy_to_new_conversation(self):
        self.defaults("worker", "high", True)
        self.settings(effort="high", fast_mode=True)
        self.runtime.send(self.lead["id"], "Task")
        new = self.runtime.new_lead({"previous": self.lead["id"]})
        self.assertNotEqual(new["id"], self.lead["id"])
        self.assertEqual(
            new["workerDefaults"],
            {"model": "worker", "effort": "high", "fastMode": True},
        )
        self.assertEqual((new["effort"], new["fastMode"]), ("high", True))
        self.runtime.close()
        self.runtime = ControlledRuntime(self.root, f.FakeServer)
        self.assertEqual(
            self.runtime.agent(new["id"])["workerDefaults"], new["workerDefaults"]
        )

    def test_catalogue_does_not_hold_runtime_lock_and_latest_defaults_win(self):
        def catalog(account):
            acquired = threading.Event()

            def enter():
                with self.runtime.lock:
                    acquired.set()

            thread = threading.Thread(target=enter)
            thread.start()
            self.assertTrue(acquired.wait(1), "Catalogue call held the runtime lock")
            thread.join()
            with self.runtime.lock, self.runtime.db() as db:
                root = self.runtime.agent(self.lead["id"], db)
                root["workerDefaults"] = {
                    "model": "worker",
                    "effort": "high",
                    "fastMode": True,
                }
                self.runtime.put(db, "agents", root)
            return copy.deepcopy(CATALOG)

        self.runtime.catalog = catalog
        child = self.worker()
        self.assertEqual(
            (child["model"], child["effort"], child["fastMode"]),
            ("worker", "high", True),
        )

    def test_model_change_falls_back_and_checks_account_catalogue(self):
        child = self.worker(model="worker", effort="high")
        changed = self.runtime.conversation_settings(child["id"], {"model": "small"})
        self.assertEqual(
            (changed["effort"], changed["nativeEffort"]), ("medium", "medium")
        )
        calls = []
        self.runtime.catalog = lambda account: calls.append(account) or {
            "data": [model("gpt-6-astra")]
        }
        with self.assertRaisesRegex(ValueError, "not available"):
            self.worker(model="worker")
        self.assertEqual(calls, [self.lead["accountKey"]])

    def test_spawn_batch_rejects_invalid_override_before_any_child(self):
        lead = self.runtime.prepare(self.lead)
        self.defaults("worker", "high", True)
        self.assertEqual(
            self.runtime.team(lead["id"])["workerDefaults"],
            {"model": "worker", "effort": "high", "fastMode": True},
        )

        def call(key, agents):
            self.runtime.dynamic(
                {
                    "id": key,
                    "params": {
                        "threadId": lead["threadId"],
                        "callId": key,
                        "tool": "orchestration_spawn",
                        "arguments": {"agents": agents},
                    },
                }
            )
            return self.runtime.server.responses[-1]["result"]

        valid = {"name": "Good", "prompt": "Review", "role": "reviewer"}
        result = call(
            "invalid-batch", [valid, {**valid, "name": "Bad", "fast_mode": "true"}]
        )
        self.assertFalse(result["success"])
        self.assertEqual(len(self.runtime.snapshot()["agents"]), 1)
        result = call(
            "valid-batch",
            [valid, {**valid, "name": "Standard", "fast_mode": False, "effort": None}],
        )
        self.assertTrue(result["success"], result)
        workers = [a for a in self.runtime.snapshot()["agents"] if a["parentId"]]
        self.assertEqual(len(workers), 2)
        self.assertEqual({a["fastMode"] for a in workers}, {True, False})
        self.assertEqual({a["effort"] for a in workers}, {"high", None})

    def test_retried_child_keeps_original_defaults_and_rejects_changed_override(self):
        key = str(uuid.uuid4())
        child = self.worker(id=key)
        self.defaults("worker", "high", True)
        self.assertEqual(self.worker(id=key), child)
        with self.assertRaisesRegex(ValueError, "different execution"):
            self.worker(id=key, fast_mode=True)


if __name__ == "__main__":
    unittest.main(verbosity=2)
