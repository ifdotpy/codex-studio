#!/usr/bin/env python3
"""Windows contracts for paths used by native worker startup."""
import os
import json
from pathlib import Path
import sqlite3
import sys
import tempfile
import threading
import unittest
from contextlib import contextmanager

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))

from codex_native_input_projection import _open_rollout, accepted_turns
from codex_progress import provision_progress, read_progress
import codex_progress_layout as layout

tempfile.tempdir = str(Path.home() / "studio-dev" / "tmp")
Path(tempfile.tempdir).mkdir(parents=True, exist_ok=True)


@unittest.skipUnless(os.name == "nt", "Windows contract")
class WindowsRuntimePathsContract(unittest.TestCase):
    class Runtime:
        def __init__(self, root):
            self.root = root
            self.lock = threading.RLock()

        @contextmanager
        def db(self):
            yield object()

        def checked_actor(self, _db, agent_id):
            return {"id": agent_id}

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

    def test_progress_file_is_created_private_and_readable(self):
        with tempfile.TemporaryDirectory() as temporary:
            state = Path(temporary) / "state with spaces"
            state.mkdir()
            path = provision_progress(state, "worker_1")
            self.assertTrue(path.is_file())
            self.assertEqual(read_progress(state, "worker_1")["error"], None)
            self.assertEqual(path.read_text(encoding="utf-8"), "")

    def test_progress_reparse_points_are_rejected(self):
        with tempfile.TemporaryDirectory() as temporary:
            state = Path(temporary)
            path = provision_progress(state, "worker_1")
            target = state / "outside.md"
            target.write_text("private", encoding="utf-8")
            path.unlink()
            try:
                path.symlink_to(target)
            except OSError as error:
                self.skipTest("Windows does not allow this account to create a symlink: " + str(error))
            self.assertIsNotNone(read_progress(state, "worker_1")["error"])

    def test_progress_layout_measurement_round_trip(self):
        with tempfile.TemporaryDirectory() as temporary:
            state = Path(temporary)
            progress = provision_progress(state, "worker_1")
            progress.write_text("Ready. Проверено.\n", encoding="utf-8")
            current = read_progress(state, "worker_1")
            body = {
                "agent": "worker_1", "revision": current["revision"], "client": "win-test",
                "sequence": 1, "renderer": layout.RENDERER, "width": 320, "height": 150,
                "contentWidth": 100, "contentHeight": 40, "fits": True, "reason": None,
                "overflowX": 0, "overflowY": 0, "totalLines": 1, "visibleLines": 1,
                "lastVisibleLine": "Ready.", "lastVisibleHeading": None,
            }
            saved = layout.record_layout(self.Runtime(state), body)
            self.assertEqual(saved["status"], "fits")
            self.assertEqual(layout.layout_status(state, "worker_1")["status"], "fits")
            feedback = layout.layout_path(state, "worker_1")
            self.assertEqual(feedback.stat().st_mode & 0o222, 0)
            self.assertEqual(read_progress(state, "worker_1")["revision"], current["revision"])


if __name__ == "__main__":
    unittest.main()
