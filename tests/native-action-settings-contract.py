#!/usr/bin/env python3
"""Native maintenance actions wait for selected settings on the exact thread."""
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location("settings_fixture", Path(__file__).with_name("prepare-steer-contract.py"))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class NativeActionSettingsContract(fixture.PrepareSteerContract):
    # Reuse lifecycle setup, but run only the distinct settings regressions here.
    def idle(self):
        a = self.lead()
        self.server.complete(a["threadId"], a["turnId"])
        return a

    def select(self, a):
        self.runtime.conversation_settings(a["id"], {"model": "gpt-5.6-sol", "effort": "high",
            "fast_mode": True, "next_turn": True, "request_id": "next-settings"})

    def test_settings_precede_review_and_consume_queued_choice(self):
        a = self.idle()
        self.select(a)
        self.server.hold.add("review/start")
        result = self.runtime.native_action(a["id"], "review")
        self.assertTrue(result["pending"])
        calls = self.server.calls
        settings = [params for method, params in calls if method == "thread/settings/update"][-1]
        self.assertEqual(settings["threadId"], a["threadId"])
        self.assertEqual((settings["model"], settings["effort"], settings["serviceTier"]),
                         ("gpt-5.6-sol", "high", "priority"))
        self.assertEqual(settings["sandboxPolicy"]["type"], "dangerFullAccess")
        methods = [method for method, _ in calls]
        self.assertLess(methods.index("thread/settings/update"), methods.index("review/start"))
        self.assertNotIn("pendingSettings", self.runtime.agent(a["id"]))
        self.runtime.run_native_action(a["id"], dict(self.runtime.agent(a["id"])["startAttempt"]))
        self.assertEqual(self.count("review/start"), 1)
        self.assertEqual(self.count("thread/settings/update"), 1)

    def test_delayed_settings_ack_starts_compaction_once(self):
        a = self.idle()
        self.select(a)
        self.server.hold.update({"thread/settings/update", "thread/compact/start"})
        self.assertTrue(self.runtime.native_action(a["id"], "compact")["pending"])
        self.assertEqual(self.count("thread/compact/start"), 0)
        entry = next(row for row in self.server.delayed if row["method"] == "thread/settings/update")
        entry["future"].set_result({})
        fixture.eventually(lambda: self.count("thread/compact/start") == 1)
        self.assertEqual(self.count("thread/settings/update"), 1)

    def test_stop_before_settings_ack_never_starts_action(self):
        a = self.idle()
        self.server.hold.add("thread/settings/update")
        self.runtime.native_action(a["id"], "review")
        self.runtime.stop(a["id"])
        entry = next(row for row in self.server.delayed if row["method"] == "thread/settings/update")
        entry["future"].set_result({})
        fixture.eventually(lambda: not self.runtime.agent(a["id"]).get("inFlight"))
        self.assertEqual(self.count("review/start"), 0)
        self.assertEqual(self.runtime.agent(a["id"])["status"], "paused")

    def test_old_connection_ack_never_starts_action(self):
        a = self.idle()
        self.server.hold.add("thread/settings/update")
        self.runtime.native_action(a["id"], "review")
        self.runtime.connection_ids["default"] = "changed"
        entry = next(row for row in self.server.delayed if row["method"] == "thread/settings/update")
        entry["future"].set_result({})
        fixture.eventually(lambda: not self.runtime.agent(a["id"]).get("inFlight"))
        self.assertEqual(self.count("review/start"), 0)

    def test_unsupported_settings_method_does_not_start_wrong_model(self):
        a = self.idle()
        self.server.hold.add("thread/settings/update")
        self.runtime.native_action(a["id"], "review")
        entry = next(row for row in self.server.delayed if row["method"] == "thread/settings/update")
        entry["future"].set_exception(RuntimeError("thread/settings/update is unsupported"))
        fixture.eventually(lambda: self.runtime.agent(a["id"])["status"] == "failed")
        self.assertEqual(self.count("review/start"), 0)
        self.assertIn("unsupported", self.runtime.agent(a["id"])["error"])


if __name__ == "__main__":
    suite = unittest.TestSuite(NativeActionSettingsContract(name) for name in NativeActionSettingsContract.__dict__ if name.startswith("test_"))
    raise SystemExit(not unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful())
