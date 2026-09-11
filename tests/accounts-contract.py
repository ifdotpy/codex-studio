#!/usr/bin/env python3
"""Profile discovery and identity boundaries, without real credentials."""

import base64
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch
import uuid
import importlib.util
import threading
import urllib.request
import urllib.error

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
from codex_accounts import AccountStore, auth_metadata


def auth(home, account, email="test@example.invalid"):
    home.mkdir(parents=True, exist_ok=True)
    claims = {
        "email": email,
        "https://api.openai.com/auth": {
            "chatgpt_account_id": account,
            "chatgpt_plan_type": "pro",
        },
    }
    payload = base64.urlsafe_b64encode(json.dumps(claims).encode()).decode().rstrip("=")
    (home / "auth.json").write_text(
        json.dumps(
            {
                "auth_mode": "chatgpt",
                "tokens": {
                    "account_id": account,
                    "id_token": "x." + payload + ".signature",
                    "access_token": "SECRET-ACCESS",
                    "refresh_token": "SECRET-REFRESH",
                },
            }
        )
    )


class AccountsContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.home = self.root / "user"
        self.primary = self.home / ".codex"
        auth(self.primary, "account-one")
        self.env = patch.dict(os.environ, {"CODEX_HOME": str(self.primary)})
        self.env.start()
        self.user = patch("codex_accounts.Path.home", return_value=self.home)
        self.user.start()
        self.store = AccountStore(self.root / "state")

    def tearDown(self):
        self.user.stop()
        self.env.stop()
        self.temp.cleanup()

    def test_discover_three_and_deduplicate_without_copying_auth(self):
        for project, account in [
            ("alpha", "account-one"),
            ("beta", "account-two"),
            ("gamma", "account-three"),
        ]:
            auth(self.home / "Projects" / project / ".codex-profile", account)
        snapshot = self.store.snapshot()
        self.assertEqual(len(snapshot["accounts"]), 3)
        self.assertEqual(
            {a["accountId"] for a in snapshot["accounts"]},
            {"account-one", "account-two", "account-three"},
        )
        serialized = json.dumps(snapshot) + self.store.path.read_text()
        self.assertNotIn("SECRET", serialized)
        self.assertNotIn("id_token", serialized)
        self.assertEqual(list(self.store.root.rglob("auth.json")), [])
        self.assertEqual(self.store.path.stat().st_mode & 0o777, 0o600)

    def test_identity_is_pinned_across_restart_without_discovery(self):
        auth(self.primary, "replacement")
        store = AccountStore(self.root / "state")
        self.assertEqual(store.get("default")["status"], "changed")
        with self.assertRaisesRegex(ValueError, "account changed"):
            store.home("default")
        self.assertEqual(store.get("default")["accountId"], "account-one")

    def test_rotated_tokens_keep_identity(self):
        before = self.store.home("default")
        auth(self.primary, "account-one", "updated@example.invalid")
        self.assertEqual(self.store.home("default"), before)
        self.assertEqual(self.store.get("default")["email"], "updated@example.invalid")

    def test_api_key_change_is_blocked_without_exporting_fingerprint(self):
        home = self.home / "api-profile"
        home.mkdir()
        (home / "auth.json").write_text(json.dumps({"OPENAI_API_KEY": "key-one"}))
        key = self.store.register(str(home))
        self.assertEqual(self.store.get(key)["status"], "ready")
        self.assertNotIn("_credentialIdentity", self.store.get(key))
        (home / "auth.json").write_text(json.dumps({"OPENAI_API_KEY": "key-two"}))
        self.assertEqual(
            AccountStore(self.root / "state").get(key)["status"], "changed"
        )
        with self.assertRaises(ValueError):
            self.store.home(key)

    def test_default_is_durable_and_does_not_modify_global_auth(self):
        home = self.home / "other"
        auth(home, "account-two")
        before = (self.primary / "auth.json").read_bytes()
        key = self.store.register(str(home))
        self.store.default(key)
        self.assertEqual(AccountStore(self.root / "state").default(), key)
        self.assertEqual((self.primary / "auth.json").read_bytes(), before)
        self.assertEqual(self.store.register(str(home)), key)

    def test_invalid_profile_and_missing_identity_fail_closed(self):
        with self.assertRaises(ValueError):
            self.store.register(str(self.home / "missing"))
        with self.assertRaises(ValueError):
            self.store.home("../../escape")
        (self.primary / "auth.json").write_text('{"tokens": SECRET')
        metadata = auth_metadata(self.primary)
        self.assertEqual(metadata["status"], "error")
        self.assertNotIn("SECRET", json.dumps(metadata))
        with self.assertRaises(ValueError):
            self.store.home("default")

    def test_old_directory_rules_are_only_migration_hints(self):
        self.store.data["accounts"]["default"]["projectRules"] = {
            "allowedProjects": [str(self.root / "old-project")], "revision": 3,
        }
        self.store._save()
        self.assertNotIn("projectRules", self.store.get("default"))
        self.assertNotIn("projectRules", self.store.refresh("default"))
        self.assertEqual(self.store.legacy_project_defaults(), {str((self.root / "old-project").resolve()): "default"})
        self.assertEqual(self.store.home("default"), self.primary.resolve())
        other = self.home / "other"
        auth(other, "account-two")
        key = self.store.register(str(other))
        self.store.data["accounts"][key]["projectRules"] = {
            "allowedProjects": [str(self.root / "old-project")], "revision": 1,
        }
        self.assertEqual(self.store.legacy_project_defaults(), {})

    def test_login_uses_new_home_and_reuses_request_receipt(self):
        (self.primary / "config.toml").write_text(
            'model = "gpt-5.6-sol"\nforced_chatgpt_account_id = "account-one"\n[features]\ntime_awareness = true\n'
        )
        (self.primary / "skills").mkdir()
        calls = []
        store = self.store

        class Runtime:
            def connect(self, key):
                self.key = key
                return self

            def call(self, method, params, timeout):
                calls.append((self.key, method, params))
                self.actual_home = store.home(self.key)
                return {
                    "type": "chatgptDeviceCode",
                    "loginId": "login-one",
                    "verificationUrl": "https://auth.openai.com/codex/device",
                    "userCode": "CODE",
                }

        runtime = Runtime()
        request = str(uuid.uuid4())
        result = store.start_login(runtime, request)
        self.assertEqual(result, store.start_login(runtime, request))
        self.assertEqual(len(calls), 1)
        home = runtime.actual_home
        self.assertNotEqual(home, self.primary)
        self.assertFalse((home / "auth.json").exists())
        self.assertNotIn(
            "forced_chatgpt_account_id", (home / "config.toml").read_text()
        )
        self.assertEqual(
            (home / "skills").resolve(), (self.primary / "skills").resolve()
        )
        auth(home, "new-account")
        store.login_completed(result["accountKey"], {"success": True})
        self.assertEqual(store.get(result["accountKey"])["status"], "ready")
        self.assertEqual(
            AccountStore(self.root / "state").get(result["accountKey"])["accountId"],
            "new-account",
        )

    def test_failed_login_stays_visible(self):
        class Runtime:
            def connect(self, key):
                raise RuntimeError("SECRET INTERNAL ERROR")

        result = self.store.start_login(Runtime(), str(uuid.uuid4()))
        self.assertEqual(self.store.get(result["accountKey"])["status"], "error")
        self.assertNotIn("SECRET", json.dumps(self.store.list()))

    def test_http_account_selection_limits_and_write_auth(self):
        spec = importlib.util.spec_from_file_location(
            "account_runtime_fixture",
            Path(__file__).with_name("runtime-accounts-contract.py"),
        )
        fixture = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(fixture)
        from codex_canvas import Canvas, make_server

        root = self.root / "http-state"
        canvas = Canvas(root)
        runtime = fixture.ControlledRuntime(root, fixture.AccountServer)
        canvas.runtime = runtime
        server = make_server(canvas)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        base = f"http://127.0.0.1:{server.server_port}"
        headers = {"Content-Type": "application/json"}

        def request(path, body=None):
            req = urllib.request.Request(
                base + path,
                data=json.dumps(body).encode() if body is not None else None,
                headers=headers,
            )
            try:
                with urllib.request.urlopen(req, timeout=5) as response:
                    return response.status, json.load(response)
            except urllib.error.HTTPError as error:
                with error:
                    return error.code, json.load(error)

        try:
            self.assertEqual(
                request("/api/accounts/default", {"account_key": "default"})[0], 403
            )
            headers.update(
                {"Origin": base, "X-Canvas-Token": request("/api/state")[1]["token"]}
            )
            other = self.home / "other"
            auth(other, "account-two")
            status, result = request("/api/accounts/register", {"home": str(other)})
            self.assertEqual(status, 200)
            key = next(
                a["id"] for a in result["accounts"] if a["accountId"] == "account-two"
            )
            self.assertEqual(
                request("/api/accounts/default", {"account_key": key})[1][
                    "defaultAccountKey"
                ],
                key,
            )
            lead = request("/api/leads", {"id": str(uuid.uuid4())})[1]
            self.assertEqual(lead["accountKey"], key)
            self.assertEqual(
                request(
                    "/api/agents/account", {"id": lead["id"], "account_key": "default"}
                )[1]["accountKey"],
                "default",
            )
            status, limits = request("/api/limits?account_key=" + key)
            self.assertEqual(status, 200)
            self.assertEqual(limits["data"]["accountId"], key)
            self.assertEqual(limits["accountKey"], key)
            self.assertEqual(request("/api/limits?account_key=missing")[0], 400)
            self.assertNotIn("SECRET", json.dumps(request("/api/accounts")[1]))
            self.assertTrue(all("projectRules" not in account for account in result["accounts"]))
            self.assertEqual(
                request("/api/agents/account", {"id": lead["id"], "account_key": key})[0],
                200,
            )
        finally:
            server.shutdown()
            server.server_close()
            thread.join()
            runtime.close()


if __name__ == "__main__":
    unittest.main()
