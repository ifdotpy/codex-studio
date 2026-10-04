"""Focused checks for accounts/provider resource event publication."""

from __future__ import annotations

import concurrent.futures
import sys
import threading
import types
import unittest
from pathlib import Path
from types import SimpleNamespace
from typing import cast
from unittest.mock import patch

from studio_api.accounts.events import (
    publish_account_change,
    publish_limits_change,
    publish_models_change,
)
from studio_api.sync.resources.models import (
    AccountsResource,
    LimitsResource,
    ModelsResource,
)


class ResourcePublicationTests(unittest.TestCase):
    def test_helpers_publish_typed_resource_refs_to_hub(self) -> None:
        calls: list[tuple[str | Path, tuple[object, ...]]] = []
        hub = types.ModuleType("studio_api.sync.resources.hub")
        hub.publish_resources = lambda state_dir, *resources: calls.append((state_dir, resources))  # type: ignore[attr-defined]
        state_dir = Path("/isolated/state")
        with patch.dict(sys.modules, {hub.__name__: hub}):
            publish_account_change(state_dir)
            publish_limits_change(state_dir, "account-a")
            publish_models_change(state_dir)

        self.assertEqual([call[0] for call in calls], [state_dir] * 3)
        self.assertEqual(
            [getattr(call[1][0], "root") for call in calls],
            [
                AccountsResource(kind="accounts"),
                LimitsResource(kind="limits", accountKey="account-a"),
                ModelsResource(kind="models"),
            ],
        )

class CatalogPublicationTests(unittest.TestCase):
    def setUp(self) -> None:
        hub = types.ModuleType("studio_api.sync.resources.hub")
        hub.publish_resources = lambda *_args: None  # type: ignore[attr-defined]
        self.hub_patch = patch.dict(sys.modules, {hub.__name__: hub})
        self.hub_patch.start()

    def tearDown(self) -> None:
        self.hub_patch.stop()

    def test_successful_async_catalog_commit_publishes_outside_cache_lock(self) -> None:
        from codex_catalog import CatalogPending, ModelCatalogCache

        native: concurrent.futures.Future[dict[str, object]] = concurrent.futures.Future()
        submitted: list[concurrent.futures.Future[dict[str, object]]] = []

        class Server:
            def submit(self, _method: str, _params: dict[str, object]) -> concurrent.futures.Future[dict[str, object]]:
                submitted.append(native)
                return native

        cache = ModelCatalogCache(wait_seconds=0)
        published: list[bool] = []

        def on_commit() -> None:
            acquired = cache.lock.acquire(blocking=False)
            published.append(acquired)
            if acquired:
                cache.lock.release()

        with self.assertRaises(CatalogPending):
            cache.read("account-a", Server(), "connection-a", lambda: True, on_commit=on_commit)
        catalog: dict[str, object] = {"data": [{"model": "fixture/model"}]}
        native.set_result(catalog)

        self.assertEqual(len(submitted), 1)
        self.assertEqual(published, [True])
        self.assertEqual(cache.read("account-a", Server(), "connection-a", lambda: True),
                         {"data": [{"model": "fixture/model"}]})

    def test_failed_async_catalog_is_cached_until_explicit_retry(self) -> None:
        from codex_catalog import CatalogPending, CatalogUnavailable, ModelCatalogCache

        native: concurrent.futures.Future[dict[str, object]] = concurrent.futures.Future()
        retry_native: concurrent.futures.Future[dict[str, object]] = concurrent.futures.Future()
        submitted = 0

        class Server:
            def submit(self, _method: str, _params: dict[str, object]) -> concurrent.futures.Future[dict[str, object]]:
                nonlocal submitted
                submitted += 1
                return native if submitted == 1 else retry_native

        cache = ModelCatalogCache(wait_seconds=0)
        publication_locks: list[bool] = []

        def on_commit() -> None:
            acquired = cache.lock.acquire(blocking=False)
            publication_locks.append(acquired)
            if acquired:
                cache.lock.release()

        server = Server()
        with self.assertRaises(CatalogPending):
            cache.read("account-a", server, "connection-a", lambda: True, on_commit=on_commit)
        with self.assertRaises(CatalogPending):
            cache.read("account-a", server, "connection-a", lambda: True,
                       retry=True, on_commit=on_commit)
        self.assertEqual(submitted, 1)
        native.set_exception(RuntimeError("catalog offline"))

        for _ in range(2):
            with self.assertRaisesRegex(CatalogUnavailable, "catalog offline"):
                cache.read("account-a", server, "connection-a", lambda: True, on_commit=on_commit)
        self.assertEqual(submitted, 1)
        self.assertEqual(publication_locks, [True])

        expected: dict[str, object] = {"data": [{"model": "fixture/recovered"}]}
        retry_native.set_result(expected)
        self.assertEqual(
            cache.read("account-a", server, "connection-a", lambda: True,
                       retry=True, on_commit=on_commit),
            expected,
        )
        self.assertEqual(submitted, 2)
        self.assertEqual(publication_locks, [True, True])
        self.assertEqual(
            cache.read("account-a", server, "connection-a", lambda: True, retry=True),
            expected,
        )
        self.assertEqual(submitted, 2)

    def test_publication_failure_does_not_fail_committed_catalog_read(self) -> None:
        from codex_catalog import ModelCatalogCache

        native: concurrent.futures.Future[dict[str, object]] = concurrent.futures.Future()
        expected: dict[str, object] = {"data": [{"model": "fixture/model"}]}
        native.set_result(expected)

        class Server:
            def submit(self, _method: str, _params: dict[str, object]) -> concurrent.futures.Future[dict[str, object]]:
                return native

        cache = ModelCatalogCache()
        with self.assertLogs("codex_catalog", level="ERROR"):
            result = cache.read(
                "account-a",
                Server(),
                "connection-a",
                lambda: True,
                on_commit=lambda: (_ for _ in ()).throw(RuntimeError("hub offline")),
            )
        self.assertEqual(result, expected)

    def test_cached_catalog_read_does_not_republish(self) -> None:
        from codex_catalog import ModelCatalogCache

        cache = ModelCatalogCache()
        expected: dict[str, object] = {"data": [{"model": "fixture/model"}]}
        native: concurrent.futures.Future[dict[str, object]] = concurrent.futures.Future()
        native.set_result(expected)

        class Server:
            def submit(self, _method: str, _params: dict[str, object]) -> concurrent.futures.Future[dict[str, object]]:
                return native

        calls: list[None] = []
        server = Server()
        self.assertEqual(cache.read("account-a", server, "connection-a", lambda: True,
                                    on_commit=lambda: calls.append(None)), expected)
        self.assertEqual(calls, [None])
        self.assertEqual(cache.read("account-a", server, "connection-a", lambda: True,
                                    on_commit=lambda: calls.append(None)), expected)
        self.assertEqual(calls, [None])


class ClaudeLoginEventTests(unittest.TestCase):
    def test_new_verification_url_notifies_while_login_is_active(self) -> None:
        from codex_claude_login import LoginManager

        manager = LoginManager.__new__(LoginManager)
        manager.runtime = SimpleNamespace(root=Path("/isolated/state"))
        manager.lock = threading.RLock()
        process = SimpleNamespace(poll=lambda: None)
        job = {
            "receipt": {"requestId": "request-a", "accountKey": "claude-a", "status": "starting"},
            "process": process,
        }
        observed: list[tuple[bool, Path | None]] = []

        def published(state_dir: Path) -> None:
            lock_available = manager.lock.acquire(blocking=False)
            if lock_available:
                manager.lock.release()
            observed.append((lock_available, state_dir if process.poll() is None else None))

        url = "https://claude.com/cai/oauth/authorize?code=fixture"
        with patch("codex_claude_login.publish_account_change", side_effect=published) as publish:
            self.assertTrue(manager._publish_verification_url(job, url))
            self.assertFalse(manager._publish_verification_url(job, url))

        publish.assert_called_once()
        self.assertEqual(observed, [(True, Path("/isolated/state"))])
        receipt = cast(dict[str, object], job["receipt"])
        self.assertEqual(receipt["status"], "pending")
        self.assertEqual(receipt["verificationUrl"], url)


if __name__ == "__main__":
    unittest.main()
