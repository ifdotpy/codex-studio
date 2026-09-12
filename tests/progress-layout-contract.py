#!/usr/bin/env python3
"""Renderer feedback refers to the exact file and preserves narrow-client failures."""
import fcntl
from contextlib import redirect_stdout
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
from email.message import Message
from types import SimpleNamespace
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
import codex_canvas
import codex_progress_layout as layout
from codex_progress import provision_progress, read_progress
from codex_progress_update import source_function
from codex_remote import RemoteAccess

spec = importlib.util.spec_from_file_location("progress_file_fixture", ROOT / "tests/progress-file-contract.py")
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class ProgressLayoutContract(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="studio-progress-layout-")
        self.root = Path(self.temporary.name)
        self.runtime = fixture.Fixture(self.root)
        for key in ("first", "second"):
            self.runtime.put(self.runtime.connection, "agents", {"id": key, "rootId": key})
        self.runtime.connection.commit()
        self.path = provision_progress(self.root, "first")
        self.path.write_text("Ready. Проверено.\n", encoding="utf-8")
        self.feedback = layout.layout_path(self.root, "first")

    def tearDown(self):
        self.runtime.connection.close()
        self.temporary.cleanup()

    def report(self, **changes):
        return {"agent": "first", "revision": read_progress(self.root, "first")["revision"],
                "client": "phone", "sequence": 1, "renderer": layout.RENDERER,
                "width": 320, "height": 150, "contentWidth": 100, "contentHeight": 40,
                "fits": True, "reason": None, **changes}

    def record(self, **changes):
        return layout.record_layout(self.runtime, self.report(**changes))

    def rewrite_feedback(self, value):
        self.feedback.chmod(0o600)
        self.feedback.write_text(json.dumps(value))

    def test_exact_revision_hash_and_file_data_retained(self):
        before = read_progress(self.root, "first")
        result = self.record()
        self.assertEqual(result["revision"], before["revision"])
        self.assertEqual(result["sha256"], hashlib.sha256(self.path.read_bytes()).hexdigest())
        self.assertEqual(layout.layout_status(self.root, "first")["status"], "fits")
        self.assertEqual(read_progress(self.root, "first"), before)
        self.assertEqual(self.feedback.stat().st_mode & 0o777, 0o444)

    def test_stale_revision_rejected_and_new_file_unmeasured(self):
        body = self.report()
        self.record()
        previous = self.feedback.read_bytes()
        self.path.write_text("A different current result.\n")
        with self.assertRaises(layout.LayoutConflict):
            layout.record_layout(self.runtime, body)
        self.assertEqual(self.feedback.read_bytes(), previous)
        self.assertEqual(layout.layout_status(self.root, "first")["status"], "unmeasured")
        self.record(sequence=2)
        self.assertEqual(layout.layout_status(self.root, "first")["status"], "fits")

    def test_out_of_order_and_conflicting_equal_sequence_refused(self):
        self.record(sequence=3, fits=False, reason="overflow", contentHeight=170)
        before = self.feedback.read_bytes()
        for sequence in (1, 2, 3):
            with self.subTest(sequence=sequence), self.assertRaises(layout.LayoutConflict):
                self.record(sequence=sequence)
        self.assertEqual(self.feedback.read_bytes(), before)

    def test_identical_lost_reply_retry_does_not_refresh_age(self):
        now = time.time()
        with patch.object(layout.time, "time", return_value=now):
            original = self.record()
        before = self.feedback.read_bytes()
        with patch.object(layout.time, "time", return_value=now + 30):
            repeated = self.record()
        self.assertEqual(repeated["reports"], original["reports"])
        self.assertEqual(self.feedback.read_bytes(), before)
        with patch.object(layout.time, "time", return_value=now + layout.FRESH_SECONDS):
            self.assertEqual(layout.layout_status(self.root, "first")["status"], "unmeasured")

    def test_wide_success_preserves_narrow_failure_and_client_can_recover(self):
        self.record(fits=False, reason="overflow", contentWidth=350)
        wide = self.record(client="desktop", width=900, contentWidth=350)
        self.assertEqual(wide["status"], "does_not_fit")
        self.assertEqual({row["client"] for row in wide["reports"]}, {"phone", "desktop"})
        self.assertEqual(layout.layout_status(self.root, "first")["status"], "does_not_fit")
        self.assertEqual(self.record(sequence=2)["status"], "fits")

    def test_expiry_future_clock_and_wrong_renderer_are_unmeasured(self):
        now = time.time()
        with patch.object(layout.time, "time", return_value=now):
            value = self.record()
        for timestamp in (now + layout.FRESH_SECONDS, now - 1):
            with patch.object(layout.time, "time", return_value=timestamp):
                self.assertEqual(layout.layout_status(self.root, "first")["status"], "unmeasured")
        value["reports"][0]["renderer"] = "old-renderer"
        self.rewrite_feedback(value)
        self.assertEqual(layout.layout_status(self.root, "first")["status"], "unmeasured")

    def test_wrong_hash_never_carries_forward_reports(self):
        value = self.record(fits=False, reason="overflow", contentHeight=170)
        value["sha256"] = "0" * 64
        self.rewrite_feedback(value)
        self.assertEqual(layout.layout_status(self.root, "first")["status"], "unmeasured")
        result = self.record(client="desktop", width=900)
        self.assertEqual([row["client"] for row in result["reports"]], ["desktop"])

    def test_file_edit_during_status_read_never_reports_old_fit(self):
        self.record()
        original = layout._read_layout
        def changed(directory):
            result = original(directory)
            self.path.write_text("An edited current revision without any measurement.\n")
            return result
        with patch.object(layout, "_read_layout", side_effect=changed):
            result = layout.layout_status(self.root, "first")
        self.assertNotEqual(result["status"], "fits")
        self.assertEqual(layout.layout_status(self.root, "first")["status"], "unmeasured")

    def test_edit_during_save_preserves_previous_feedback_and_removes_temporary(self):
        self.record()
        before = self.feedback.read_bytes()
        body = self.report(sequence=2)
        original = layout.read_progress
        calls = 0
        def changed(*args):
            nonlocal calls
            calls += 1
            if calls == 2:
                self.path.write_text("Changed during measurement save.\n")
            return original(*args)
        with patch.object(layout, "read_progress", side_effect=changed), self.assertRaises(layout.LayoutConflict):
            layout.record_layout(self.runtime, body)
        self.assertEqual(self.feedback.read_bytes(), before)
        self.assertEqual(list(self.path.parent.glob(".layout-*.tmp")), [])

    def test_invalid_and_corrupt_reports_never_count_as_fit(self):
        for changes in ({"sequence": True}, {"sequence": -1}, {"width": float("nan")},
                        {"height": 151}, {"contentHeight": 151}, {"client": "../other"},
                        {"fits": 1}, {"reason": "overflow"}, {"renderer": "unknown"},
                        {"extra": 1}, {"fits": False}):
            with self.subTest(changes=changes), self.assertRaises(ValueError):
                self.record(**changes)
        value = self.record()
        for changes in ({"sequence": "invalid"}, {"width": -1}, {"contentWidth": 9000},
                        {"client": None}, {"measuredAt": "now"}, {"extra": True}):
            changed = {**value, "reports": [{**value["reports"][0], **changes}]}
            self.rewrite_feedback(changed)
            self.assertEqual(layout.layout_status(self.root, "first")["status"], "unmeasured")
            self.record(sequence=2)
        self.feedback.chmod(0o600)
        self.feedback.write_bytes(b"\xffbroken-json")
        self.assertEqual(layout.layout_status(self.root, "first")["status"], "unmeasured")

    def test_actor_guard_precedes_files_and_io_releases_shared_locks(self):
        self.runtime.put(self.runtime.connection, "agents", {"id": "second", "deletedAt": 1})
        self.runtime.connection.commit()
        with patch.object(layout, "_directory") as directory:
            for agent in ("missing", "second", "../first"):
                with self.assertRaises(ValueError):
                    self.record(agent=agent)
            directory.assert_not_called()
        original = layout.read_progress
        def checked(*args):
            self.assertFalse(self.runtime.in_db)
            self.assertFalse(self.runtime.lock._is_owned())
            return original(*args)
        with patch.object(layout, "read_progress", side_effect=checked):
            self.record()

    def test_contended_lock_is_bounded_and_retry_retains_clients(self):
        self.record(client="desktop")
        lock = os.open(self.path.parent / ".layout.lock", os.O_RDWR)
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            start = time.monotonic()
            with self.assertRaises(layout.LayoutConflict):
                self.record()
            self.assertLess(time.monotonic() - start, 1)
        finally:
            os.close(lock)
        self.assertEqual(len(self.record()["reports"]), 2)

    def test_symlink_and_nonregular_lock_feedback_and_progress_refuse(self):
        outside = self.root / "outside"
        outside.write_bytes(b"retained")
        lock = self.path.parent / ".layout.lock"
        for location in (lock, self.feedback):
            for kind in ("symlink", "fifo", "directory"):
                with self.subTest(location=location.name, kind=kind):
                    if kind == "symlink":
                        location.symlink_to(outside)
                    elif kind == "fifo":
                        os.mkfifo(location)
                    else:
                        location.mkdir()
                    with self.assertRaises((OSError, ValueError)):
                        self.record()
                    if kind == "directory":
                        location.rmdir()
                    else:
                        location.unlink()
        body = self.report()
        self.path.unlink()
        self.path.symlink_to(outside)
        with self.assertRaises(layout.LayoutConflict):
            layout.record_layout(self.runtime, body)
        self.assertEqual(outside.read_bytes(), b"retained")

    def test_client_limit_refuses_without_dropping_existing_measurement(self):
        for index in range(layout.MAX_CLIENTS):
            self.record(client=f"client_{index}")
        before = self.feedback.read_bytes()
        with self.assertRaises(layout.LayoutConflict):
            self.record(client="another")
        self.assertEqual(self.feedback.read_bytes(), before)
        self.assertEqual(len(self.record(client="client_0", sequence=2)["reports"]), layout.MAX_CLIENTS)

    def test_cli_codes_include_empty_unknown_and_invalid_file(self):
        def run(path=self.path):
            result = subprocess.run([sys.executable, str(ROOT / "scripts/codex_progress_layout.py"), str(path)],
                                    capture_output=True, text=True, timeout=10)
            return result.returncode, json.loads(result.stdout)["status"]
        self.assertEqual(run(), (2, "unmeasured"))
        self.record()
        self.assertEqual(run(), (0, "fits"))
        self.record(sequence=2, fits=False, reason="unsupported")
        self.assertEqual(run(), (1, "does_not_fit"))
        self.path.write_text("")
        self.assertEqual(run(), (0, "empty"))
        self.assertEqual(run(self.root / "wrong.md"), (2, "unmeasured"))

    def test_actual_http_methods_require_token_origin_workspace_and_actor(self):
        # Execute the real route and trust methods with their real closure policy.
        # No listener, background thread, or live runtime is started.
        source = (ROOT / "scripts/codex_canvas.py").read_text()
        values = {"canvas": SimpleNamespace(runtime=self.runtime),
                  "remote": RemoteAccess(self.root), "token": "fixture-token",
                  "sync": lambda: SimpleNamespace(identity=lambda: {"workspaceId": "fixture-workspace"})}
        def cell(value):
            return (lambda: value).__closure__[0]
        methods = {}
        for name in ("trusted", "do_POST"):
            path = ("make_server", "Handler", name)
            function, _ = source_function(source, path, vars(codex_canvas))
            closure = tuple(cell(values.get(key)) for key in function.__code__.co_freevars) or None
            methods[name] = source_function(source, path, vars(codex_canvas), closure=closure)[0]
        handler = type("FixtureHandler", (), methods)()
        handler.send = lambda value, status=200: (status, value)
        handler.server = SimpleNamespace(server_port=43210)
        handler.connection = SimpleNamespace(settimeout=lambda seconds: None)
        handler.path = "/api/panel/layout"
        def post(changes=None, body=None, peer="127.0.0.1"):
            payload = json.dumps(body or self.report()).encode()
            headers = {"Host": "127.0.0.1:43210", "Origin": "http://127.0.0.1:43210",
                       "X-Canvas-Token": "fixture-token", "X-Canvas-Workspace": "fixture-workspace",
                       "Content-Type": "application/json", "Content-Length": str(len(payload)), **(changes or {})}
            handler.headers = Message()
            for key, value in headers.items():
                if value is not None:
                    handler.headers[key] = value
            handler.client_address = (peer, 12)
            handler.rfile = io.BytesIO(payload)
            return handler.do_POST()
        for headers in ({"X-Canvas-Token": None}, {"X-Canvas-Token": "wrong"},
                        {"Origin": "https://evil.example"}, {"Sec-Fetch-Site": "cross-site"},
                        {"Host": "evil.example"}):
            self.assertEqual(post(headers)[0], 403)
        self.assertEqual(post(peer="192.0.2.1")[0], 403)
        self.assertEqual(post({"X-Canvas-Workspace": "other-workspace"})[0], 409)
        self.assertEqual(post(body=self.report(agent="missing"))[0], 400)
        self.assertFalse(self.feedback.exists())
        self.assertEqual(post()[0], 200)
        self.assertEqual(post(body=self.report(revision="stale"))[0], 409)
        self.assertEqual(post(body=self.report(height=151))[0], 400)

    def test_cli_wait_stops_on_measurement_or_deadline(self):
        for measured, expected in ((True, 0), (False, 2)):
            statuses = ([{"status": "unmeasured"}, {"status": "fits"}] if measured
                        else [{"status": "unmeasured"}])
            output = io.StringIO()
            with patch.object(sys, "argv", ["layout", str(self.path), "--wait", "0.2"]), \
                    patch.object(layout, "layout_status", side_effect=statuses), \
                    patch.object(layout.time, "monotonic", side_effect=[0, 0, 0] if measured else [0, 0.2]), \
                    patch.object(layout.time, "sleep") as sleeper, redirect_stdout(output):
                self.assertEqual(layout.main(), expected)
            self.assertEqual(sleeper.call_count, int(measured))
        for wait in ("-1", "5.01", "nan", "inf"):
            result = subprocess.run([sys.executable, str(ROOT / "scripts/codex_progress_layout.py"),
                                     str(self.path), "--wait", wait], capture_output=True, timeout=10)
            self.assertEqual(result.returncode, 2)


if __name__ == "__main__":
    unittest.main()
