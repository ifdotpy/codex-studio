#!/usr/bin/env python3
"""New account sign-ins preserve existing profiles and durable request identity."""
import importlib.util
import json
from pathlib import Path
import sys
import threading
import unittest
import uuid
from unittest.mock import patch

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location("account_fixture", Path(__file__).with_name("accounts-contract.py"))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)

class LoginContract(fixture.AccountsContract):
    def runtime(self, call):
        store = self.store
        class Runtime:
            def connect(self, key):
                class Server:
                    def call(self, method, params, timeout):
                        return call(key, method, params)
                return Server()
        return Runtime()

    def response(self, key):
        return {"type": "chatgptDeviceCode", "loginId": key,
                "verificationUrl": "https://auth.openai.com/codex/device", "userCode": "TEST-CODE"}

    def test_start_reply_lost_is_reconciled_after_restart_without_replay(self):
        calls = []
        def call(key, method, params):
            calls.append(key)
            fixture.auth(self.store.home(key), "second-account", "second@example.invalid")
            raise TimeoutError("SECRET native response")
        request = str(uuid.uuid4())
        original = (self.primary / "auth.json").read_bytes()
        result = self.store.start_login(self.runtime(call), request)
        self.assertEqual(result["status"], "ready")
        restarted = fixture.AccountStore(self.root / "state")
        self.assertEqual(restarted.start_login(self.runtime(call), request)["status"], "ready")
        self.assertEqual(len(calls), 1)
        self.assertEqual(len(restarted.snapshot()["accounts"]), 2)
        self.assertEqual((self.primary / "auth.json").read_bytes(), original)
        self.assertEqual(restarted.default(), "default")
        self.assertNotIn("SECRET", json.dumps(restarted.snapshot()))

    def test_early_completion_is_not_overwritten_by_pending_reply(self):
        def call(key, method, params):
            fixture.auth(self.store.home(key), "early-account")
            self.store.login_completed(key, {"loginId": key, "success": True})
            return self.response(key)
        result = self.store.start_login(self.runtime(call), str(uuid.uuid4()))
        self.assertEqual(result["status"], "ready")
        self.assertNotIn("userCode", result)

    def test_duplicate_account_is_not_added_as_another_parallel_identity(self):
        result = self.store.start_login(self.runtime(lambda key, *_: self.response(key)), str(uuid.uuid4()))
        fixture.auth(self.store.home(result["accountKey"]), "account-one")
        self.store.login_completed(result["accountKey"], {"success": True, "loginId": result["loginId"]})
        snapshot = self.store.snapshot()
        self.assertEqual(len(snapshot["accounts"]), 1)
        self.assertEqual(snapshot["logins"][0]["status"], "duplicate")
        self.assertEqual(snapshot["logins"][0]["resolvedAccountKey"], "default")
        self.assertEqual(len(self.store.snapshot()["accounts"]), 1)

    def test_slow_sign_in_does_not_block_another_account_or_repeat_same_request(self):
        entered, release = threading.Event(), threading.Event()
        calls = []
        def call(key, *_):
            calls.append(key)
            if len(calls) == 1:
                entered.set()
                if not release.wait(3): raise TimeoutError()
            return self.response(key)
        runtime = self.runtime(call)
        first = str(uuid.uuid4())
        worker = threading.Thread(target=self.store.start_login, args=(runtime, first))
        worker.start()
        try:
            self.assertTrue(entered.wait(1))
            self.assertEqual(self.store.start_login(runtime, first)["status"], "starting")
            second = self.store.start_login(runtime, str(uuid.uuid4()))
            self.assertEqual(second["status"], "pending")
            self.assertEqual(len(calls), 2)
            self.assertNotEqual(calls[0], calls[1])
            self.assertEqual(self.store.get("default")["status"], "ready")
        finally:
            release.set(); worker.join(2)

    def test_cancel_targets_only_its_native_login_and_survives_restart(self):
        calls = []
        def call(key, method, params):
            calls.append((key, method, params))
            return {"status": "canceled"} if method.endswith("cancel") else self.response(key)
        runtime = self.runtime(call)
        request = str(uuid.uuid4())
        result = self.store.start_login(runtime, request)
        cancelled = self.store.cancel_login(runtime, request)
        self.assertEqual(cancelled["status"], "cancelled")
        self.assertEqual(calls[-1], (result["accountKey"], "account/login/cancel", {"loginId": result["loginId"]}))
        self.assertEqual(self.store.cancel_login(runtime, request)["status"], "cancelled")
        self.assertEqual(len(calls), 2)
        restarted = fixture.AccountStore(self.root / "state")
        self.assertEqual(restarted.snapshot()["logins"][0]["status"], "cancelled")
        self.assertEqual(len(restarted.list()), 1)

    def test_unknown_cancel_is_not_reported_as_cancelled(self):
        def call(key, method, params):
            if method.endswith("cancel"): raise TimeoutError("SECRET")
            return self.response(key)
        runtime = self.runtime(call); request = str(uuid.uuid4())
        self.store.start_login(runtime, request)
        result = self.store.cancel_login(runtime, request)
        self.assertEqual(result["status"], "pending")
        self.assertIn("unconfirmed", result["error"])
        self.assertNotIn("SECRET", json.dumps(result))

    def test_stale_completion_cannot_fail_current_sign_in(self):
        result = self.store.start_login(self.runtime(lambda key, *_: self.response(key)), str(uuid.uuid4()))
        self.store.login_completed(result["accountKey"], {"success": False, "loginId": "another-login"})
        self.assertEqual(self.store.snapshot()["logins"][0]["status"], "pending")

if __name__ == "__main__":
    unittest.main()
