#!/usr/bin/env python3
"""Delayed model metadata cannot create workers or block coordination locks."""
import concurrent.futures
import importlib.util
from pathlib import Path
import sys
import tempfile
import threading
import unittest

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
from codex_catalog import CatalogPending, CatalogUnavailable, ModelCatalogCache

fixture_spec = importlib.util.spec_from_file_location("catalog_fixture", ROOT / "tests/runtime-contract.py")
fixture = importlib.util.module_from_spec(fixture_spec)
fixture_spec.loader.exec_module(fixture)

CATALOG = {"data": [{"model": "gpt-6-astra", "defaultReasoningEffort": "medium",
                     "supportedReasoningEfforts": [{"reasoningEffort": "medium"}],
                     "serviceTiers": []}], "nextCursor": None}


class MetadataServer:
    def __init__(self):
        self.requests = []
        self.requested = threading.Event()

    def submit(self, method, params):
        assert method == "model/list" and params == {"limit": 100}
        future = concurrent.futures.Future()
        self.requests.append(future)
        self.requested.set()
        return future

    def on_result(self, submitted, callback):
        submitted.add_done_callback(callback)


class CatalogCacheContract(unittest.TestCase):
    def setUp(self):
        self.now = 1000
        self.cache = ModelCatalogCache(wait_seconds=.01, clock=lambda: self.now)
        self.server = MetadataServer()
        self.current = True

    def read(self, account="a", server=None, connection="one"):
        return self.cache.read(account, server or self.server, connection, lambda: self.current)

    def warm(self):
        with self.assertRaises(CatalogPending):
            self.read()
        self.server.requests[0].set_result(CATALOG)
        return self.read()

    def test_timeout_single_flight_late_response_and_defensive_copy(self):
        with self.assertRaisesRegex(CatalogPending, "no workers were created"):
            self.read()
        with self.assertRaises(CatalogPending):
            self.read()
        self.assertEqual(len(self.server.requests), 1)
        self.server.requests[0].set_result(CATALOG)
        result = self.read()
        self.assertEqual(result, CATALOG)
        result["data"][0]["model"] = "invented"
        self.assertEqual(self.read(), CATALOG)
        self.assertEqual(len(self.server.requests), 1)

    def test_expired_success_needs_fresh_read_and_preserves_exact_pending(self):
        self.warm()
        self.now += 301
        for _ in range(2):
            with self.assertRaises(CatalogPending):
                self.read()
        self.assertEqual(len(self.server.requests), 2)
        self.server.requests[1].set_result({"data": []})
        self.assertEqual(self.read(), {"data": []})

    def test_account_and_connection_are_independent_admission_boundaries(self):
        self.warm()
        with self.assertRaises(CatalogPending):
            self.read(account="b")
        with self.assertRaises(CatalogPending):
            self.read(connection="two")
        replacement = MetadataServer()
        with self.assertRaises(CatalogPending):
            self.read(server=replacement, connection="two")
        self.assertEqual(len(replacement.requests), 1)
        self.assertEqual(len(self.server.requests), 3)

    def test_late_old_connection_cannot_replace_new_snapshot(self):
        with self.assertRaises(CatalogPending):
            self.read()
        replacement = MetadataServer()
        with self.assertRaises(CatalogPending):
            self.read(server=replacement, connection="two")
        replacement.requests[0].set_result(CATALOG)
        self.server.requests[0].set_result({"data": []})
        self.assertEqual(self.read(server=replacement, connection="two"), CATALOG)

    def test_disconnect_and_invalid_response_cannot_admit_models(self):
        with self.assertRaises(CatalogPending):
            self.read()
        self.current = False
        self.server.requests[0].set_result(CATALOG)
        with self.assertRaises(CatalogUnavailable):
            self.read()
        self.current = True
        with self.assertRaises(CatalogPending):
            self.read()
        self.server.requests[1].set_result({"data": ["not a model"]})
        with self.assertRaises(CatalogPending):
            self.read()
        self.assertEqual(len(self.server.requests), 3)
        self.server.requests[2].set_result(CATALOG)
        self.assertEqual(self.read(), CATALOG)

    def test_submission_unknown_uses_retained_native_future(self):
        from codex_runtime import SubmissionUnknown
        class UnknownServer(MetadataServer):
            def submit(self, method, params):
                future = super().submit(method, params)
                raise SubmissionUnknown((1, method, future), OSError("pipe error"))

            def on_result(self, submitted, callback):
                submitted[2].add_done_callback(callback)
        self.server = UnknownServer()
        with self.assertRaises(CatalogPending):
            self.read()
        self.server.requests[0].set_result(CATALOG)
        self.assertEqual(self.read(), CATALOG)
        self.assertEqual(len(self.server.requests), 1)

    def test_concurrent_callers_share_native_request_and_bounded_wait(self):
        with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
            calls = [pool.submit(self.read) for _ in range(8)]
            for call in calls:
                with self.assertRaises(CatalogPending):
                    call.result(1)
        self.assertEqual(len(self.server.requests), 1)

    def test_blocked_ordered_callback_does_not_delay_native_metadata(self):
        class QueuedServer(MetadataServer):
            def __init__(self):
                super().__init__()
                self.ordered_callbacks = []

            def submit(self, method, params):
                return (1, method, super().submit(method, params))

            def on_result(self, submitted, callback):
                # Simulates an ordered dispatcher held by an older notification.
                self.ordered_callbacks.append((submitted, callback))
        self.server = QueuedServer()
        self.cache.wait_seconds = .3
        with concurrent.futures.ThreadPoolExecutor(max_workers=1) as pool:
            result = pool.submit(self.read)
            self.assertTrue(self.server.requested.wait(1))
            self.server.requests[0].set_result(CATALOG)
            self.assertEqual(result.result(1), CATALOG)
        self.assertEqual(self.server.ordered_callbacks, [])
        self.assertEqual(self.read(), CATALOG)
        self.assertEqual(len(self.server.requests), 1)


class ControlledRuntime(fixture.Runtime):
    def schedule(self):
        while not self.closed:
            self.changed.wait(.05)
            self.changed.clear()


class DelayedServer(fixture.FakeServer):
    def __init__(self, *args):
        super().__init__(*args)
        self.metadata = MetadataServer()

    def submit(self, method, params):
        if method == "model/list":
            return self.metadata.submit(method, params)
        return super().submit(method, params)


class CatalogRuntimeContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.runtime = ControlledRuntime(Path(self.temp.name), DelayedServer)
        self.runtime._catalog_cache = ModelCatalogCache(wait_seconds=.03)
        self.lead = self.runtime.create({"name": "Lead", "prompt": "Plan", "cwd": self.temp.name}, defer=True)
        with self.runtime.lock, self.runtime.db() as db:
            self.lead.update(autoWake=True)
            self.runtime.put(db, "agents", self.lead)
        self.server = self.runtime.connect()

    def tearDown(self):
        self.runtime.close()
        self.temp.cleanup()

    def test_pending_metadata_creates_no_worker_and_late_success_reuses_catalog(self):
        spec = {"name": "One", "prompt": "Check one"}
        before = self.runtime.team(self.lead["id"])["agents"]
        with self.assertRaises(CatalogPending):
            self.runtime.create(spec, self.lead["id"], defer=True)
        self.assertEqual(self.runtime.team(self.lead["id"])["agents"], before)
        self.server.metadata.requests[0].set_result(CATALOG)
        one = self.runtime.create(spec, self.lead["id"], defer=True)
        two = self.runtime.create({"name": "Two", "prompt": "Check two"}, self.lead["id"], defer=True)
        self.assertNotEqual(one["id"], two["id"])
        self.assertEqual(len(self.server.metadata.requests), 1)
        with self.assertRaisesRegex(ValueError, "not available for this account"):
            self.runtime.create({"name": "Other", "prompt": "Check", "model": "invented"}, self.lead["id"], defer=True)

    def test_native_wait_does_not_hold_runtime_or_connection_lock(self):
        self.runtime._catalog_cache.wait_seconds = .2
        with concurrent.futures.ThreadPoolExecutor(max_workers=1) as pool:
            pending = pool.submit(self.runtime.catalog)
            self.assertTrue(self.server.metadata.requested.wait(1))
            for lock in (self.runtime.lock, self.runtime.start_lock):
                acquired = lock.acquire(timeout=.03)
                self.assertTrue(acquired)
                if acquired:
                    lock.release()
            self.assertEqual(len(self.runtime.team(self.lead["id"])["agents"]), 1)
            with self.assertRaises(CatalogPending):
                pending.result(1)


if __name__ == "__main__":
    unittest.main()
