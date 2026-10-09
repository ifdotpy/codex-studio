#!/usr/bin/env python3
"""Limit refresh recovery and notification ordering. Isolated state, no model calls."""

# Support direct execution without runner-provided PYTHONPATH.
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()


import importlib.util
from pathlib import Path
import time
import threading
from concurrent.futures import ThreadPoolExecutor
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location(
    "accounts_fixture", Path(__file__).with_name("runtime-accounts-contract.py")
)
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
from codex_runtime import ResponseTimeout


class LimitsRefreshContracts(unittest.TestCase):
    setUp = fixture.AccountContracts.setUp
    tearDown = fixture.AccountContracts.tearDown

    def cache(self, error=None):
        value = {"data": {"accountId": "default", "old": True},
                 "at": time.time(), "readAt": time.time(), "error": error}
        with self.runtime.lock:
            self.runtime.set_rate_limits("default", value)
        return self.runtime.rate_limits_for()

    def notify(self, key="default"):
        self.runtime.notification({"method": "account/rateLimits/updated", "params": {
            "rateLimits": {"limitId": "codex", "primary": {"usedPercent": 17}}
        }}, key)

    def timeout(self):
        return ResponseTimeout("account/rateLimits/read response timed out; outcome unknown")

    def test_delayed_notification_uses_receive_time_not_dispatch_time(self):
        received = time.time() - 180
        self.runtime.notification({"method": "account/rateLimits/updated",
            "_studioReceivedAt": received,
            "params": {"rateLimits": {"primary": {"usedPercent": 17}}}})
        self.assertEqual(self.runtime.rate_limits_for()["at"], received)
        server = self.runtime.connect()
        with patch.object(server, "call", return_value={"accountId": "fresh"}) as call:
            result = self.runtime.limits()
        call.assert_called_once()
        self.assertEqual(result["data"]["accountId"], "fresh")

    def test_other_account_limit_update_republishes_workspace_entity(self):
        from codex_sync_entities import put
        with self.runtime.db() as db:
            put(db, "workspace", "current", {"rateLimitsByAccount": {}})
        first = {"at": time.time(), "data": {"accountId": "billing-other", "rateLimits": {"limitId": "claude"}}}
        self.runtime.set_rate_limits(self.other_key, first)
        with self.runtime.db() as db:
            before = db.execute("SELECT seq FROM sync_entities WHERE collection='workspace' AND id='current'").fetchone()[0]
        second = {"at": first["at"] + 1, "data": {"accountId": "billing-other", "rateLimits": {"limitId": "claude", "secondary": {"usedPercent": 20}}}}
        self.runtime.set_rate_limits(self.other_key, second)
        with self.runtime.db() as db:
            seq, payload = db.execute("SELECT seq,payload FROM sync_entities WHERE collection='workspace' AND id='current'").fetchone()
        import json
        self.assertGreater(seq, before)
        self.assertEqual(json.loads(payload)["value"]["rateLimitsByAccount"][self.other_key]["data"], second["data"])
        self.runtime.set_rate_limits(self.other_key, {**second, "at": second["at"] + 1,
                                                      "readAt": second["at"] + 1})
        with self.runtime.db() as db:
            timestamp_seq = db.execute("SELECT seq FROM sync_entities WHERE collection='workspace' AND id='current'").fetchone()[0]
        self.assertEqual(timestamp_seq, seq)
        reset = {**second, "data": {**second["data"], "rateLimits": {
            **second["data"]["rateLimits"], "secondary": {"usedPercent": 20, "resetsAt": 1802000000}}}}
        self.runtime.set_rate_limits(self.other_key, reset)
        with self.runtime.db() as db:
            reset_seq = db.execute("SELECT seq FROM sync_entities WHERE collection='workspace' AND id='current'").fetchone()[0]
        self.assertEqual(reset_seq - seq, 1)
        self.runtime.set_rate_limits(self.other_key, {**reset, "error": "Sign in required"})
        with self.runtime.db() as db:
            error_seq = db.execute("SELECT seq FROM sync_entities WHERE collection='workspace' AND id='current'").fetchone()[0]
        self.assertEqual(error_seq - reset_seq, 1)
        self.runtime.set_rate_limits(self.other_key, {**reset, "error": "Sign in required",
                                                      "data": {**reset["data"], "signedIn": False}})
        with self.runtime.db() as db:
            signout_seq = db.execute("SELECT seq FROM sync_entities WHERE collection='workspace' AND id='current'").fetchone()[0]
        self.assertEqual(signout_seq - error_seq, 1)

    def test_320_equal_notifications_publish_one_workspace_change(self):
        from codex_sync_entities import put
        with self.runtime.db() as db:
            put(db, "workspace", "current", {"rateLimitsByAccount": {}})
            before = db.execute("SELECT seq FROM sync_entities WHERE collection='workspace' AND id='current'").fetchone()[0]
        def notify(used):
            self.runtime.notification({"method": "account/rateLimits/updated", "params": {
                "rateLimits": {"limitId": "claude", "secondary": {
                    "usedPercent": used, "resetsAt": 1802000000, "windowDurationMins": 10080,
                }}
            }}, self.other_key)
        for _ in range(320):
            notify(11)
        with self.runtime.db() as db:
            repeated = db.execute("SELECT seq FROM sync_entities WHERE collection='workspace' AND id='current'").fetchone()[0]
        self.assertEqual(repeated - before, 1)
        notify(12)
        with self.runtime.db() as db:
            changed = db.execute("SELECT seq FROM sync_entities WHERE collection='workspace' AND id='current'").fetchone()[0]
        self.assertEqual(changed - repeated, 1)

    def test_delayed_notification_does_not_replace_newer_read(self):
        cached = self.cache()
        self.runtime.notification({"method": "account/rateLimits/updated",
            "_studioReceivedAt": cached["at"] - 180,
            "params": {"rateLimits": {"primary": {"usedPercent": 99}}}})
        self.assertIs(self.runtime.rate_limits_for(), cached)
        import json
        def stale_saved():
            with self.runtime.db() as db:
                rows = db.execute("SELECT record FROM analytics_limits ORDER BY rowid DESC LIMIT 2").fetchall()
            return any(json.loads(row[0]).get("ignoredAsStale") for row in rows)
        fixture.f.eventually(stale_saved)

    def test_slow_account_does_not_block_other_account(self):
        slow = self.runtime.connect()
        started, release = threading.Event(), threading.Event()

        def read(*args, **kwargs):
            started.set()
            if not release.wait(5):
                raise TimeoutError("Fixture read was not released")
            return {"accountId": "default"}

        with patch.object(slow, "call", side_effect=read):
            with ThreadPoolExecutor(max_workers=2) as pool:
                pending = pool.submit(self.runtime.limits)
                try:
                    self.assertTrue(started.wait(2))
                    other = pool.submit(self.runtime.limits, self.other_key)
                    self.assertEqual(other.result(timeout=2)["accountKey"], self.other_key)
                    self.assertFalse(pending.done())
                finally:
                    release.set()
                self.assertEqual(pending.result(timeout=2)["data"]["accountId"], "default")

    def test_successful_recent_cache_does_not_read(self):
        cached = self.cache()
        with patch.object(self.runtime, "connect") as connect:
            self.assertIs(self.runtime.limits(), cached)
        connect.assert_not_called()

    def test_many_clients_share_one_successful_read(self):
        server = self.runtime.connect()
        with patch.object(server, "call", return_value={"accountId": "default"}) as call:
            with ThreadPoolExecutor(max_workers=12) as pool:
                values = list(pool.map(lambda _: self.runtime.limits(), range(40)))
        self.assertEqual(call.call_count, 1)
        self.assertTrue(all(value["data"]["accountId"] == "default" for value in values))

    def test_error_cooldown_prevents_client_retry_storm(self):
        server = self.runtime.connect()
        with patch.object(server, "call", side_effect=RuntimeError("Offline")) as call:
            with ThreadPoolExecutor(max_workers=12) as pool:
                values = list(pool.map(lambda _: self.runtime.limits(), range(40)))
        self.assertEqual(call.call_count, 1)
        self.assertTrue(all(value["error"] == "Offline" for value in values))
        self.assertTrue(all(value["at"] is None for value in values))
        with self.runtime.lock:
            self.runtime.rate_limits_for()["checkedAt"] -= 31
        with patch.object(server, "call", return_value={"accountId": "default"}) as call:
            self.assertIsNone(self.runtime.limits()["error"])
        self.assertEqual(call.call_count, 1)

    def test_recent_error_cache_does_not_block_recovery(self):
        self.cache("Earlier timeout")
        server = self.runtime.connect()
        with patch.object(server, "call", return_value={"accountId": "default"}) as call:
            result = self.runtime.limits()
        self.assertIsNone(result["error"])
        self.assertNotIn("old", result["data"])
        call.assert_called_once_with("account/rateLimits/read", {}, timeout=10)

    def test_timeout_retries_only_the_read_once(self):
        server = self.runtime.connect()
        fresh = {"accountId": "default", "fresh": True}
        with patch.object(server, "call", side_effect=[self.timeout(), fresh]) as call:
            result = self.runtime.limits()
        self.assertEqual(result["data"], fresh)
        self.assertIsNone(result["error"])
        self.assertEqual(call.call_count, 2)
        self.assertTrue(all(args.args == ("account/rateLimits/read", {}) for args in call.call_args_list))

    def test_repeated_timeout_preserves_last_success_and_timestamp(self):
        cached = self.cache()
        server = self.runtime.connect()
        with patch.object(server, "call", side_effect=self.timeout()) as call:
            result = self.runtime.limits(force=True)
        self.assertEqual(call.call_count, 2)
        self.assertEqual(result["data"], cached["data"])
        self.assertEqual(result["at"], cached["at"])
        self.assertIn("timed out", result["error"])

    def test_non_timeout_error_is_not_retried(self):
        server = self.runtime.connect()
        with patch.object(server, "call", side_effect=RuntimeError("Sign in required")) as call:
            result = self.runtime.limits()
        self.assertEqual(call.call_count, 1)
        self.assertEqual(result["error"], "Sign in required")
        self.assertIsNone(result["at"])

    def test_live_notification_survives_timeout(self):
        self.cache()
        server = self.runtime.connect()

        def read(*args, **kwargs):
            self.notify()
            raise self.timeout()

        with patch.object(server, "call", side_effect=read) as call:
            result = self.runtime.limits(force=True)
        self.assertEqual(call.call_count, 1)
        self.assertIsNone(result["error"])
        self.assertEqual(result["data"]["rateLimits"]["primary"]["usedPercent"], 17)

    def test_live_notification_survives_older_read_response(self):
        server = self.runtime.connect()

        def read(*args, **kwargs):
            self.notify()
            return {"rateLimits": {"primary": {"usedPercent": 1}}}

        with patch.object(server, "call", side_effect=read):
            result = self.runtime.limits()
        self.assertIsNone(result["error"])
        self.assertEqual(result["data"]["rateLimits"]["primary"]["usedPercent"], 17)

    def test_notifications_do_not_hide_reset_credits_forever(self):
        # Live 2026-10-02: notifications kept the cache fresh, so the full read
        # never ran and the account had no reset credits or account ID.
        self.notify()
        server = self.runtime.connect()
        full = {"accountId": "acct", "rateLimitResetCredits": {"availableCount": 1, "credits": []},
                "rateLimits": {"primary": {"usedPercent": 1}}}
        with patch.object(server, "call", return_value=full) as call:
            result = self.runtime.limits()
        call.assert_called_once()
        self.assertEqual(result["data"]["accountId"], "acct")
        self.assertEqual(result["data"]["rateLimitResetCredits"]["availableCount"], 1)
        self.notify()
        kept = self.runtime.rate_limits_for()
        self.assertEqual(kept["data"]["rateLimitResetCredits"]["availableCount"], 1)
        with patch.object(server, "call") as again:
            self.assertEqual(self.runtime.limits()["data"]["accountId"], "acct")
        again.assert_not_called()

    def test_read_keeps_account_fields_when_notification_arrives_meanwhile(self):
        server = self.runtime.connect()

        def read(*args, **kwargs):
            self.notify()
            return {"accountId": "acct", "rateLimitResetCredits": {"availableCount": 2},
                    "rateLimits": {"primary": {"usedPercent": 1}}}

        with patch.object(server, "call", side_effect=read):
            result = self.runtime.limits()
        self.assertEqual(result["data"]["rateLimits"]["primary"]["usedPercent"], 17)
        self.assertEqual(result["data"]["rateLimitResetCredits"]["availableCount"], 2)
        self.assertEqual(result["data"]["accountId"], "acct")

    def test_other_account_notification_does_not_hide_read_failure(self):
        server = self.runtime.connect()

        def read(*args, **kwargs):
            self.notify(self.other_key)
            raise self.timeout()

        with patch.object(server, "call", side_effect=read) as call:
            result = self.runtime.limits()
        self.assertEqual(call.call_count, 2)
        self.assertIsNone(result["data"])
        self.assertIn("timed out", result["error"])
        self.assertIsNone(self.runtime.rate_limits_for(self.other_key)["error"])


if __name__ == "__main__":
    unittest.main(verbosity=2)
