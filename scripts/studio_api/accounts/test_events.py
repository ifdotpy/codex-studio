"""Focused checks for accounts/provider resource event publication."""

from __future__ import annotations

import asyncio
import concurrent.futures
import tempfile
import threading
import unittest
from pathlib import Path
from types import SimpleNamespace
from typing import Any, Protocol, cast
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
    ResourceRef,
)
from studio_api.sync.resources.hub import (
    ResourceHub,
    register_resource_hub,
    unregister_resource_hub,
)


class _TimedLock(Protocol):
    def acquire(self, blocking: bool = True, timeout: float = -1) -> bool: ...

    def release(self) -> None: ...


def _available_to_other_thread(lock: _TimedLock) -> bool:
    acquired_by_other: list[bool] = []
    finished = threading.Event()

    def probe() -> None:
        acquired = False
        try:
            acquired = lock.acquire(timeout=0.1)
            acquired_by_other.append(acquired)
        finally:
            try:
                if acquired:
                    lock.release()
            finally:
                finished.set()

    probe_thread = threading.Thread(target=probe, daemon=True)
    probe_thread.start()
    if not finished.wait(timeout=0.5):
        probe_thread.join(timeout=0.1)
        return False
    probe_thread.join(timeout=0.1)
    return not probe_thread.is_alive() and acquired_by_other == [True]


class ResourcePublicationTests(unittest.IsolatedAsyncioTestCase):
    async def test_helpers_publish_typed_resources_to_registered_subscriber(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            state_dir = Path(temporary)
            hub = ResourceHub("accounts-workspace")
            register_resource_hub(state_dir, hub)
            subscription = hub.subscribe(
                [
                    ResourceRef(AccountsResource(kind="accounts")),
                    ResourceRef(LimitsResource(kind="limits", accountKey="account-a")),
                    ResourceRef(ModelsResource(kind="models")),
                ],
                loop=asyncio.get_running_loop(),
            )
            try:
                expected = [
                    AccountsResource(kind="accounts"),
                    LimitsResource(kind="limits", accountKey="account-a"),
                    ModelsResource(kind="models"),
                ]
                for publish in (
                    lambda: publish_account_change(state_dir),
                    lambda: publish_limits_change(state_dir, "account-a"),
                    lambda: publish_models_change(state_dir),
                ):
                    publish()
                    event = await subscription.next_event(timeout=1)
                    self.assertIsNotNone(event)
                    assert event is not None
                    self.assertEqual(event.reason, "change")
                    self.assertEqual(
                        [resource.root for resource in event.resources], [expected.pop(0)]
                    )
            finally:
                subscription.close()
                unregister_resource_hub(state_dir, hub)

    async def test_producers_are_noop_when_no_hub_is_registered(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            state_dir = Path(temporary)
            publish_account_change(state_dir)
            publish_limits_change(state_dir, "account-a")
            publish_models_change(state_dir)

    async def test_api_context_startup_registers_hub_before_producers(self) -> None:
        from studio_api.context import ApiContext

        class SyncIdentity:
            def identity(self) -> dict[str, str]:
                return {"workspaceId": "startup-workspace"}

        with tempfile.TemporaryDirectory() as temporary:
            state_dir = Path(temporary)
            canvas = SimpleNamespace(root=state_dir, runtime=None)
            context = ApiContext(cast(Any, canvas), remote=cast(Any, object()))
            setattr(context, "_sync_store", SyncIdentity())
            context.initialize()
            hub = context.resource_hub()
            subscription = hub.subscribe(
                [ResourceRef(AccountsResource(kind="accounts"))], loop=asyncio.get_running_loop()
            )
            try:
                publish_account_change(state_dir)
                event = await subscription.next_event(timeout=1)
                self.assertIsNotNone(event)
                assert event is not None
                self.assertEqual(event.reason, "change")
                self.assertEqual(event.resources, [ResourceRef(AccountsResource(kind="accounts"))])
            finally:
                subscription.close()
                context.close()

    async def test_runtime_usage_refresh_publishes_only_changes_after_cache_lock(self) -> None:
        from codex_runtime import Runtime
        from studio_api.sync.resources.hub import publish_resources as publish_to_hub

        with tempfile.TemporaryDirectory() as temporary:
            state_dir = Path(temporary)
            hub = ResourceHub("limits-workspace")
            register_resource_hub(state_dir, hub)
            subscription = hub.subscribe(
                [ResourceRef(LimitsResource(kind="limits", accountKey="account-a"))],
                loop=asyncio.get_running_loop(),
            )
            cache_lock = threading.RLock()
            changed_values = iter((True, False))
            publications_under_lock: list[bool] = []

            def store_rate_limits(_account_key: str, _value: dict[str, object]) -> bool:
                return next(changed_values)

            runtime = SimpleNamespace(
                root=state_dir,
                connection_current=lambda _account_key, _connection_id: True,
                rate_limits_for=lambda _account_key: {"data": {}, "at": 0},
                store_rate_limits=store_rate_limits,
                usage_resume_limits_changed=lambda _account_key, _value: None,
                _rate_cache_lock=cache_lock,
            )
            bucket = {"limitId": "codex", "primary": {"usedPercent": 27}}
            message = {
                "method": "account/rateLimits/updated",
                "params": {"rateLimits": bucket, "rateLimitsByLimitId": {"codex": bucket}},
            }

            def publish_checked(root: str | Path, *resources: ResourceRef) -> None:
                publications_under_lock.append(_available_to_other_thread(cast(_TimedLock, cache_lock)))
                publish_to_hub(root, *resources)

            try:
                with patch(
                    "studio_api.sync.resources.hub.publish_resources", side_effect=publish_checked
                ):
                    Runtime.notification(runtime, message, "account-a", "connection-a")
                    event = await subscription.next_event(timeout=1)
                    self.assertIsNotNone(event)
                    assert event is not None
                    self.assertEqual(
                        event.resources,
                        [ResourceRef(LimitsResource(kind="limits", accountKey="account-a"))],
                    )
                    Runtime.notification(runtime, message, "account-a", "connection-a")

                self.assertIsNone(await subscription.next_event(timeout=0.01))
                self.assertEqual(publications_under_lock, [True])
            finally:
                subscription.close()
                unregister_resource_hub(state_dir, hub)


class CatalogPublicationTests(unittest.IsolatedAsyncioTestCase):
    async def test_successful_async_catalog_commit_reaches_subscriber(self) -> None:
        from codex_catalog import CatalogPending, ModelCatalogCache

        native: concurrent.futures.Future[dict[str, object]] = concurrent.futures.Future()
        submitted: list[concurrent.futures.Future[dict[str, object]]] = []

        class Server:
            def submit(
                self, _method: str, _params: dict[str, object]
            ) -> concurrent.futures.Future[dict[str, object]]:
                submitted.append(native)
                return native

        with tempfile.TemporaryDirectory() as temporary:
            state_dir = Path(temporary)
            hub = ResourceHub("catalog-workspace")
            register_resource_hub(state_dir, hub)
            subscription = hub.subscribe(
                [ResourceRef(ModelsResource(kind="models"))], loop=asyncio.get_running_loop()
            )
            cache = ModelCatalogCache(wait_seconds=0)
            published: list[bool] = []

            def on_commit() -> None:
                acquired = cache.lock.acquire(blocking=False)
                published.append(acquired)
                if acquired:
                    cache.lock.release()
                publish_models_change(state_dir)

            try:
                with self.assertRaises(CatalogPending):
                    cache.read(
                        "account-a", Server(), "connection-a", lambda: True,
                        on_commit=on_commit,
                    )
                catalog: dict[str, object] = {"data": [{"model": "fixture/model"}]}
                native.set_result(catalog)

                self.assertEqual(len(submitted), 1)
                self.assertEqual(published, [True])
                event = await subscription.next_event(timeout=1)
                self.assertIsNotNone(event)
                assert event is not None
                self.assertEqual(event.reason, "change")
                self.assertEqual(event.resources, [ResourceRef(ModelsResource(kind="models"))])
                self.assertEqual(
                    cache.read("account-a", Server(), "connection-a", lambda: True), catalog
                )
            finally:
                subscription.close()
                unregister_resource_hub(state_dir, hub)

    async def test_terminal_failure_and_user_retry_reach_registered_subscriber(self) -> None:
        from codex_catalog import CatalogPending, CatalogUnavailable, ModelCatalogCache

        first: concurrent.futures.Future[dict[str, object]] = concurrent.futures.Future()
        retried: concurrent.futures.Future[dict[str, object]] = concurrent.futures.Future()
        submitted = 0

        class Server:
            def submit(
                self, _method: str, _params: dict[str, object]
            ) -> concurrent.futures.Future[dict[str, object]]:
                nonlocal submitted
                submitted += 1
                return first if submitted == 1 else retried

        with tempfile.TemporaryDirectory() as temporary:
            state_dir = Path(temporary)
            hub = ResourceHub("catalog-retry-workspace")
            register_resource_hub(state_dir, hub)
            subscription = hub.subscribe(
                [ResourceRef(ModelsResource(kind="models"))], loop=asyncio.get_running_loop()
            )
            cache = ModelCatalogCache(wait_seconds=0)
            publish = lambda: publish_models_change(state_dir)
            server = Server()
            try:
                with self.assertRaises(CatalogPending):
                    cache.read("account-a", server, "connection-a", lambda: True, on_commit=publish)
                first.set_exception(RuntimeError("provider unavailable"))
                failed = await subscription.next_event(timeout=1)
                self.assertIsNotNone(failed)
                assert failed is not None
                self.assertEqual(failed.resources, [ResourceRef(ModelsResource(kind="models"))])

                for _ in range(2):
                    with self.assertRaisesRegex(CatalogUnavailable, "provider unavailable"):
                        cache.read(
                            "account-a", server, "connection-a", lambda: True,
                            on_commit=publish,
                        )
                self.assertEqual(submitted, 1)
                self.assertIsNone(await subscription.next_event(timeout=0.01))

                expected: dict[str, object] = {"data": [{"model": "fixture/recovered"}]}
                retried.set_result(expected)
                self.assertEqual(
                    cache.read("account-a", server, "connection-a", lambda: True,
                               retry=True, on_commit=publish),
                    expected,
                )
                recovered = await subscription.next_event(timeout=1)
                self.assertIsNotNone(recovered)
                assert recovered is not None
                self.assertEqual(recovered.resources, [ResourceRef(ModelsResource(kind="models"))])
                self.assertEqual(submitted, 2)
            finally:
                subscription.close()
                unregister_resource_hub(state_dir, hub)

    def test_failed_async_catalog_is_cached_until_explicit_retry(self) -> None:
        from codex_catalog import CatalogPending, CatalogUnavailable, ModelCatalogCache

        native: concurrent.futures.Future[dict[str, object]] = concurrent.futures.Future()
        retry_native: concurrent.futures.Future[dict[str, object]] = concurrent.futures.Future()
        submitted = 0

        class Server:
            def submit(
                self, _method: str, _params: dict[str, object]
            ) -> concurrent.futures.Future[dict[str, object]]:
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
            def submit(
                self, _method: str, _params: dict[str, object]
            ) -> concurrent.futures.Future[dict[str, object]]:
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


class ClaudeLoginEventTests(unittest.IsolatedAsyncioTestCase):
    async def test_new_verification_url_reaches_subscriber_while_login_is_active(self) -> None:
        from codex_claude_login import LoginManager

        manager = LoginManager.__new__(LoginManager)
        manager.lock = threading.RLock()
        process = SimpleNamespace(poll=lambda: None)
        job = {
            "receipt": {"requestId": "request-a", "accountKey": "claude-a", "status": "starting"},
            "process": process,
        }
        observed: list[tuple[bool, Path | None]] = []
        url = "https://claude.com/cai/oauth/authorize?code=fixture"
        from studio_api.accounts.events import (
            publish_account_change as publish_account_change_to_hub,
        )

        with tempfile.TemporaryDirectory() as temporary:
            state_dir = Path(temporary)
            manager.runtime = SimpleNamespace(root=state_dir)
            hub = ResourceHub("claude-login-workspace")
            register_resource_hub(state_dir, hub)
            subscription = hub.subscribe(
                [ResourceRef(AccountsResource(kind="accounts"))], loop=asyncio.get_running_loop()
            )

            def published(published_state_dir: Path) -> None:
                lock_available = _available_to_other_thread(cast(_TimedLock, manager.lock))
                observed.append(
                    (lock_available, published_state_dir if process.poll() is None else None)
                )
                publish_account_change_to_hub(published_state_dir)

            try:
                with patch(
                    "codex_claude_login.publish_account_change", side_effect=published
                ) as publish:
                    self.assertTrue(manager._publish_verification_url(job, url))
                    event = await subscription.next_event(timeout=1)
                    self.assertIsNotNone(event)
                    self.assertFalse(manager._publish_verification_url(job, url))

                publish.assert_called_once()
                self.assertEqual(observed, [(True, state_dir)])
                assert event is not None
                self.assertEqual(event.resources, [ResourceRef(AccountsResource(kind="accounts"))])
                receipt = cast(dict[str, object], job["receipt"])
                self.assertEqual(receipt["status"], "pending")
                self.assertEqual(receipt["verificationUrl"], url)
            finally:
                subscription.close()
                unregister_resource_hub(state_dir, hub)


if __name__ == "__main__":
    unittest.main()
