#!/usr/bin/env python3
"""Cross-provider worker routing and atomic spawn receipts. No paid calls."""
import copy
import importlib.util
import json
import os
from pathlib import Path
import tempfile
import unittest
import uuid
from unittest.mock import patch

spec = importlib.util.spec_from_file_location(
    "worker_fixture", Path(__file__).with_name("worker-defaults-contract.py"))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)


class CrossProviderWorkers(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="cross-provider-workers-")
        self.root = Path(self.tmp.name).resolve()
        home = self.root / "codex-home"
        home.mkdir()
        (home / "auth.json").write_text(json.dumps({"tokens": {
            "account_id": "fixture-codex", "access_token": "fixture"}}))
        self.env = patch.dict(os.environ, {"CODEX_HOME": str(home)})
        self.env.start()
        self.rt = f.ControlledRuntime(self.root / "state", f.f.FakeServer)
        self.rt.accounts.data["accounts"]["claude-fixture"] = {
            "id": "claude-fixture", "provider": "claude", "status": "ready",
            "home": str(self.root), "accountId": "fixture-claude"}
        original_get = self.rt.accounts.get
        self.account_patch = patch.object(self.rt.accounts, "get", side_effect=lambda key:
            copy.deepcopy(self.rt.accounts.data["accounts"][key]) if key == "claude-fixture"
            else original_get(key))
        self.account_patch.start()
        self.catalog_calls = []
        def catalog(account="default"):
            self.catalog_calls.append(account)
            self.rt.accounts.get(account)
            return {"data": [f.model(name, ("medium", "high", "xhigh"), "medium")
                for name in (["claude-sonnet-4-6", "claude-opus-4-6"]
                             if account == "claude-fixture"
                             else ["gpt-6-luna", "gpt-6-sol", "gpt-6-astra"])]}
        self.rt.catalog = catalog
        self.lead = self.rt.create({"name": "Claude lead", "prompt": "",
            "cwd": str(self.root), "account_key": "claude-fixture",
            "model": "claude-sonnet-4-6"}, draft=True)

    def tearDown(self):
        self.rt.close()
        self.account_patch.stop()
        self.env.stop()
        self.tmp.cleanup()

    def child(self, **values):
        return self.rt.create({"name": "Worker", "prompt": "Review the fixture",
            "role": "reviewer", **values}, parent=self.lead["id"], defer=True)

    def workers(self):
        return [a for a in self.rt.snapshot()["agents"] if a.get("parentId")]

    def spawn(self, workers, call_id=None):
        self.lead = self.rt.prepare(self.rt.agent(self.lead["id"]))
        key = call_id or str(uuid.uuid4())
        self.rt.dynamic({"id": key, "params": {"threadId": self.lead["threadId"],
            "callId": key, "tool": "orchestration_spawn", "arguments": {"agents": workers}}},
            "claude-fixture", self.rt.connection_ids["claude-fixture"])
        return self.rt.servers["claude-fixture"].responses[-1]["result"]

    def select_worker_account(self, key):
        return self.rt.conversation_settings(self.lead["id"], {"worker_defaults": {
            "model": "gpt-6-luna", "effort": "high", "fast_mode": False, "account_key": key}})

    def test_user_selected_account_routes_future_workers_and_preserves_existing(self):
        previous = self.child()
        prior_get = self.rt.accounts.get.side_effect
        self.rt.accounts.data["accounts"]["codex-selected"] = {
            "id": "codex-selected", "provider": "codex", "status": "ready", "home": str(self.root)}
        self.rt.accounts.get.side_effect = lambda key: (
            copy.deepcopy(self.rt.accounts.data["accounts"][key]) if key == "codex-selected" else prior_get(key))
        changed = self.select_worker_account("codex-selected")
        self.assertEqual(changed["workerDefaults"]["accountKey"], "codex-selected")
        self.assertEqual(self.child(model="gpt-6-sol", effort="xhigh")["accountKey"], "codex-selected")
        self.assertEqual(self.rt.agent(previous["id"])["accountKey"], "default")
        with self.assertRaisesRegex(ValueError, "selected in chat settings"):
            self.child(account_key="default")
        self.select_worker_account(None)
        self.assertEqual(self.child()["accountKey"], "default")

    def test_selected_account_validates_model_and_rejects_invalid_choices(self):
        before = self.rt.agent(self.lead["id"])["workerDefaults"]
        for key in ["missing", "claude-fixture", "", 42]:
            with self.subTest(key=key), self.assertRaises(ValueError):
                self.select_worker_account(key)
        self.rt.accounts.data["accounts"]["default"]["disconnected"] = True
        with self.assertRaises(ValueError):
            self.select_worker_account("default")
        self.assertEqual(self.rt.agent(self.lead["id"])["workerDefaults"], before)

    def test_changed_account_choice_during_catalog_rejects_whole_batch(self):
        self.select_worker_account("default")
        original = self.rt.catalog
        def changed(account="default"):
            value = original(account)
            with self.rt.lock, self.rt.db() as db:
                lead = self.rt.agent(self.lead["id"], db)
                lead["workerDefaults"]["accountKey"] = "claude-fixture"
                self.rt.put(db, "agents", lead)
            return value
        self.rt.catalog = changed
        result = self.spawn([{"name": "Worker", "prompt": "Read", "role": "reviewer"}])
        self.assertFalse(result["success"])
        self.assertEqual(self.workers(), [])

    def test_claude_parent_defaults_to_codex_luna_high(self):
        child = self.child()
        self.assertEqual((child["accountKey"], child["provider"], child["model"],
                          child["effort"], child["nativeEffort"]),
                         ("default", "codex", "gpt-6-luna", "high", "high"))
        self.assertEqual((child["parentId"], child["rootId"], child["cwd"]),
                         (self.lead["id"], self.lead["id"], str(self.root)))
        self.assertIn("default", self.catalog_calls)

    def test_orchestrator_selects_model_reasoning_and_account(self):
        codex = self.child(model="gpt-6-sol", effort="xhigh", account_key="default")
        claude = self.child(model="claude-opus-4-6", effort="medium")
        self.assertEqual((codex["accountKey"], codex["model"], codex["effort"]),
                         ("default", "gpt-6-sol", "xhigh"))
        self.assertEqual((claude["accountKey"], claude["provider"], claude["model"], claude["effort"]),
                         ("claude-fixture", "claude", "claude-opus-4-6", "medium"))

    def test_claude_lead_can_save_codex_worker_defaults(self):
        changed = self.rt.conversation_settings(self.lead["id"], {"worker_defaults": {
            "model": "gpt-6-sol", "effort": "xhigh", "fast_mode": False}})
        self.assertEqual(changed["model"], "claude-sonnet-4-6")
        child = self.child()
        self.assertEqual((child["accountKey"], child["model"], child["effort"]),
                         ("default", "gpt-6-sol", "xhigh"))

    def test_codex_lead_can_select_claude_worker(self):
        self.lead = self.rt.create({"name": "Codex lead", "prompt": "",
            "cwd": str(self.root), "account_key": "default", "model": "gpt-6-astra"}, draft=True)
        child = self.child(model="claude-opus-4-6", effort="medium")
        self.assertEqual((child["accountKey"], child["provider"], child["model"]),
                         ("claude-fixture", "claude", "claude-opus-4-6"))

    def test_unknown_disconnected_and_wrong_catalog_targets_reject(self):
        for values in ({"account_key": "missing"},
                       {"account_key": "claude-fixture", "model": "gpt-6-luna"},
                       {"account_key": "default", "model": "gpt-6-sol", "effort": "impossible"}):
            with self.subTest(values=values), self.assertRaises(ValueError):
                self.child(**values)
        self.rt.accounts.data["accounts"]["default"]["disconnected"] = True
        with self.assertRaises(ValueError):
            self.child(account_key="default")
        self.assertEqual(self.workers(), [])

    def test_no_ready_codex_account_does_not_silently_select_claude(self):
        self.rt.accounts.data["accounts"]["default"]["disconnected"] = True
        with self.assertRaises(ValueError):
            self.child()
        self.assertEqual(self.workers(), [])

    def test_unavailable_claude_catalog_does_not_block_codex_luna(self):
        original_catalog = self.rt.catalog
        def catalog(key="default"):
            if key == "claude-fixture":
                raise RuntimeError("Claude catalog unavailable")
            return original_catalog(key)
        with patch.object(self.rt, "catalog", side_effect=catalog):
            child = self.child()
        self.assertEqual((child["accountKey"], child["model"], child["effort"]),
                         ("default", "gpt-6-luna", "high"))

    def test_target_account_rechecked_after_catalog_before_any_batch_write(self):
        for change in ({"disconnected": True}, {"status": "signedOut"}, {"status": "changed"}):
            with self.subTest(change=change):
                original_get, original_catalog = self.rt.accounts.get, self.rt.catalog
                after_catalog = False
                def get(key):
                    row = original_get(key)
                    return {**row, **change} if key == "default" and after_catalog else row
                def catalog(key="default"):
                    nonlocal after_catalog
                    value = original_catalog(key)
                    if key == "default":
                        after_catalog = True
                    return value
                with patch.object(self.rt.accounts, "get", side_effect=get), patch.object(self.rt, "catalog", side_effect=catalog):
                    result = self.spawn([
                        {"name": "Claude", "prompt": "Review fixture", "role": "reviewer",
                         "model": "claude-opus-4-6", "account_key": "claude-fixture"},
                        {"name": "Codex", "prompt": "Review fixture", "role": "reviewer",
                         "account_key": "default"}])
                self.assertFalse(result["success"], result)
                self.assertEqual(self.workers(), [])

    def test_mixed_account_batch_is_atomic_and_dispatches_to_each_provider(self):
        good = {"name": "Codex worker", "prompt": "Review fixture", "role": "reviewer"}
        bad = {**good, "name": "Claude worker", "account_key": "claude-fixture",
               "model": "claude-opus-4-6", "effort": "impossible"}
        result = self.spawn([good, bad])
        self.assertFalse(result["success"], result)
        self.assertEqual(self.workers(), [])
        result = self.spawn([good, {**bad, "effort": "medium"}])
        self.assertTrue(result["success"], result)
        workers = self.workers()
        self.assertEqual({a["accountKey"] for a in workers}, {"default", "claude-fixture"})
        self.rt.dispatch()
        f.f.eventually(lambda: all(self.rt.agent(a["id"])["status"] == "running" for a in workers))
        for key, expected in (("default", "gpt-6-luna"), ("claude-fixture", "claude-opus-4-6")):
            turns = [params for method, params in self.rt.servers[key].calls if method == "turn/start"]
            self.assertEqual(len(turns), 1)
            self.assertEqual(turns[0]["model"], expected)

    def test_spawn_routes_profile_model_and_explicit_override(self):
        profile = self.rt.profiles({"name": "Claude review", "model": "claude-opus-4-6",
                                    "role": "reviewer", "effort": "medium"})
        result = self.spawn([
            {"name": "Profile worker", "prompt": "Review fixture", "role": "reviewer", "profile_id": profile["id"]},
            {"name": "Override worker", "prompt": "Review fixture", "role": "reviewer", "profile_id": profile["id"],
             "model": "gpt-6-sol", "effort": "xhigh"}])
        self.assertTrue(result["success"], result)
        workers = {worker["name"]: worker for worker in self.workers()}
        self.assertEqual((workers["Profile worker"]["accountKey"], workers["Profile worker"]["model"],
                          workers["Profile worker"]["effort"]),
                         ("claude-fixture", "claude-opus-4-6", "medium"))
        self.assertEqual((workers["Override worker"]["accountKey"], workers["Override worker"]["model"],
                          workers["Override worker"]["effort"]),
                         ("default", "gpt-6-sol", "xhigh"))

    def test_codex_result_wakes_claude_and_both_can_send_messages(self):
        result = self.spawn([{"name": "Worker", "prompt": "Review fixture", "role": "reviewer"}])
        self.assertTrue(result["success"], result)
        child = self.workers()[0]
        self.rt.dispatch()
        f.f.eventually(lambda: self.rt.agent(child["id"])["status"] == "running")
        child = self.rt.agent(child["id"])
        codex, claude = self.rt.servers["default"], self.rt.servers["claude-fixture"]
        codex.complete(child["threadId"], child["turnId"], "Codex evidence for Claude")
        self.assertEqual(self.rt.agent(self.lead["id"])["status"], "queued")
        events = [event for event in self.rt.snapshot()["events"] if event["kind"] == "child_result"]
        self.assertEqual(len(events), 1)
        self.assertEqual(events[0]["agent"], self.lead["id"])
        self.rt.dispatch()
        f.f.eventually(lambda: self.rt.agent(self.lead["id"])["status"] == "running")
        lead = self.rt.agent(self.lead["id"])
        turns = [params for method, params in claude.calls if method == "turn/start"]
        self.assertIn("Codex evidence for Claude", turns[-1]["input"][0]["text"])
        claude.complete(lead["threadId"], lead["turnId"], "Reviewed the Codex result")
        downstream = self.rt.chat_message(lead["id"], child["id"], "Check the second case", "cross-down")
        upstream = self.rt.chat_message(child["id"], "parent", "The first case is confirmed", "cross-up")
        self.assertEqual(downstream["deliveries"], {child["id"]: "queued"})
        self.assertEqual(upstream["deliveries"], {lead["id"]: "queued"})
        self.assertEqual(downstream["room"], upstream["room"])
        self.assertEqual([m["sender"] for m in self.rt.chat_read(downstream["room"], lead["id"])["messages"]],
                         [lead["id"], child["id"]])
        self.rt.dispatch()
        f.f.eventually(lambda: all(self.rt.agent(key)["status"] == "running" for key in (lead["id"], child["id"])))
        for server, text in ((codex, "Check the second case"), (claude, "The first case is confirmed")):
            turns = [params for method, params in server.calls if method == "turn/start"]
            self.assertIn(text, turns[-1]["input"][0]["text"])

    def test_spawn_retry_keeps_receipt_and_does_not_duplicate_workers(self):
        workers = [{"name": "Worker", "prompt": "Review fixture", "role": "reviewer"}]
        key = str(uuid.uuid4())
        first = self.spawn(workers, key)
        self.assertTrue(first["success"], first)
        ids = [a["id"] for a in self.workers()]
        second = self.spawn(workers, key)
        self.assertEqual(second, first)
        self.assertEqual([a["id"] for a in self.workers()], ids)
        self.assertEqual(len(ids), 1)


if __name__ == "__main__":
    unittest.main(verbosity=2)
