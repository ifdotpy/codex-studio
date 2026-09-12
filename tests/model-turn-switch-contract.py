#!/usr/bin/env python3
"""A loaded native thread keeps Studio's selected model for its next turn."""
import importlib.util
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("runtime_fixture", Path(__file__).with_name("runtime-contract.py"))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)

ASTRA = "gpt-6-astra"
SOL = "gpt-5.6-sol"


class LoadedThreadServer(fixture.FakeServer):
    """Resume returns the loaded model; only an explicit turn override changes it."""
    def __init__(self, *args):
        super().__init__(*args)
        self.thread_models = {}
        self.effective_turn_models = []
        self.resume_results = []

    def call(self, method, params, timeout=60):
        if method == "turn/start":
            thread = params["threadId"]
            model = params.get("model", self.thread_models[thread])
            self.effective_turn_models.append(model)
            self.thread_models[thread] = model
        result = super().call(method, params, timeout)
        if method == "thread/start":
            self.thread_models[result["thread"]["id"]] = result["model"]
        elif method == "thread/resume":
            result["model"] = self.thread_models[params["threadId"]]
            self.resume_results.append(result["model"])
        return result


class ModelTurnSwitchContract(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="studio-model-turn-switch-")
        self.root = Path(self.temporary.name)
        self.runtime = fixture.Runtime(self.root, LoadedThreadServer)
        self.server = self.runtime.connect()
        created = self.runtime.create({"name": "Lead", "cwd": str(self.root), "prompt": "Original task",
                                       "model": ASTRA, "effort": "high", "fast_mode": True, "yolo_mode": False})
        self.key = created["id"]
        fixture.eventually(lambda: self.agent().get("turnId") and self.agent()["status"] == "running")
        self.first = self.agent()
        self.first_turn = self.starts()[0]

    def tearDown(self):
        self.runtime.close()
        self.temporary.cleanup()

    def agent(self):
        return self.runtime.agent(self.key)

    def starts(self):
        return [params for method, params in self.server.calls if method == "turn/start"]

    def complete(self):
        current = self.agent()
        self.server.complete(current["threadId"], current["turnId"])
        fixture.eventually(lambda: not self.agent().get("inFlight"))

    def next_turn(self):
        count = len(self.starts())
        self.runtime.send(self.key, "Continue the same task")
        fixture.eventually(lambda: len(self.starts()) == count + 1 and self.agent().get("turnId")
                           and self.agent()["status"] == "running")
        return self.starts()[-1]

    def assert_selected_turn(self, params, model):
        self.assertEqual(self.agent()["model"], model)
        self.assertEqual(params.get("model"), model)
        self.assertEqual(self.server.effective_turn_models[-1], model)
        self.assertEqual(params["threadId"], self.first["threadId"])
        self.assertEqual(self.agent()["threadId"], self.first["threadId"])
        self.assertEqual(self.agent()["accountKey"], self.first["accountKey"])
        for field in ("effort", "serviceTier", "approvalPolicy", "sandboxPolicy"):
            self.assertEqual(params[field], self.first_turn[field], field)
        self.assertEqual(params["effort"], "high")
        self.assertEqual(params["serviceTier"], "priority")
        self.assertEqual(params["approvalPolicy"], "on-request")
        self.assertEqual(params["sandboxPolicy"]["type"], "workspaceWrite")
        self.assertEqual(sum(method == "thread/start" for method, _ in self.server.calls), 1)

    def test_queued_model_survives_old_resume_and_applies_once_to_next_turn(self):
        self.runtime.conversation_settings(self.key, {"model": SOL, "next_turn": True, "request_id": "select-sol"})
        queued = self.agent()
        self.assertEqual(queued["model"], ASTRA)
        self.assertEqual(queued["pendingSettings"]["model"], SOL)
        self.assertEqual(self.server.effective_turn_models, [ASTRA])
        self.complete()
        params = self.next_turn()
        self.assertEqual(self.server.resume_results, [ASTRA])
        self.assertNotIn("pendingSettings", self.agent())
        self.assertNotIn("pendingSettingsAccountKey", self.agent())
        self.assert_selected_turn(params, SOL)
        # A retry of the accepted settings request must not queue another change.
        self.runtime.conversation_settings(self.key, {"model": SOL, "next_turn": True, "request_id": "select-sol"})
        self.assertNotIn("pendingSettings", self.agent())
        self.complete()
        self.assert_selected_turn(self.next_turn(), SOL)
        self.assertEqual(self.server.resume_results, [ASTRA])

    def test_idle_model_change_preserves_same_thread_and_selected_policy(self):
        self.complete()
        updated = self.runtime.conversation_settings(self.key, {"model": SOL})
        self.assertEqual(updated["model"], SOL)
        self.assertEqual(updated["threadId"], self.first["threadId"])
        self.assert_selected_turn(self.next_turn(), SOL)
        self.assertEqual(self.server.resume_results, [ASTRA])

    def test_reverse_model_change_also_overrides_loaded_sol(self):
        self.complete()
        self.runtime.conversation_settings(self.key, {"model": SOL})
        self.assert_selected_turn(self.next_turn(), SOL)
        self.complete()
        self.runtime.conversation_settings(self.key, {"model": ASTRA})
        self.assert_selected_turn(self.next_turn(), ASTRA)
        self.assertEqual(self.server.resume_results, [ASTRA, SOL])

    def test_capacity_retry_uses_selected_model_without_replaying_input(self):
        self.complete()
        self.runtime.conversation_settings(self.key, {"model": SOL})
        self.assert_selected_turn(self.next_turn(), SOL)
        current = self.agent()
        self.server.notify({"method": "turn/completed", "params": {"threadId": current["threadId"],
            "turn": {"id": current["turnId"], "status": "failed",
                     "error": {"message": "Model is at capacity", "codexErrorInfo": "serverOverloaded"}}}})
        retry = self.agent()["capacityRetry"]
        self.runtime.capacity_retry(self.key, retry["id"], "retry")
        fixture.eventually(lambda: len(self.starts()) == 3 and self.agent().get("turnId"))
        params = self.starts()[-1]
        self.assert_selected_turn(params, SOL)
        self.assertEqual(self.server.resume_results, [ASTRA])
        self.assertEqual(params["input"], [])
        self.assertNotIn("clientUserMessageId", params)
        self.runtime.capacity_retry(self.key, retry["id"], "retry")
        self.assertEqual(len(self.starts()), 3)


if __name__ == "__main__":
    unittest.main()
