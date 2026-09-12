#!/usr/bin/env python3
"""Plain progress reads preserve agent identity, file data, and legacy state."""

from contextlib import contextmanager
import json
import os
from pathlib import Path
import sqlite3
import sys
import tempfile
import threading
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
from codex_panel import PanelMixin, panel_tools
from codex_progress import MAX_PROGRESS_BYTES, progress_context, progress_path, provision_progress, read_progress
from codex_work import WorkMixin


class Fixture(PanelMixin):
    checked_actor = WorkMixin.checked_actor

    def __init__(self, root):
        self.root = root
        self.lock = threading.RLock()
        self.in_db = False
        self.connection = sqlite3.connect(root / "fixture.sqlite3")
        self.connection.execute("CREATE TABLE IF NOT EXISTS runtime_agents(id TEXT PRIMARY KEY, record TEXT)")
        self.setup_panels(self.connection)
        self.connection.commit()

    @contextmanager
    def db(self):
        self.in_db = True
        try:
            with self.connection:
                yield self.connection
        finally:
            self.in_db = False

    def agent(self, key, db):
        row = db.execute("SELECT record FROM runtime_agents WHERE id=?", (key,)).fetchone()
        if row is None:
            raise ValueError("Unknown agent")
        return json.loads(row[0])

    def put(self, db, table, row):
        db.execute("INSERT OR REPLACE INTO runtime_" + table + " VALUES (?,?)", (row["id"], json.dumps(row)))


class ProgressContract(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="codex-progress-contract-")
        self.root = Path(self.temporary.name)
        self.runtime = Fixture(self.root)
        for key in ("first", "second"):
            self.runtime.put(self.runtime.connection, "agents", {"id": key, "rootId": key, "autoWake": True})
        self.runtime.connection.commit()

    def tearDown(self):
        self.runtime.connection.close()
        self.temporary.cleanup()

    def test_missing_create_edit_remove_and_restart(self):
        missing = self.runtime.get_panel("first")
        self.assertEqual((missing["format"], missing["markdown"], missing["exists"], missing["revision"], missing["error"]),
                         ("markdown", "", False, None, None))
        self.assertFalse((self.root / "progress").exists(), "A read must not provision state")
        path = provision_progress(self.root, "first")
        self.assertEqual(path, Path(missing["path"]))
        self.assertEqual(path.stat().st_mode & 0o777, 0o600)
        self.assertEqual(path.parent.stat().st_mode & 0o777, 0o700)
        empty = self.runtime.get_panel("first")
        self.assertTrue(empty["exists"])
        self.assertIsNotNone(empty["revision"])
        text = "# Проверка\n\n- [x] Tested\n- [ ] Review\n"
        path.write_text(text, encoding="utf-8")
        self.assertEqual(provision_progress(self.root, "first"), path)
        edited = self.runtime.get_panel("first")
        self.assertEqual(edited["markdown"], text)
        self.assertNotEqual(edited["revision"], empty["revision"])
        self.runtime.connection.close()
        self.runtime = Fixture(self.root)
        self.assertEqual(self.runtime.get_panel("first"), edited)
        path.unlink()
        self.assertEqual(self.runtime.get_panel("first"), missing)

    def test_agent_isolation_and_identity_checked_before_file_read(self):
        for key in ("first", "second"):
            provision_progress(self.root, key).write_text(key)
            self.assertEqual(self.runtime.get_panel(key)["markdown"], key)
        self.runtime.put(self.runtime.connection, "agents", {"id": "second", "deletedAt": 1})
        self.runtime.connection.commit()
        with patch("codex_progress.read_progress") as reader:
            for key in ("unknown", "second", "../first"):
                with self.assertRaises(ValueError):
                    self.runtime.get_panel(key)
            reader.assert_not_called()
        for key in ("../first", "first/second", "", ".", ".."):
            with self.assertRaises(ValueError):
                progress_path(self.root, key)

    def test_file_io_occurs_outside_runtime_and_database_lock(self):
        def read(root, agent):
            self.assertFalse(self.runtime.lock._is_owned())
            self.assertFalse(self.runtime.in_db)
            return {"agent": agent}
        with patch("codex_progress.read_progress", side_effect=read):
            self.assertEqual(self.runtime.get_panel("first"), {"agent": "first"})

    def test_utf8_and_byte_limit_fail_visibly_without_truncation(self):
        path = provision_progress(self.root, "first")
        path.write_bytes(b"x" * MAX_PROGRESS_BYTES)
        self.assertEqual(len(self.runtime.get_panel("first")["markdown"]), MAX_PROGRESS_BYTES)
        for content, reason in ((b"x" * (MAX_PROGRESS_BYTES + 1), "128 KiB"), (b"\xff", "UTF-8")):
            path.write_bytes(content)
            result = self.runtime.get_panel("first")
            self.assertIn(reason, result["error"])
            self.assertEqual(result["markdown"], "")
            self.assertIsNone(result["revision"])
            self.assertEqual(path.read_bytes(), content)

    def test_atomic_replace_exposes_complete_revisions(self):
        path = provision_progress(self.root, "first")
        path.write_text("before")
        before = self.runtime.get_panel("first")
        replacement = path.with_suffix(".new")
        replacement.write_text("after")
        original_open = os.open
        replaced = False
        def opening(name, *args, **kwargs):
            nonlocal replaced
            descriptor = original_open(name, *args, **kwargs)
            if name == "PROGRESS.md" and not replaced:
                replaced = True
                replacement.replace(path)
            return descriptor
        with patch("codex_progress.os.open", side_effect=opening):
            during = self.runtime.get_panel("first")
        self.assertEqual(during["markdown"], "before")
        after = self.runtime.get_panel("first")
        self.assertEqual(after["markdown"], "after")
        self.assertNotEqual(after["revision"], before["revision"])
        self.assertIsNone(after["error"])

    def test_in_place_change_during_read_returns_visible_retry_error(self):
        path = provision_progress(self.root, "first")
        path.write_text("before")
        original_fstat = os.fstat
        calls = 0
        def changed(descriptor):
            nonlocal calls
            calls += 1
            if calls == 2:
                path.write_text("changed size")
            return original_fstat(descriptor)
        with patch("codex_progress.os.fstat", side_effect=changed):
            result = self.runtime.get_panel("first")
        self.assertIn("changed during the read", result["error"])
        self.assertEqual(result["markdown"], "")
        self.assertEqual(self.runtime.get_panel("first")["markdown"], "changed size")

    def test_read_error_is_visible_and_later_read_recovers(self):
        path = provision_progress(self.root, "first")
        path.write_text("retained content")
        original_open = os.open
        def denied(name, *args, **kwargs):
            if name == "PROGRESS.md":
                raise PermissionError("fixture read denied")
            return original_open(name, *args, **kwargs)
        with patch("codex_progress.os.open", side_effect=denied):
            result = self.runtime.get_panel("first")
        self.assertIn("fixture read denied", result["error"])
        self.assertEqual(result["markdown"], "")
        self.assertIsNone(result["revision"])
        self.assertEqual(self.runtime.get_panel("first")["markdown"], "retained content")

    def test_symlinks_and_fifo_never_read_another_file_or_block(self):
        first = provision_progress(self.root, "first")
        second = provision_progress(self.root, "second")
        second.write_text("private second")
        first.unlink()
        first.symlink_to(second)
        self.assertIsNotNone(self.runtime.get_panel("first")["error"])
        with self.assertRaisesRegex(ValueError, "regular file"):
            provision_progress(self.root, "first")
        first.unlink()
        os.mkfifo(first)
        self.assertIn("regular file", self.runtime.get_panel("first")["error"])
        first.unlink()
        first.parent.rmdir()
        first.parent.symlink_to(second.parent, target_is_directory=True)
        self.assertIsNotNone(self.runtime.get_panel("first")["error"])
        self.assertEqual(second.read_text(), "private second")

    def test_read_and_provision_preserve_legacy_rows_receipts_and_active_feed(self):
        panel = {"id": "first", "agent": "first", "version": 7, "html": "legacy", "css": "",
                 "callbacks": [{"id": "go", "label": "Go"}],
                 "feed": {"status": "running", "monitorId": "active-monitor"}}
        self.runtime.put(self.runtime.connection, "panels", panel)
        self.runtime.connection.execute("INSERT INTO runtime_panel_callbacks VALUES (?,?,?,?,?)",
                                        ("first", 7, "go", "{}", '{"status":"pending"}'))
        before = list(self.runtime.connection.iterdump())
        provision_progress(self.root, "first").write_text("new markdown")
        self.runtime.get_panel("first")
        self.assertEqual(list(self.runtime.connection.iterdump()), before)
        self.assertEqual(self.runtime.panel("first")["feed"]["status"], "running")
        self.assertEqual(self.runtime.panel("first")["submittedCallbacks"], ["go"])
        self.assertEqual(panel_tools(lambda *args: self.fail("Retired schema constructed"), {}), [])
        context = progress_context(self.root, "first")
        self.assertIn(str(progress_path(self.root, "first")), context)
        self.assertIn("ordinary file tools", context)
        self.assertEqual(self.runtime.get_panel("first")["markdown"], "new markdown")


if __name__ == "__main__":
    unittest.main()
