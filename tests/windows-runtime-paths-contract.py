#!/usr/bin/env python3
"""Windows contracts for paths used by native worker startup."""
import os
import json
from pathlib import Path
import sqlite3
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))

from codex_native_input_projection import _open_rollout, accepted_turns
from codex_progress import provision_progress, read_progress

tempfile.tempdir = str(Path.home() / "studio-dev" / "tmp")
Path(tempfile.tempdir).mkdir(parents=True, exist_ok=True)


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


if __name__ == "__main__":
    unittest.main()
