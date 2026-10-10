#!/usr/bin/env python3
"""Local release validation never replaces native connections or repeats work."""

# Support direct execution without runner-provided PYTHONPATH.
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import fcntl
import hashlib
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

ROOT = REPOSITORY_ROOT
sys.path.insert(0, str(SERVER_SOURCE_ROOT))
from codex_live_updates import LiveUpdates, start
from codex_source_inventory import source_files
from codex_backend_identity import backend_build

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
                     name: hashlib.sha256(path.read_bytes()).hexdigest()
                     for name, path in source_files(self.scripts)}}
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
        publish = runpy.run_path(str(SERVER_SOURCE_ROOT / "codex-publish-update"))["publish"]
        receipt = publish(self.scripts, "codex_fixture_update.py", "release-1", "One fixture function")
        self.assertEqual(receipt["id"], "release-1")
        self.manager.tick()
        self.assertEqual(self.manager.status()["status"], "applied")
        self.assertEqual(self.manager.status()["scope"], "One fixture function")
        self.assertEqual(self.runtime.mutations, 1)

    def test_nested_package_is_identity_and_manifest_source_but_fixtures_are_excluded(self):
        package = self.scripts / "analytics"
        package.mkdir()
        (package / "__init__.py").write_text("\n")
        nested = package / "rollout_parser.py"
        nested.write_text("VALUE = 1\n")
        excluded = package / "tests"
        excluded.mkdir()
        (excluded / "fixture.py").write_text("VALUE = 1\n")
        benchmark = package / "benchmarks"
        benchmark.mkdir()
        (benchmark / "measure.py").write_text("VALUE = 1\n")
        sources = dict(source_files(self.scripts))
        self.assertIn("analytics/__init__.py", sources)
        self.assertIn("analytics/rollout_parser.py", sources)
        self.assertNotIn("analytics/tests/fixture.py", sources)
        self.assertNotIn("analytics/benchmarks/measure.py", sources)
        before = backend_build(self.scripts)
        nested.write_text("VALUE = 2\n")
        self.assertNotEqual(backend_build(self.scripts), before)
        publish = runpy.run_path(str(SERVER_SOURCE_ROOT / "codex-publish-update"))["publish"]
        published = publish(self.scripts, "codex_fixture_update.py", "nested-release", "Nested package")
        manifest = json.loads((self.scripts / "studio-live-update.json").read_text())
        self.assertIn("analytics/rollout_parser.py", manifest["inputs"])
        self.assertNotIn("analytics/tests/fixture.py", manifest["inputs"])
        self.assertNotIn("analytics/benchmarks/measure.py", manifest["inputs"])
        self.assertEqual(published["inputs"], len(source_files(self.scripts)))
        self.manager.tick()
        self.assertEqual(self.manager.status()["status"], "applied")

    def test_top_level_only_identity_preserves_legacy_digest(self):
        files = sorted(path for path in self.scripts.iterdir()
                       if path.is_file() and (path.suffix == ".py" or path.name == "codex-canvas"))
        digest = hashlib.sha256()
        for path in files:
            digest.update((path.name + "\0" + hashlib.sha256(path.read_bytes()).hexdigest() + "\n").encode())
        self.assertEqual(backend_build(self.scripts), digest.hexdigest())

    def test_nested_tampering_missing_coverage_traversal_and_symlinks_fail_closed(self):
        package = self.scripts / "analytics"
        package.mkdir()
        (package / "__init__.py").write_text("\n")
        nested = package / "rollout_parser.py"
        nested.write_text("VALUE = 1\n")
        manifest = self.manifest()
        manifest["inputs"].update({name: hashlib.sha256(path.read_bytes()).hexdigest()
                                   for name, path in source_files(self.scripts)
                                   if name.startswith("analytics/")})
        (self.scripts / "studio-live-update.json").write_text(json.dumps(manifest))
        nested.write_text("VALUE = 2\n")
        self.manager.tick()
        self.assertEqual(self.manager.status()["status"], "failed")
        self.assertEqual(self.runtime.attempts, 0)

        nested.write_text("VALUE = 1\n")
        manifest = self.manifest(id="missing-nested")
        manifest["inputs"].pop("analytics/rollout_parser.py", None)
        (self.scripts / "studio-live-update.json").write_text(json.dumps(manifest))
        self.manager.tick()
        self.assertEqual(self.manager.status()["status"], "failed")

        manifest = self.manifest(id="traversal")
        manifest["inputs"]["../outside.py"] = "0" * 64
        (self.scripts / "studio-live-update.json").write_text(json.dumps(manifest))
        self.manager.tick()
        self.assertEqual(self.manager.status()["status"], "failed")

        self.manifest(id="symlink")
        (package / "linked.py").symlink_to(nested)
        with self.assertRaisesRegex(ValueError, "symlink"):
            source_files(self.scripts)
        self.manager.tick()
        self.assertEqual(self.manager.status()["status"], "failed")
        publish = runpy.run_path(str(SERVER_SOURCE_ROOT / "codex-publish-update"))["publish"]
        with self.assertRaisesRegex(ValueError, "symlink"):
            publish(self.scripts, "codex_fixture_update.py", "symlink-release", "Must reject")

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

    def test_guarded_patch_preserves_bound_callback_and_database(self):
        from codex_source import signature
        import sqlite3
        from types import MethodType

        def transcript(runtime):
            return dict(runtime.record)

        self.runtime.record = {"message": "existing message"}
        self.runtime.transcript = MethodType(transcript, self.runtime)
        callback = self.runtime.transcript
        self.runtime.connection = sqlite3.connect(self.root / "fixture.sqlite3")
        self.addCleanup(self.runtime.connection.close)
        self.runtime.connection.execute("CREATE TABLE history(message TEXT)")
        self.runtime.connection.execute("INSERT INTO history VALUES (?)", ("existing message",))
        self.runtime.connection.commit()
        before = (self.root / "fixture.sqlite3").read_bytes()
        patch_source = """from codex_source import signature

def desired(runtime):
    return {**runtime.record, 'requestedDelivery': 'after_tool'}

def apply(runtime):
    with runtime.lock:
        function = runtime.transcript.__func__
        if signature(function) == signature(desired):
            return {'status': 'already_applied'}
        if signature(function) != EXPECTED:
            raise RuntimeError('Unknown live function')
        function.__code__ = desired.__code__
        return {'status': 'applied'}
""".replace("EXPECTED", repr(signature(transcript)))
        (self.scripts / "codex_fixture_update.py").write_text(patch_source)
        self.manifest()
        self.assertNotIn("requestedDelivery", callback())
        self.manager.tick()
        self.assertEqual(self.manager.status()["status"], "applied", self.manager.status())
        self.assertIs(callback.__func__, self.runtime.transcript.__func__)
        self.assertEqual(callback()["requestedDelivery"], "after_tool")
        self.assertEqual((self.root / "fixture.sqlite3").read_bytes(), before)
        self.assertEqual(self.runtime.connection.execute("SELECT message FROM history").fetchall(),
                         [("existing message",)])
        self.manager.tick()
        self.assertEqual(self.manager.status()["attempt"], 1)


if __name__ == "__main__":
    unittest.main()
