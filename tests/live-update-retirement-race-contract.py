#!/usr/bin/env python3
"""Publisher retirement preserves proven application receipts without caching absence."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import fcntl
import hashlib
import json
from pathlib import Path
import sys
import tempfile
import threading
import time
from types import SimpleNamespace
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
from codex_live_updates import LiveUpdates
from codex_source_inventory import source_files


PATCH = """def apply(runtime):
    with runtime.lock:
        runtime.attempts += 1
        if runtime.applied:
            return {'status': 'already_applied'}
        runtime.applied = True
        runtime.mutations += 1
        return {'status': 'applied'}
"""


class LiveUpdateRetirementRaceContract(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="studio-update-retirement-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()
        self.scripts = self.root / "scripts"
        self.scripts.mkdir()
        (self.scripts / "codex-canvas").write_text("# fixture entry\n")
        self.patch_path = self.scripts / "codex_fixture_update.py"
        self.patch_path.write_text(PATCH)
        self.manifest_path = self.scripts / "studio-live-update.json"
        self.receipt_path = self.root / "live-update.json"
        self.lease_path = self.scripts / ".studio-update.lock"
        self.runtime = SimpleNamespace(root=self.root, closed=False, lock=threading.RLock(),
                                       servers={"default": object()}, connection_ids={"default": "native-1"},
                                       attempts=0, mutations=0, applied=False)
        self.manager = LiveUpdates(self.runtime, self.scripts)
        self.addCleanup(self.manager.close)
        self.manifest = {"version": 1, "id": "retirement-fixture", "python": [3, 14],
                         "scope": "One isolated fixture operation", "patch": self.patch_path.name,
                         "inputs": {name: hashlib.sha256(path.read_bytes()).hexdigest()
                                    for name, path in source_files(self.scripts)}}
        self.raw = json.dumps(self.manifest).encode()
        self.manifest_path.write_bytes(self.raw)

    def applied(self):
        self.manager.tick()
        self.assertEqual(self.manager.status()["status"], "applied", self.manager.status())
        self.assertEqual((self.runtime.attempts, self.runtime.mutations), (1, 1))
        return self.manager.status(), self.receipt_path.read_bytes()

    def assert_receipt(self, saved):
        self.assertEqual(self.manager.status(), saved[0])
        self.assertEqual(self.receipt_path.read_bytes(), saved[1])
        self.assertEqual((self.runtime.attempts, self.runtime.mutations), (1, 1))

    def test_exclusive_retirement_cannot_split_manifest_read_from_shared_lease(self):
        saved = self.applied()
        servers, connections = self.runtime.servers, self.runtime.connection_ids
        read_snapshot = threading.Event()
        attempted = threading.Event()
        retired = threading.Event()
        observations = {}
        errors = []

        def publisher():
            try:
                if not read_snapshot.wait(2):
                    raise RuntimeError("The manifest read did not enter")
                with self.lease_path.open("a+b") as lease:
                    try:
                        fcntl.flock(lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    except BlockingIOError:
                        observations["blocked_by_reader"] = True
                        attempted.set()
                        fcntl.flock(lease, fcntl.LOCK_EX)
                    else:
                        observations["blocked_by_reader"] = False
                    self.patch_path.unlink()
                    self.manifest_path.unlink()
                    attempted.set()
                retired.set()
            except Exception as error:
                errors.append(error)
                attempted.set()

        read_bytes = Path.read_bytes
        first_read = True

        def manifest_read(path):
            nonlocal first_read
            raw = read_bytes(path)
            if path == self.manifest_path and first_read:
                first_read = False
                read_snapshot.set()
                if not attempted.wait(2):
                    raise RuntimeError("The exclusive publisher did not attempt its lease")
            return raw

        worker = threading.Thread(target=publisher, daemon=True)
        worker.start()
        try:
            with patch.object(Path, "read_bytes", manifest_read):
                self.manager.tick()
        finally:
            read_snapshot.set()
            worker.join(timeout=2)
        self.assertFalse(worker.is_alive(), "The exclusive publisher still holds its lease")
        self.assertEqual(errors, [])
        self.assertTrue(retired.is_set())
        self.assert_receipt(saved)
        self.assertTrue(observations["blocked_by_reader"], "The first read did not own the shared lease")
        for _ in range(2):
            self.manager.tick()
            self.assert_receipt(saved)
        self.assertIsNone(self.manager._successful)
        self.assertIs(self.runtime.servers, servers)
        self.assertIs(self.runtime.connection_ids, connections)

    def test_busy_retirement_preserves_applied_receipt_before_manifest_removal(self):
        saved = self.applied()
        with self.lease_path.open("a+b") as publisher:
            fcntl.flock(publisher, fcntl.LOCK_EX)
            self.patch_path.unlink()
            began = time.monotonic()
            with patch.object(self.manager, "_sources", wraps=self.manager._sources) as sources:
                self.manager.tick()
            self.assertLess(time.monotonic() - began, .5)
            sources.assert_not_called()
            self.assert_receipt(saved)
            self.manifest_path.unlink()
        self.manager.tick()
        self.assert_receipt(saved)
        self.assertIsNone(self.manager._successful)

    def test_identical_republication_revalidates_after_confirmed_absence(self):
        saved = self.applied()
        source_signature = self.manager._sources()
        with self.lease_path.open("a+b") as publisher:
            fcntl.flock(publisher, fcntl.LOCK_EX)
            self.manifest_path.unlink()
        self.manager.tick()
        self.assert_receipt(saved)
        self.assertIsNone(self.manager._successful)
        with self.lease_path.open("a+b") as publisher:
            fcntl.flock(publisher, fcntl.LOCK_EX)
            self.manifest_path.write_bytes(self.raw)
        self.assertEqual(self.manager._sources(), source_signature)
        self.manager.tick()
        state = self.manager.status()
        self.assertEqual((state["status"], state["patchStatus"]), ("applied", "already_applied"))
        self.assertEqual(state["manifestHash"], saved[0]["manifestHash"])
        self.assertEqual((self.runtime.attempts, self.runtime.mutations), (2, 1))

    def test_missing_manifest_never_creates_a_success_receipt(self):
        with self.lease_path.open("a+b") as publisher:
            fcntl.flock(publisher, fcntl.LOCK_EX)
            self.manifest_path.unlink()
        self.manager.tick()
        self.assertEqual(self.manager.status(), {"status": "idle"})
        self.assertFalse(self.receipt_path.exists())
        self.assertEqual((self.runtime.attempts, self.runtime.mutations), (0, 0))

    def test_current_unknown_source_hash_still_invalidates_applied_cache(self):
        saved = self.applied()
        with self.lease_path.open("a+b") as publisher:
            fcntl.flock(publisher, fcntl.LOCK_EX)
            self.manifest["inputs"]["codex-canvas"] = "0" * 64
            self.manifest_path.write_text(json.dumps(self.manifest))
        self.manager.tick()
        state = self.manager.status()
        self.assertEqual(state["status"], "failed", state)
        self.assertIn("source hash differs", state["error"])
        self.assertNotEqual(state["manifestHash"], saved[0]["manifestHash"])
        self.assertEqual((self.runtime.attempts, self.runtime.mutations), (1, 1))
        self.assertIsNone(self.manager._successful)
        with self.lease_path.open("a+b") as publisher:
            fcntl.flock(publisher, fcntl.LOCK_EX)
            self.manifest_path.unlink()
        failed_receipt = self.receipt_path.read_bytes()
        self.manager.tick()
        self.assertEqual(self.manager.status(), state)
        self.assertEqual(self.receipt_path.read_bytes(), failed_receipt)


if __name__ == "__main__":
    unittest.main()
