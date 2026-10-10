"""Account limits survive restart and cached routes do not wait for providers."""
import json
import sqlite3
import threading
import unittest
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from tempfile import TemporaryDirectory
from types import SimpleNamespace
from typing import Callable, cast
from unittest.mock import Mock

from fastapi import FastAPI
from fastapi.testclient import TestClient

from codex_runtime import Runtime
from studio_api.accounts.router import create_router
from studio_api.context import ApiContext


class LimitsCacheTests(unittest.TestCase):
    def test_restores_existing_workspace_snapshot_and_timestamp(self) -> None:
        snapshot = {"accountKey": "a", "at": 12.0, "readAt": 11.0,
                    "data": {"accountId": "a", "rateLimits": {"primary": {"usedPercent": 42}}},
                    "error": None}
        with sqlite3.connect(":memory:") as db:
            db.execute("CREATE TABLE sync_entities(collection TEXT,id TEXT,payload TEXT,deleted INTEGER)")
            db.execute("INSERT INTO sync_entities VALUES('workspace','current',?,0)",
                       (json.dumps({"value": {"rateLimitsByAccount": {"a": snapshot}}}),))
            runtime = SimpleNamespace(rate_limits_by_account={}, rate_limits={})
            cast(Callable[..., None], Runtime.restore_rate_limits)(runtime, db)
        self.assertEqual(runtime.rate_limits_by_account["a"], snapshot)
        self.assertNotIn("b", runtime.rate_limits_by_account)

    def test_runtime_restart_keeps_the_last_snapshot_even_when_only_time_changes(self) -> None:
        with TemporaryDirectory(prefix="studio-limits-cache-") as directory:
            snapshot = {"accountKey": "default", "at": 12.0, "readAt": 12.0,
                        "data": {"rateLimits": {"primary": {"usedPercent": 42}}}, "error": None}
            runtime = Runtime(Path(directory))
            try:
                runtime.set_rate_limits("default", snapshot)
                runtime.set_rate_limits("default", {**snapshot, "at": 20.0, "readAt": 20.0})
            finally:
                runtime.close()
            restarted = Runtime(Path(directory))
            try:
                restored = restarted.rate_limits_for("default")
                self.assertEqual(restored["at"], 20.0)
                self.assertEqual(restored["readAt"], 20.0)
                self.assertEqual(restored["data"], snapshot["data"])
            finally:
                restarted.close()

    def test_serves_snapshot_while_one_background_provider_read_is_pending(self) -> None:
        snapshot = {"accountKey": "a", "at": 12.0, "data": {"accountId": "a"}, "error": None}
        entered = threading.Event()
        release = threading.Event()
        def slow_read(key: str) -> None:
            entered.set()
            self.assertTrue(release.wait(5))
        with ThreadPoolExecutor(max_workers=1) as pool:
            runtime = SimpleNamespace(lock=threading.Lock(), closed=False, pool=pool,
                                      limits=Mock(side_effect=slow_read),
                                      rate_limits_for=lambda key: snapshot)
            runtime.refresh_limits_background = lambda key: cast(Callable[..., None], Runtime.refresh_limits_background)(runtime, key)
            context = ApiContext.for_schema()
            context.canvas.runtime = runtime
            app = FastAPI()
            app.include_router(create_router(context))
            try:
                with TestClient(app) as client:
                    first = client.get("/api/limits?account_key=a")
                    self.assertEqual(first.json(), snapshot)
                    self.assertTrue(entered.wait(2))
                    second = client.get("/api/limits?account_key=a")
                    self.assertEqual(second.json(), snapshot)
                    runtime.limits.assert_called_once_with("a")
            finally:
                release.set()


if __name__ == "__main__":
    unittest.main()
