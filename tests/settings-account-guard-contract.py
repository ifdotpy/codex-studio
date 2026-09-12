#!/usr/bin/env python3
"""A stale account view cannot change settings on the destination account."""
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location("runtime_fixture", Path(__file__).with_name("runtime-contract.py"))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class SettingsAccountGuardContract(fixture.RuntimeContract):
    def test_stale_account_rejects_each_settings_write_before_catalog(self):
        a = self.runtime.new_lead({})
        before = self.runtime.agent(a["id"])
        def unavailable(*args):
            self.fail("Stale account must fail before native metadata access")
        self.runtime.catalog = unavailable
        for values in [{"model": "gpt-5.6-sol"}, {"yolo_mode": False},
                       {"worker_defaults": {"model": None, "effort": None, "fast_mode": False}},
                       {"model": "gpt-5.6-sol", "next_turn": True, "request_id": "stale"}]:
            with self.subTest(values=values), self.assertRaisesRegex(ValueError, "account changed"):
                self.runtime.conversation_settings(a["id"], {**values, "expected_account_key": "old-account"})
            self.assertEqual(self.runtime.agent(a["id"]), before)

    def test_old_receipt_cannot_hide_account_change(self):
        a = self.runtime.new_lead({})
        request = {"model": "gpt-5.6-sol", "next_turn": True,
                   "request_id": "accepted", "expected_account_key": "default"}
        self.runtime.conversation_settings(a["id"], request)
        with self.runtime.lock, self.runtime.db() as db:
            current = self.runtime.agent(a["id"], db)
            current["accountKey"] = "destination"
            self.runtime.put(db, "agents", current)
        with self.assertRaisesRegex(ValueError, "account changed"):
            self.runtime.conversation_settings(a["id"], request)

    def test_matching_account_supports_worker_and_default_updates(self):
        a = self.runtime.new_lead({})
        self.runtime.conversation_settings(a["id"], {"expected_account_key": "default",
            "worker_defaults": {"model": None, "effort": "high", "fast_mode": False}})
        worker = self.runtime.create({"prompt": "Review", "role": "reviewer"}, parent=a["id"], defer=True)
        result = self.runtime.conversation_settings(worker["id"],
            {"expected_account_key": "default", "model": "gpt-5.6-sol", "effort": "high"})
        self.assertEqual(result["model"], "gpt-5.6-sol")

    def test_invalid_expected_identity_is_rejected(self):
        a = self.runtime.new_lead({})
        for value in [None, "", [], 1]:
            with self.subTest(value=value), self.assertRaisesRegex(ValueError, "identity"):
                self.runtime.conversation_settings(a["id"], {"expected_account_key": value, "model": "gpt-5.6-sol"})

    def test_new_account_partial_choice_does_not_inherit_old_account_queue(self):
        a = self.runtime.new_lead({"model": "gpt-6-astra"})
        with self.runtime.lock, self.runtime.db() as db:
            current = self.runtime.agent(a["id"], db)
            current.update(pendingSettings={"model": "gpt-5.6-sol", "effort": "high", "nativeEffort": "high", "fastMode": True},
                           pendingSettingsAccountKey="old-account")
            self.runtime.put(db, "agents", current)
        result = self.runtime.conversation_settings(a["id"], {"expected_account_key": "default", "effort": "low",
            "next_turn": True, "request_id": "new-account-choice"})
        self.assertEqual(result["pendingSettings"]["model"], "gpt-6-astra")
        self.assertFalse(result["pendingSettings"]["fastMode"])


if __name__ == "__main__":
    suite = unittest.TestSuite(SettingsAccountGuardContract(name) for name in SettingsAccountGuardContract.__dict__ if name.startswith("test_"))
    raise SystemExit(not unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful())
