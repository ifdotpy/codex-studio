#!/usr/bin/env python3
"""Local release validation never replaces native connections or repeats work."""
import fcntl
import hashlib
import importlib.util
import json
import os
import runpy
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
from codex_live_updates import LiveUpdates, start

PATCH = """def apply(runtime):
    with runtime.lock:
        runtime.attempts += 1
        if runtime.applied:
            return {'status': 'already_applied'}
        if runtime.fail:
            raise ValueError('Unknown live function')
        runtime.applied = True
        runtime.mutations += 1
        return {'status': 'applied'}
"""


class LiveUpdatesContract(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.scripts = self.root / "scripts"
        self.scripts.mkdir()
        (self.scripts / "codex-canvas").write_text("# entry point\n")
        (self.scripts / "codex_fixture_update.py").write_text(PATCH)
        self.runtime = SimpleNamespace(root=self.root, closed=False, lock=threading.RLock(),
                                       servers={"default": object()}, connection_ids={"default": "native-1"},
                                       attempts=0, mutations=0, applied=False, fail=False)
        self.manager = LiveUpdates(self.runtime, self.scripts, interval=0.01)
        self.addCleanup(self.manager.close)
        self.version = patch("codex_live_updates.sys.version_info", (3, 14))
        self.version.start()
        self.addCleanup(self.version.stop)

    def manifest(self, **overrides):
        value = {"version": 1, "id": "fixture-1", "python": [3, 14], "scope": "Fixture patch only",
                 "patch": "codex_fixture_update.py", "inputs": {
                     path.name: hashlib.sha256(path.read_bytes()).hexdigest()
                     for path in self.scripts.iterdir() if path.suffix == ".py" or path.name == "codex-canvas"}}
        value.update(overrides)
        (self.scripts / "studio-live-update.json").write_text(json.dumps(value))
        return value

    def test_apply_once_preserves_native_identity_and_durable_receipt(self):
        self.manifest()
        servers, connections = self.runtime.servers, self.runtime.connection_ids
        self.manager.tick()
        self.manager.tick()
        self.assertEqual((self.runtime.attempts, self.runtime.mutations), (1, 1))
        self.assertIs(self.runtime.servers, servers)
        self.assertIs(self.runtime.connection_ids, connections)
        state = self.manager.status()
        self.assertEqual(state["status"], "applied")
        self.assertNotIn("backendBuild", state)
        self.assertEqual(state["pid"], os.getpid())
        self.assertEqual(state["scope"], "Fixture patch only")
        self.assertEqual(len(state["managerId"]), 32)
        self.assertEqual(json.loads((self.root / "live-update.json").read_text()), state)
        self.assertEqual((self.root / "live-update.json").stat().st_mode & 0o777, 0o600)

    def test_new_manager_verifies_live_code_despite_saved_receipt(self):
        self.manifest()
        self.manager.tick()
        replacement = LiveUpdates(self.runtime, self.scripts)
        replacement.tick()
        self.assertEqual((self.runtime.attempts, self.runtime.mutations), (2, 1))
        self.assertEqual(replacement.status()["patchStatus"], "already_applied")
        self.assertNotEqual(replacement.status()["managerId"], self.manager.status()["managerId"])

    def test_incomplete_changed_and_unknown_sources_never_execute(self):
        cases = ["missing", "changed", "unlisted", "path", "hash", "python", "version"]
        for case in cases:
            with self.subTest(case=case):
                extra = self.scripts / "extra.py"
                extra.unlink(missing_ok=True)
                manifest = self.manifest(id=case)
                if case == "missing":
                    manifest["inputs"].pop("codex-canvas")
                elif case == "changed":
                    manifest["inputs"]["codex-canvas"] = "0" * 64
                elif case == "unlisted":
                    extra.write_text("# unlisted\n")
                elif case == "path":
                    manifest["patch"] = "../codex_fixture_update.py"
                elif case == "hash":
                    manifest["inputs"]["codex-canvas"] = "not-a-hash"
                elif case == "python":
                    manifest["python"] = [3, 13]
                else:
                    manifest["version"] = 2
                (self.scripts / "studio-live-update.json").write_text(json.dumps(manifest))
                self.manager.tick()
                self.assertEqual(self.manager.status()["status"], "failed")
                self.assertEqual(self.runtime.attempts, 0)

    def test_shared_lock_refuses_partial_publication_without_wait(self):
        self.manifest()
        with (self.scripts / ".studio-update.lock").open("a+b") as publisher:
            fcntl.flock(publisher, fcntl.LOCK_EX)
            began = time.monotonic()
            self.manager.tick()
            self.assertLess(time.monotonic() - began, 0.5)
            self.assertEqual(self.manager.status()["status"], "waiting")
            self.assertEqual(self.runtime.attempts, 0)
        self.manifest(id="published-2")
        self.manager.tick()
        self.assertEqual(self.manager.status()["status"], "applied")

    def test_unknown_live_code_failure_retries_with_backoff(self):
        self.runtime.fail = True
        self.manifest()
        self.manager.tick()
        self.manager.tick()
        self.assertEqual(self.runtime.attempts, 1)
        self.assertEqual(self.manager.status()["status"], "failed")
        self.assertEqual(self.runtime.mutations, 0)
        self.runtime.fail = False
        self.manifest(id="replacement")
        self.manager.tick()
        self.assertEqual(self.manager.status()["status"], "applied")
        self.assertEqual(self.runtime.mutations, 1)

    def test_republished_previous_manifest_revalidates_live_code(self):
        previous = self.manifest()
        self.manager.tick()
        self.manifest(id="next-patch")
        self.manager.tick()
        (self.scripts / "studio-live-update.json").write_text(json.dumps(previous))
        self.manager.tick()
        self.assertEqual(self.runtime.attempts, 3)
        self.assertEqual(self.runtime.mutations, 1)

    def test_changed_sources_invalidate_applied_status_for_same_manifest(self):
        self.manifest()
        self.manager.tick()
        path = self.scripts / "codex-canvas"
        previous = path.read_bytes()
        path.write_text("# incompatible installed entry point\n")
        self.manager.tick()
        self.assertEqual(self.manager.status()["status"], "failed")
        self.assertEqual(self.runtime.attempts, 1)
        path.write_bytes(previous)
        self.manager.tick()
        self.assertEqual(self.manager.status()["status"], "applied")
        self.assertEqual(self.runtime.attempts, 2)

    def test_invalid_result_never_reports_applied(self):
        (self.scripts / "codex_fixture_update.py").write_text(
            "def apply(runtime):\n    return {'status': 'pending'}\n")
        self.manifest()
        self.manager.tick()
        self.assertEqual(self.manager.status()["status"], "failed")

    def test_receipt_failure_does_not_repeat_successful_patch(self):
        self.manifest()
        with patch("codex_live_updates.os.replace", side_effect=OSError("disk full")):
            self.manager.tick()
        self.assertEqual(self.manager.status()["status"], "applied")
        self.assertEqual(self.manager.status()["receiptError"], "disk full")
        self.manager.tick()
        self.assertEqual(self.runtime.attempts, 1)
        self.assertNotIn("receiptError", self.manager.status())
        self.assertEqual(json.loads((self.root / "live-update.json").read_text()), self.manager.status())

    def test_publisher_manifest_applies_and_records_explicit_scope(self):
        publish = runpy.run_path(str(ROOT / "scripts/codex-publish-update"))["publish"]
        receipt = publish(self.scripts, "codex_fixture_update.py", "release-1", "One fixture function")
        self.assertEqual(receipt["id"], "release-1")
        self.manager.tick()
        self.assertEqual(self.manager.status()["status"], "applied")
        self.assertEqual(self.manager.status()["scope"], "One fixture function")
        self.assertEqual(self.runtime.mutations, 1)

    def test_background_update_leaves_status_available_while_runtime_lock_is_busy(self):
        self.manifest()
        with self.runtime.lock:
            self.manager.start()
            deadline = time.monotonic() + 2
            while self.manager.status()["status"] != "applying" and time.monotonic() < deadline:
                time.sleep(0.005)
            began = time.monotonic()
            self.assertEqual(self.manager.status()["status"], "applying")
            self.assertLess(time.monotonic() - began, 0.1)
            self.assertFalse(self.runtime.closed)
            self.assertEqual(self.runtime.mutations, 0)
        deadline = time.monotonic() + 2
        while self.manager.status()["status"] != "applied" and time.monotonic() < deadline:
            time.sleep(0.005)
        self.assertEqual(self.manager.status()["status"], "applied")
        self.manager.close()
        self.manager._thread.join(timeout=2)
        self.assertFalse(self.manager._thread.is_alive())

    def test_start_is_idempotent_and_closed_runtime_never_applies(self):
        self.runtime.live_updates = self.manager
        self.runtime.closed = True
        self.manifest()
        self.assertIs(start(self.runtime), self.manager)
        thread = self.manager._thread
        self.assertIs(start(self.runtime)._thread, thread)
        self.manager.tick()
        self.assertEqual(self.runtime.attempts, 0)

    def test_real_guarded_patch_preserves_bound_callback_and_database(self):
        specification = importlib.util.spec_from_file_location(
            "message_intent_fixture", ROOT / "tests/message-intent-update-contract.py")
        fixture_module = importlib.util.module_from_spec(specification)
        specification.loader.exec_module(fixture_module)
        fixture_type = fixture_module.MessageIntentUpdateContract
        fixture_type.setUpClass()
        fixture = fixture_type()
        fixture.setUp()
        self.addCleanup(fixture.doCleanups)
        (self.scripts / "codex_fixture_update.py").unlink()
        for name in ("codex_message_intent_update.py", "codex_runtime.py"):
            (self.scripts / name).write_bytes((ROOT / "scripts" / name).read_bytes())
        fixture.module.__file__ = str(self.scripts / "codex_runtime.py")
        fixture.runtime.root = self.root
        manager = LiveUpdates(fixture.runtime, self.scripts)
        before = fixture.state()
        callback = fixture.runtime.transcript
        self.assertNotIn("requestedDelivery", callback("agent")["items"][0])
        self.manifest(patch="codex_message_intent_update.py")
        manager.tick()
        self.assertEqual(manager.status()["status"], "applied", manager.status())
        self.assertIs(callback.__func__, before[0])
        self.assertEqual(callback("agent")["items"][0]["requestedDelivery"], "after_tool")
        self.assertEqual(fixture.state()[2:], before[2:])
        manager.tick()
        self.assertEqual(manager.status()["attempt"], 1)


if __name__ == "__main__":
    unittest.main()
