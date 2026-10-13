#!/usr/bin/env python3
"""Backend-neutral account and cached limits contracts for the Rust slice."""

from __future__ import annotations

from test_isolation import isolate_supervisor_environment

import json
import os
from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'src'))

isolate_supervisor_environment()
from runtime_test_environment import make_runtime

SERVER_SOURCE = Path(__file__).resolve().parents[1] / "src"
sys.path.insert(0, str(Path(__file__).parent))

from codex_runtime import Runtime
from http_backend_harness import BackendProcess, HttpResult


def assert_status(response: HttpResult, expected: int) -> None:
    assert response.status == expected, (expected, response.status, response.body[:2000])


def fixture_registry(codex_home: Path, missing_home: Path, malformed_home: Path) -> dict[str, object]:
    return {
        "version": 1,
        "defaultAccountKey": "default",
        "accounts": {
            "default": {
                "id": "default", "home": str(codex_home), "label": "Codex",
                "source": "Codex CLI", "accountId": None, "email": None,
                "plan": None, "status": "signedOut",
            },
            "disconnected": {
                "id": "disconnected", "home": str(missing_home), "label": "Old",
                "source": "Local profile", "accountId": "stale-id", "plan": "pro",
                "status": "ready", "disconnected": True,
            },
            "duplicate": {
                "id": "duplicate", "home": str(missing_home), "label": "Duplicate",
                "source": "Local profile", "status": "duplicate", "duplicateOf": "default",
            },
            "deleted": {
                "id": "deleted", "home": str(missing_home), "label": "Deleted",
                "source": "Local profile", "status": "ready", "deleted": True,
            },
            "malformed": {
                "id": "malformed", "home": str(malformed_home), "label": "Broken",
                "source": "Local profile", "status": "ready",
            },
        },
        "logins": {},
        "deleteReceipts": {},
    }


class RustCatalogHttpContract(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        scratch = os.environ.get("TMPDIR")
        if not scratch:
            raise RuntimeError("The server runner must provide a memory-backed TMPDIR")
        cls.backend = BackendProcess(Path(scratch))
        cls.backend.create_codex_replay("restart-mid-turn-supervisor-replay.json")
        # The harness installs a replay executable and a disposable auth file.
        # Keep the account signed out so even startup cannot launch that provider.
        (cls.backend.codex_home / "auth.json").write_text("{}", encoding="utf-8")
        missing_home = cls.backend.root / "missing-profile"
        malformed_home = cls.backend.root / "malformed-profile"
        malformed_home.mkdir()
        (malformed_home / "auth.json").write_text('{"tokens":', encoding="utf-8")
        account_root = cls.backend.state / "accounts"
        account_root.mkdir(parents=True)
        (account_root / "registry.json").write_text(
            json.dumps(fixture_registry(cls.backend.codex_home, missing_home, malformed_home)),
            encoding="utf-8",
        )

        # Python creates the existing schema; Rust only opens it in place.
        runtime = make_runtime(Runtime, cls.backend)
        try:
            runtime.set_rate_limits(
                "default",
                {
                    "data": {
                        "accountId": "fixture-account",
                        "status": "ready",
                        "signedIn": True,
                        "rateLimits": {"primary": {"usedPercent": 27}},
                    },
                    "at": 1234.5,
                    "readAt": 1234.5,
                    "error": None,
                },
            )
            runtime.set_rate_limits(
                "cached-unknown",
                {
                    "data": {"accountId": "cached-only"},
                    "at": 2345.0,
                    "readAt": 2345.0,
                    "error": None,
                },
            )
        finally:
            runtime.close()

        cls.backend.start()

    @classmethod
    def tearDownClass(cls) -> None:
        cls.backend.close()

    def test_accounts_returns_file_registry_status_and_deleted_archive(self) -> None:
        result = self.backend.request("GET", "/api/accounts")
        assert_status(result, 200)
        body = result.json()
        self.assertEqual(body["defaultAccountKey"], "default")
        self.assertEqual(
            [account["id"] for account in body["accounts"]],
            ["default", "disconnected", "duplicate", "malformed"],
        )
        accounts = {account["id"]: account for account in body["accounts"]}
        self.assertEqual(accounts["default"]["status"], "signedOut")
        self.assertEqual(accounts["disconnected"]["status"], "ready")
        self.assertTrue(accounts["disconnected"]["disconnected"])
        self.assertEqual(accounts["disconnected"]["accountId"], "stale-id")
        self.assertEqual(accounts["duplicate"]["duplicateOf"], "default")
        self.assertEqual(accounts["malformed"]["status"], "ready")
        self.assertEqual([row["id"] for row in body["archivedAccounts"]], ["deleted"])
        self.assertTrue(body["supportsDisconnect"])
        self.assertTrue(body["supportsDelete"])
        self.assertNotIn("fixture-only-replay-key", json.dumps(body))

        real_home = Path.home().resolve()
        for account in body["accounts"] + body["archivedAccounts"]:
            resolved = Path(account["home"]).resolve()
            self.assertNotEqual(resolved, real_home)
            self.assertNotIn(real_home, resolved.parents)
            self.assertTrue(resolved == self.backend.root or self.backend.root in resolved.parents)
        methods = {frame.get("method") for frame in self.backend.received_provider_frames()}
        self.assertNotIn("model/list", methods)
        self.assertNotIn("account/rateLimits/read", methods)

    def test_accounts_z_keeps_the_registry_loaded_at_startup(self) -> None:
        registry_path = self.backend.state / "accounts" / "registry.json"
        original = registry_path.read_bytes()
        try:
            registry_path.write_text("{", encoding="utf-8")
            result = self.backend.request("GET", "/api/accounts")
            assert_status(result, 200)
            self.assertEqual(
                [account["id"] for account in result.json()["accounts"]],
                ["default", "disconnected", "duplicate", "malformed"],
            )
        finally:
            registry_path.write_bytes(original)

    def test_malformed_registry_prevents_backend_start(self) -> None:
        scratch = os.environ.get("TMPDIR")
        if not scratch:
            self.fail("The server runner must provide a memory-backed TMPDIR")
        backend = BackendProcess(Path(scratch))
        try:
            backend.create_codex_replay("restart-mid-turn-supervisor-replay.json")
            (backend.codex_home / "auth.json").write_text("{}", encoding="utf-8")
            registry_path = backend.state / "accounts" / "registry.json"
            registry_path.parent.mkdir(parents=True)
            registry_path.write_text(
                json.dumps(fixture_registry(
                    backend.codex_home,
                    backend.root / "missing-profile",
                    backend.root / "malformed-profile",
                )),
                encoding="utf-8",
            )
            (backend.root / "malformed-profile").mkdir()
            (backend.root / "malformed-profile" / "auth.json").write_text(
                "{}", encoding="utf-8",
            )
            runtime = make_runtime(Runtime, backend)
            runtime.close()

            registry_path.write_text("{", encoding="utf-8")

            with self.assertRaisesRegex(RuntimeError, "Backend did not emit its ready line") as raised:
                backend.start()
            failure = str(raised.exception)
            self.assertIn("child exit status=1", failure)
            self.assertIn("Cannot read the account registry", failure)
        finally:
            backend.close()

    def test_missing_registry_default_home_stays_in_disposable_home(self) -> None:
        scratch = os.environ.get("TMPDIR")
        if not scratch:
            self.fail("The server runner must provide a memory-backed TMPDIR")
        backend = BackendProcess(Path(scratch))
        try:
            backend.create_codex_replay("restart-mid-turn-supervisor-replay.json")
            (backend.codex_home / "auth.json").write_text("{}", encoding="utf-8")
            runtime = make_runtime(Runtime, backend)
            runtime.close()
            backend.start()
            result = backend.request("GET", "/api/accounts")

            assert_status(result, 200)
            account = result.json()["accounts"][0]
            self.assertEqual(account["id"], "default")
            resolved_home = Path(account["home"]).resolve()
            real_home = Path.home().resolve()
            self.assertEqual(resolved_home, backend.codex_home.resolve())
            self.assertNotEqual(resolved_home, real_home)
            self.assertNotIn(real_home, resolved_home.parents)
            self.assertIn(backend.root.resolve(), resolved_home.parents)
        finally:
            backend.close()

    def test_limits_a_plain_default_snapshot_is_available(self) -> None:
        result = self.backend.request("GET", "/api/limits?account_key=default&cached=1")
        assert_status(result, 200)
        self.assertEqual(result.json()["accountKey"], "default")
        self.assertEqual(result.json()["data"]["accountId"], "fixture-account")

    def test_limits_b_cached_query_returns_the_existing_database_snapshot(self) -> None:
        result = self.backend.request(
            "GET", "/api/limits?account_key=default&cached=1",
        )
        assert_status(result, 200)
        self.assertEqual(result.json(), {
            "accountKey": "default",
            "data": {
                "accountId": "fixture-account",
                "status": "ready",
                "signedIn": True,
                "rateLimits": {"primary": {"usedPercent": 27}},
            },
            "at": 1234.5,
            "readAt": 1234.5,
            "error": None,
        })
        methods = {frame.get("method") for frame in self.backend.received_provider_frames()}
        self.assertNotIn("account/rateLimits/read", methods)

    def test_limits_scalar_blank_and_missing_account_keys(self) -> None:
        for query in (
            "account_key=default&cached=1",
            "account_key=&cached=1",
            "cached=1",
        ):
            with self.subTest(query=query):
                result = self.backend.request("GET", "/api/limits" + (f"?{query}" if query else ""))
                assert_status(result, 200)
                self.assertEqual(result.json()["accountKey"], "default")

    def test_limits_unknown_account_returns_python_validation_error(self) -> None:
        result = self.backend.request("GET", "/api/limits?account_key=not-registered")
        assert_status(result, 400)
        self.assertEqual(result.json(), {"error": "Unknown Codex account"})

    def test_limits_cached_unknown_account_returns_the_snapshot(self) -> None:
        result = self.backend.request(
            "GET", "/api/limits?account_key=cached-unknown&cached=1",
        )
        assert_status(result, 200)
        self.assertEqual(result.json()["data"]["accountId"], "cached-only")

    def test_limits_repeated_scalar_query_returns_located_400(self) -> None:
        result = self.backend.request(
            "GET", "/api/limits?account_key=&account_key=default&cached=1",
        )
        assert_status(result, 400)
        self.assertEqual(result.json(), {
            "error": "Invalid request",
            "location": ["query", "account_key"],
        })

    def test_limits_rejects_extra_query_parameters(self) -> None:
        result = self.backend.request("GET", "/api/limits?extra=1&cached=1")
        assert_status(result, 400)
        self.assertEqual(result.json(), {
            "error": "Invalid request",
            "location": ["query", "extra"],
        })

    def test_uncached_limits_declares_the_live_provider_dependency(self) -> None:
        if not os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"):
            self.skipTest(
                "Python's uncached limits route starts a provider read; the Rust partial contract is 501"
            )
        result = self.backend.request("GET", "/api/limits?account_key=disconnected")
        assert_status(result, 501)
        self.assertEqual(
            result.json(),
            {"error": "Not implemented: requires provider-session/account-rate-limits"},
        )
        methods = {frame.get("method") for frame in self.backend.received_provider_frames()}
        self.assertNotIn("account/rateLimits/read", methods)

    def test_models_unknown_account_validation_remains_a_python_only_contract(self) -> None:
        if os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"):
            self.skipTest(
                "Known gap: Rust GET /api/models remains 501 until provider-session/model-list lands"
            )
        result = self.backend.request("GET", "/api/models?account_key=not-registered")
        assert_status(result, 400)
        self.assertEqual(result.json(), {"error": "Unknown Codex account"})
        methods = {frame.get("method") for frame in self.backend.received_provider_frames()}
        self.assertNotIn("model/list", methods)


if __name__ == "__main__":
    unittest.main()
