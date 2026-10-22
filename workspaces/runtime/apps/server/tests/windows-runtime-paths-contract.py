#!/usr/bin/env python3
"""Windows contracts for paths used by native worker startup."""

from test_isolation import isolate_supervisor_environment

# Support direct execution without runner-provided PYTHONPATH.
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT

isolate_supervisor_environment()
from runtime_test_environment import make_runtime  # noqa: F401  # installs the home guard

import os
import json
from pathlib import Path
import subprocess
import sqlite3
import sys
import tempfile
import unittest
from types import SimpleNamespace
from unittest.mock import patch

ROOT = REPOSITORY_ROOT
sys.path.insert(0, str(SERVER_SOURCE_ROOT))

from codex_native_input_projection import _open_rollout, accepted_turns
from codex_private_paths import reject_reparse_path
import codex_private_paths

tempfile.tempdir = str(Path.home() / "studio-dev" / "tmp")
Path(tempfile.tempdir).mkdir(parents=True, exist_ok=True)


def _junction(link, target):
    def literal(path):
        return "'" + str(path).replace("'", "''") + "'"

    command = f"New-Item -ItemType Junction -Path {literal(link)} -Target {literal(target)} | Out-Null"
    result = subprocess.run(["powershell.exe", "-NoProfile", "-NonInteractive", "-Command", command],
                            capture_output=True, text=True, timeout=15)
    if result.returncode:
        raise AssertionError(result.stderr or result.stdout)


@unittest.skipUnless(os.name == "nt", "Windows contract")
class WindowsRuntimePathsContract(unittest.TestCase):
    def test_native_rollout_path_opens_without_posix_directory_flags(self):
        with tempfile.TemporaryDirectory() as temporary:
            home = Path(temporary) / "Codex Profile Ω"
            rollout = home / "sessions" / "thread.jsonl"
            rollout.parent.mkdir(parents=True)
            rollout.write_bytes(b'{"type":"session_meta"}\n')
            with _open_rollout(home, Path("sessions") / "thread.jsonl") as stream:
                self.assertEqual(stream.readline(), b'{"type":"session_meta"}\n')

    def test_saved_native_receipt_replay(self):
        with tempfile.TemporaryDirectory() as temporary:
            home = Path(temporary) / "Codex Profile Ω"
            sessions = home / "sessions"
            sessions.mkdir(parents=True)
            rollout = sessions / "thread.jsonl"
            item = {"type": "userMessage", "id": "item", "clientId": "input",
                    "content": [{"type": "text", "text": "Run the worker.", "text_elements": []}]}
            records = [
                {"type": "session_meta", "ordinal": 4, "payload": {"id": "thread"}},
                {"type": "event_msg", "ordinal": 5,
                 "payload": {"type": "task_started", "turn_id": "turn"}},
                {"type": "event_msg", "ordinal": 6, "payload": {
                    "type": "item_completed", "thread_id": "thread", "turn_id": "turn",
                    "item": {"type": "UserMessage", "id": "item", "client_id": "input",
                             "content": item["content"]}}},
            ]
            encoded = b"".join((json.dumps(record) + "\n").encode() for record in records)
            rollout.write_bytes(encoded)
            state_db = sqlite3.connect(home / "state_5.sqlite")
            try:
                state_db.execute("CREATE TABLE threads(id TEXT PRIMARY KEY, rollout_path TEXT)")
                state_db.execute("INSERT INTO threads VALUES (?,?)", ("thread", str(rollout)))
                state_db.commit()
            finally:
                state_db.close()
            history = sqlite3.connect(home / "thread_history_1.sqlite")
            try:
                history.execute("CREATE TABLE thread_items(thread_id,turn_id,item_id,item_type,item_json,rollout_ordinal)")
                history.execute("CREATE TABLE thread_turns(thread_id,turn_id,rollout_ordinal,rollout_byte_offset)")
                history.execute("CREATE TABLE thread_history_projection_state(thread_id,next_rollout_byte_offset,next_rollout_ordinal)")
                history.execute("INSERT INTO thread_items VALUES (?,?,?,?,?,?)",
                                ("thread", "turn", "item", "userMessage", json.dumps(item), 6))
                header_bytes = (json.dumps(records[0]) + "\n").encode()
                history.execute("INSERT INTO thread_turns VALUES (?,?,?,?)",
                                ("thread", "turn", 5, len(header_bytes)))
                history.execute("INSERT INTO thread_history_projection_state VALUES (?,?,?)",
                                ("thread", len(encoded), 7))
                history.commit()
            finally:
                history.close()
            self.assertEqual(accepted_turns(home, "thread", ["input"]), [{
                "id": "turn", "startOutcome": "accepted", "clientUserMessageId": "input",
                "items": [item],
            }])

    def test_private_path_check_rejects_a_reparse_state_root(self):
        with tempfile.TemporaryDirectory() as temporary:
            outside = Path(temporary) / "outside"
            outside.mkdir()
            root = Path(temporary) / "state"
            _junction(root, outside)
            with self.assertRaises(ValueError):
                reject_reparse_path(root, root / "multi-server", allow_missing=True)
            os.rmdir(root)

    def test_rollout_handle_check_rejects_parent_junction_swap(self):
        import codex_native_input_projection as projection

        with tempfile.TemporaryDirectory() as temporary:
            home = Path(temporary) / "profile"
            sessions = home / "sessions"
            sessions.mkdir(parents=True)
            target = sessions / "thread.jsonl"
            target.write_text("inside", encoding="utf-8")
            outside = Path(temporary) / "outside"
            outside.mkdir()
            (outside / "thread.jsonl").write_text("outside", encoding="utf-8")
            moved = sessions.with_name("sessions-saved")
            original_open = os.open
            swapped = False

            def swap(path, *args, **kwargs):
                nonlocal swapped
                if not swapped and Path(path) == target:
                    sessions.rename(moved)
                    _junction(sessions, outside)
                    swapped = True
                return original_open(path, *args, **kwargs)

            try:
                with patch.object(projection.os, "open", side_effect=swap):
                    with self.assertRaises(ValueError):
                        projection._open_rollout(home, Path("sessions") / "thread.jsonl")
                self.assertTrue(swapped)
            finally:
                if sessions.exists():
                    os.rmdir(sessions)
                moved.rename(sessions)

class PrivatePathPosixMockContract(unittest.TestCase):
    def test_nested_components_are_checked_cumulatively(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary) / "state"
            target = root / "one" / "two" / "rollout.jsonl"
            target.parent.mkdir(parents=True)
            target.write_text("ok", encoding="utf-8")
            reject_reparse_path(root, target)

            for component in (root, root / "one", root / "one" / "two", target):
                original_lstat = Path.lstat

                def marked(path, *, selected=component):
                    info = original_lstat(path)
                    if path == selected:
                        return SimpleNamespace(st_file_attributes=0x400, st_mode=info.st_mode)
                    return info

                with patch.object(Path, "lstat", marked):
                    with self.assertRaisesRegex(ValueError, "reparse point"):
                        reject_reparse_path(root, target)


if __name__ == "__main__":
    unittest.main()
