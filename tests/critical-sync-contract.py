#!/usr/bin/env python3
"""Regression checks for draft identity and legacy Canvas delivery recovery."""
import contextlib
import json
import importlib.util
import os
from pathlib import Path
import sqlite3
import sys
import tempfile
import threading
import unittest
import uuid
from unittest.mock import patch

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
from codex_canvas import Canvas
from codex_sync import SyncStore
from codex_runtime import Runtime


def load_runtime_fixture():
    path = Path(__file__).with_name("runtime-contract.py")
    spec = importlib.util.spec_from_file_location("critical_runtime_fixture", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


runtime_fixture = load_runtime_fixture()


class DraftIdentityContract(unittest.TestCase):
    def test_payload_session_must_match_document_key(self):
        with tempfile.TemporaryDirectory(prefix="critical-sync-") as directory:
            path = Path(directory) / "state.sqlite3"

            @contextlib.contextmanager
            def connect():
                db = sqlite3.connect(path)
                try:
                    with db:
                        yield db
                finally:
                    db.close()

            store = SyncStore(connect, lambda: {}, lambda _key: {})
            original = {
                "id": "device-a:chat-a",
                "device": "device-a",
                "session": "chat-a",
                "text": "chat A",
                "updated": 1,
            }
            store.push_drafts([{
                "newDocumentState": {
                    "id": original["id"],
                    "payload": json.dumps(original),
                }
            }])
            assumed = store.pull("drafts")["documents"][0]
            forged = {
                **original,
                "session": "chat-b",
                "text": "chat B data under chat A key",
                "updated": 2,
            }
            with self.assertRaisesRegex(ValueError, "identity"):
                store.push_drafts([{
                    "newDocumentState": {
                        "id": original["id"],
                        "payload": json.dumps(forged),
                    },
                    "assumedMasterState": assumed,
                }])
            current = store.pull("drafts")["documents"]
            self.assertEqual(len(current), 1)
            self.assertEqual(json.loads(current[0]["payload"]), original)


class FakeRuntime:
    def __init__(self):
        self.lock = threading.RLock()
        self.receipts = {}
        self.send_calls = []
        self.receipt_error = None

    def snapshot(self):
        return {
            "agents": [{
                "id": "managed-agent",
                "name": "Managed agent",
                "kind": "agent",
                "source": "managed",
                "status": "running",
                "canSend": True,
                "launcherAlive": True,
            }]
        }

    def send(self, key, text, message_id):
        self.send_calls.append((key, text, message_id))
        self.receipts[message_id] = {
            "id": message_id,
            "status": "pending",
            "error": None,
        }
        return {"id": message_id, "status": "queued"}

    def delivery_receipt(self, message_id):
        if self.receipt_error:
            raise self.receipt_error
        return self.receipts.get(message_id)


class CanvasDeliveryContract(unittest.TestCase):
    def test_retry_reads_existing_runtime_receipt_without_resending(self):
        with tempfile.TemporaryDirectory(prefix="critical-canvas-") as directory:
            canvas = Canvas(Path(directory) / "state")
            runtime = FakeRuntime()
            canvas.runtime = runtime
            room = str(uuid.uuid4())
            canvas.create_chat("Managed team", ["managed-agent"], room)
            key = str(uuid.uuid4())
            delivery_id = key + ":managed-agent"
            runtime.receipts[delivery_id] = {
                "id": delivery_id,
                "status": "pending",
                "error": None,
            }
            with canvas.connect() as db:
                db.execute(
                    "INSERT INTO messages VALUES (?,?,?,?,?,?)",
                    (key, room, "user", "recover this", 1.0,
                     json.dumps({"managed-agent": "pending"})),
                )

            result = canvas.post(room, "recover this", key)

            self.assertEqual(result["deliveries"], {"managed-agent": "queued"})
            self.assertEqual(runtime.send_calls, [])
            saved = canvas.messages(room)[0]
            self.assertEqual(saved["deliveries"], {"managed-agent": "queued"})

    def test_receipt_read_failure_marks_unknown_without_sending(self):
        with tempfile.TemporaryDirectory(prefix="critical-canvas-") as directory:
            canvas = Canvas(Path(directory) / "state")
            runtime = FakeRuntime()
            canvas.runtime = runtime
            room = str(uuid.uuid4())
            canvas.create_chat("Managed team", ["managed-agent"], room)
            key = str(uuid.uuid4())
            with canvas.connect() as db:
                db.execute(
                    "INSERT INTO messages VALUES (?,?,?,?,?,?)",
                    (key, room, "user", "read receipt", 1.0,
                     json.dumps({"managed-agent": "pending"})),
                )
            runtime.receipt_error = OSError("receipt database unavailable")

            result = canvas.post(room, "read receipt", key)

            self.assertTrue(result["deliveries"]["managed-agent"].startswith("unknown:"))
            self.assertEqual(runtime.send_calls, [])

    def test_legacy_pending_replay_marks_unknown_without_subprocess(self):
        with tempfile.TemporaryDirectory(prefix="critical-canvas-") as directory:
            root = Path(directory) / "state"
            canvas = Canvas(root)
            (root / "codex-swarm-status.one.json").write_text(json.dumps([{
                "name": "worker",
                "threadId": "thread-one",
                "runId": "run-one",
                "turnStatus": "running",
                "goalStatus": "active",
                "launcherPid": os.getpid(),
            }]))
            target = canvas.threads()[0]["id"]
            key = str(uuid.uuid4())
            with canvas.connect() as db:
                db.execute(
                    "INSERT INTO messages VALUES (?,?,?,?,?,?)",
                    (key, target, "user", "legacy replay", 1.0,
                     json.dumps({target: "pending"})),
                )
            with patch("codex_canvas.subprocess.run") as command:
                result = canvas.post(target, "legacy replay", key)

            self.assertTrue(result["deliveries"][target].startswith("unknown:"))
            command.assert_not_called()


class RealRuntimeDeliveryContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="critical-runtime-")
        self.root = Path(self.temp.name)
        self.runtime = Runtime(self.root, runtime_fixture.FakeServer)
        self.canvas = Canvas(self.root)
        self.canvas.runtime = self.runtime
        self.agent = self.runtime.create({
            "name": "Managed lead",
            "cwd": str(self.root),
            "prompt": "Test delivery recovery",
        }, defer=True)

    def tearDown(self):
        self.runtime.close()
        self.temp.cleanup()

    def pending_message(self, key, text):
        with self.canvas.connect() as db:
            db.execute(
                "INSERT INTO messages VALUES (?,?,?,?,?,?)",
                (key, self.agent["id"], "user", text, 1.0,
                 json.dumps({self.agent["id"]: "pending"})),
            )

    def test_missing_event_queues_once_then_retries_by_receipt(self):
        key = str(uuid.uuid4())
        text = "queue exactly once"
        self.pending_message(key, text)
        with patch.object(self.runtime, "send", wraps=self.runtime.send) as send:
            first = self.canvas.post(self.agent["id"], text, key)
            with self.canvas.connect() as db:
                db.execute("UPDATE messages SET deliveries=? WHERE id=?",
                           (json.dumps({self.agent["id"]: "pending"}), key))
            second = self.canvas.post(self.agent["id"], text, key)

        self.assertEqual(send.call_count, 1)
        self.assertEqual(first["deliveries"], {self.agent["id"]: "queued"})
        self.assertEqual(second["deliveries"], {self.agent["id"]: "queued"})
        self.assertIn(
            self.runtime.delivery_receipt(key + ":" + self.agent["id"])["status"],
            {"pending", "reserved", "dispatching", "delivered"},
        )

    def test_existing_event_after_receipt_write_failure_is_not_resent(self):
        key = str(uuid.uuid4())
        text = "recover the receipt"
        delivery_id = key + ":" + self.agent["id"]
        self.runtime.send(self.agent["id"], text, delivery_id)
        self.pending_message(key, text)
        with patch.object(self.runtime, "send", side_effect=AssertionError("duplicate runtime send")):
            result = self.canvas.post(self.agent["id"], text, key)

        self.assertEqual(result["deliveries"], {self.agent["id"]: "queued"})
        self.assertEqual(self.runtime.delivery_receipt(delivery_id)["id"], delivery_id)

    def test_uncertain_event_is_unknown_and_never_resent(self):
        key = str(uuid.uuid4())
        text = "uncertain delivery"
        delivery_id = key + ":" + self.agent["id"]
        with self.runtime.db() as db:
            agent = self.runtime.agent(self.agent["id"], db)
            db.execute(
                "INSERT INTO runtime_events VALUES (?,?,?,?,?,?,?,?,?)",
                (delivery_id, agent["id"], "user", text, "uncertain", 1.0,
                 agent["epoch"], None, "native outcome unknown"),
            )
        self.pending_message(key, text)
        with patch.object(self.runtime, "send", side_effect=AssertionError("uncertain event replayed")):
            result = self.canvas.post(self.agent["id"], text, key)

        self.assertTrue(result["deliveries"][self.agent["id"]].startswith("unknown:"))
        self.assertEqual(self.runtime.delivery_receipt(delivery_id)["status"], "uncertain")


if __name__ == "__main__":
    unittest.main()
