"""Focused tests for the sync state signature callback."""
from __future__ import annotations

import json
import sys
import tempfile
import threading
import unittest
from pathlib import Path
from types import ModuleType, SimpleNamespace
from typing import Any, Protocol, cast
from unittest.mock import patch

from studio_api.context import ApiContext, RemoteAccessContract


class CallableSignature(Protocol):
    def __call__(self) -> tuple[object, ...] | None: ...


class CapturingSyncStore:
    def __init__(self, *_args: object, state_signature: object, **_kwargs: object) -> None:
        self.state_signature = cast(CallableSignature, state_signature)


class SyncStateSignatureTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory(prefix="sync-state-signature-")
        self.addCleanup(self.temporary.cleanup)
        self.warnings: list[dict[str, object]] = [
            {
                "id": "provider-version:account-a",
                "accountKey": "account-a",
                "provider": "codex",
                "version": "0.1.0",
                "baseline": "0.2.0",
                "message": "Update is recommended.",
                "at": 1800000000.0,
            }
        ]
        self.runtime = SimpleNamespace(
            start_lock=threading.Lock(),
            lock=threading.RLock(),
            servers={"account-a": object()},
            offline_accounts=set(),
            closed=False,
            rate_limits={
                "accountKey": "default",
                "data": {
                    "signedIn": True,
                    "rateLimitsByLimitId": {
                        "codex": {
                            "primary": {"usedPercent": 99.5, "resetsAt": 1800000000},
                            "secondary": None,
                        }
                    },
                },
                "at": 1800000000.0,
                "error": None,
            },
            rate_limits_by_account={
                "account-a": {
                    "accountKey": "account-a",
                    "data": {
                        "signedIn": True,
                        "rateLimits": {"primary": {"usedPercent": 42, "resetsAt": None}},
                    },
                    "at": None,
                    "error": "temporarily unavailable",
                }
            },
            connection_ids={"account-a": "connection-a"},
            provider_version_monitor=SimpleNamespace(
                status=lambda: {"warnings": self.warnings}
            ),
        )
        self.runtime.rate_limits_for = lambda key: (
            self.runtime.rate_limits
            if key == "default"
            else self.runtime.rate_limits_by_account.get(
                key, {"accountKey": key, "data": None, "at": None, "error": None}
            )
        )

        sync_module = ModuleType("codex_sync")
        sync_module.SyncStore = CapturingSyncStore  # type: ignore[attr-defined]
        state_module = ModuleType("codex_state")
        state_module.process_is_alive = lambda _pid: True  # type: ignore[attr-defined]
        state_module.read_threads = lambda _root: []  # type: ignore[attr-defined]
        modules = patch.dict(
            sys.modules,
            {"codex_sync": sync_module, "codex_state": state_module},
        )
        modules.start()
        self.addCleanup(modules.stop)

        canvas = SimpleNamespace(
            root=Path(self.temporary.name),
            runtime=self.runtime,
            connect=lambda: None,
            snapshot=lambda **_kwargs: {},
            transcript=lambda *_args: None,
        )
        context = ApiContext(
            cast(Any, canvas),
            remote=cast(RemoteAccessContract, object()),
        )
        self.signature = cast(CallableSignature, context.sync().state_signature)

    def old_serialized_signature(self) -> str:
        """Recreate the previous normalization before its final JSON dump."""
        with self.runtime.lock:
            rate_limits = json.loads(json.dumps(self.runtime.rate_limits, sort_keys=True))
            by_account = {
                key: json.loads(
                    json.dumps(self.runtime.rate_limits_for(key), sort_keys=True)
                )
                for key in self.runtime.rate_limits_by_account
            }
            connection_ids = dict(self.runtime.connection_ids)
            warnings = self.runtime.provider_version_monitor.status()["warnings"]
            return json.dumps(
                {
                    "rateLimits": rate_limits,
                    "rateLimitsByAccount": by_account,
                    "connectionIds": connection_ids,
                    "providerWarnings": warnings,
                },
                sort_keys=True,
                separators=(",", ":"),
            )

    def test_json_producer_values_keep_the_exact_serialized_signature(self) -> None:
        signature = self.signature()
        self.assertIsNotNone(signature)
        assert signature is not None
        self.assertEqual(signature[1], self.old_serialized_signature())

    def test_producer_state_changes_change_the_signature(self) -> None:
        original = self.signature()
        assert original is not None

        self.runtime.rate_limits["error"] = "refresh failed"
        changed_limits = self.signature()
        self.assertNotEqual(original, changed_limits)
        assert changed_limits is not None

        self.runtime.connection_ids["account-a"] = "connection-b"
        changed_connection = self.signature()
        self.assertNotEqual(changed_limits, changed_connection)
        assert changed_connection is not None

        self.warnings[0]["message"] = "A newer version is recommended."
        self.assertNotEqual(changed_connection, self.signature())

    def test_busy_runtime_locks_return_without_waiting(self) -> None:
        self.assertTrue(self.runtime.start_lock.acquire(blocking=False))
        try:
            self.assertIsNone(self.signature())
        finally:
            self.runtime.start_lock.release()

        locked = threading.Event()
        release = threading.Event()

        def hold_runtime_lock() -> None:
            with self.runtime.lock:
                locked.set()
                release.wait(timeout=2)

        holder = threading.Thread(target=hold_runtime_lock)
        holder.start()
        self.assertTrue(locked.wait(timeout=2))
        try:
            self.assertIsNone(self.signature())
        finally:
            release.set()
            holder.join(timeout=2)
        self.assertFalse(holder.is_alive())


if __name__ == "__main__":
    unittest.main()
