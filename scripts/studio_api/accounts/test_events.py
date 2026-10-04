"""Focused checks for accounts/provider resource event publication."""

from __future__ import annotations

import concurrent.futures
import sys
import types
import unittest
from pathlib import Path
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

    def test_absent_optional_hub_is_a_noop(self) -> None:
        with patch.dict(sys.modules, {"studio_api.sync.resources.hub": None}):
            publish_account_change("/isolated/state")


class CatalogPublicationTests(unittest.TestCase):
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


if __name__ == "__main__":
    unittest.main()
