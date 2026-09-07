#!/usr/bin/env python3
"""Limit refresh recovery and notification ordering. Isolated state, no model calls."""

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
                 "at": time.time(), "error": error}
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

    def test_delayed_notification_does_not_replace_newer_read(self):
        cached = self.cache()
        self.runtime.notification({"method": "account/rateLimits/updated",
            "_studioReceivedAt": cached["at"] - 180,
            "params": {"rateLimits": {"primary": {"usedPercent": 99}}}})
        self.assertIs(self.runtime.rate_limits_for(), cached)
        with self.runtime.db() as db:
            row = db.execute("SELECT record FROM analytics_limits ORDER BY rowid DESC LIMIT 1").fetchone()
        import json
        self.assertTrue(json.loads(row[0])["ignoredAsStale"])

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
