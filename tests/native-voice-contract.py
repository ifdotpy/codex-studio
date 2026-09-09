#!/usr/bin/env python3
"""Deterministic native voice receipts, cancellation and account boundaries."""
import concurrent.futures
from contextlib import contextmanager
import sqlite3
import sys
import tempfile
import threading
import time
import unittest
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
from codex_voice import VoiceStore


class Server:
    def __init__(self):
        self.calls = []
        self.submissions = []
        self.auth = "chatgpt"
        self.jobs = []
        self.read_gate = None

    def call(self, method, params, timeout=60):
        self.calls.append((method, params))
        if method == "account/read":
            if self.read_gate:
                self.read_gate.wait(2)
            return {"account": {"type": self.auth}}
        if method == "thread/read":
            return {"thread": {"status": {"type": "idle"}}}
        if method == "thread/backgroundTerminals/list":
            return {"data": self.jobs}
        return {}

    def submit(self, method, params):
        future = concurrent.futures.Future()
        operation = (len(self.submissions), method, future)
        self.submissions.append((method, params, future))
        return operation

    def on_result(self, submitted, callback):
        submitted[2].add_done_callback(callback)


class Runtime:
    def __init__(self, root):
        self.root = Path(root)
        self.lock = threading.RLock()
        self.server = Server()
        self.connection_ids = {"chosen": "process-one"}
        self.loaded = {"lead"}
        self.prepare_locks = {}
        self.prepared = 0
        self.actors = {"lead": {"id": "lead", "isLead": True, "accountKey": "chosen", "threadId": "thread", "status": "complete", "epoch": 2, "autoWake": True},
                       "other": {"id": "other", "isLead": True, "accountKey": "other"}}
        with self.db() as db:
            db.execute("CREATE TABLE runtime_events(id TEXT PRIMARY KEY)")

    @contextmanager
    def db(self):
        db = sqlite3.connect(self.root / "voice.sqlite3", timeout=5)
        db.row_factory = sqlite3.Row
        try:
            with db:
                yield db
        finally:
            db.close()

    def agent(self, agent, db=None):
        return self.actors[agent].copy()

    def put(self, db, table, actor):
        self.actors[actor["id"]] = actor

    def connect(self, account):
        assert account == "chosen", account
        return self.server

    def connection_current(self, account, connection):
        return self.connection_ids.get(account) == connection

    def prepare(self, actor):
        self.prepared += 1

    prepare_locked = prepare


class NativeVoiceContract(unittest.TestCase):
    def setUp(self):
        self.folder = tempfile.TemporaryDirectory()
        self.runtime = Runtime(self.folder.name)
        self.voice = VoiceStore(self.runtime)
        self.server = self.runtime.server

    def tearDown(self):
        self.folder.cleanup()

    def until(self, condition):
        deadline = time.monotonic() + 3
        while not condition():
            if time.monotonic() > deadline:
                self.fail("Fixture deadline")
            time.sleep(.005)

    def start(self, sid="session"):
        self.voice.start("lead", sid, "v=0\noffer")
        self.until(lambda: bool(self.server.submissions))
        return self.server.submissions[-1]

    def emit(self, name, **params):
        return self.voice.native_notification({"method": "thread/realtime/" + name, "params": {"threadId": "thread", **params}}, "chosen", "process-one")

    def ready(self):
        operation = self.start()
        operation[2].set_result({})
        self.emit("sdp", sdp="answer")

    def test_native_v3_uses_selected_account_and_no_custom_courier(self):
        method, params, future = self.start()
        self.assertEqual(method, "thread/realtime/start")
        self.assertEqual(params["transport"], {"type": "webrtc", "sdp": "v=0\noffer"})
        self.assertEqual(params["version"], "v3")
        self.assertFalse(params["clientManagedHandoffs"])
        self.assertFalse(params["flushTranscriptTailOnSessionEnd"])
        self.assertNotIn("prompt", params)
        self.assertEqual(self.server.calls[0], ("account/read", {"refreshToken": False}))
        future.set_result({})
        self.assertEqual(self.voice.session("lead", "session")["state"], "connecting")
        self.emit("sdp", sdp="answer")
        self.assertEqual(self.voice.session("lead", "session")["sdp"], "answer")

    def test_start_receipt_and_original_offer_survive_retry(self):
        self.ready()
        self.assertEqual(self.voice.start("lead", "session", "v=0\noffer")["sdp"], "answer")
        self.assertEqual(len(self.server.submissions), 1)
        with self.assertRaises(ValueError): self.voice.start("lead", "session", "v=0\nchanged")
        with self.assertRaises(ValueError): self.voice.start("other", "session", "v=0\noffer")
        with self.assertRaises(ValueError): self.voice.start("lead", "second", "v=0\noffer")

    def test_stop_before_start_prevents_late_request(self):
        self.voice.end("lead", "delayed")
        self.assertEqual(self.voice.start("lead", "delayed", "v=0\noffer")["state"], "ended")
        self.assertEqual(self.server.submissions, [])

    def test_stop_during_prepare_never_starts_voice(self):
        self.server.read_gate = threading.Event()
        self.voice.start("lead", "session", "v=0\noffer")
        self.voice.end("lead", "session")
        self.server.read_gate.set()
        self.until(lambda: self.runtime.prepared > 0)
        self.assertEqual(self.server.submissions, [])

    def test_stop_keeps_thread_and_receipt_until_closed_event(self):
        self.ready()
        self.voice.end("lead", "session")
        self.voice.end("lead", "session")
        self.assertEqual([r[0] for r in self.server.submissions], ["thread/realtime/start", "thread/realtime/stop"])
        self.server.submissions[-1][2].set_result({})
        self.assertEqual(self.voice.session("lead", "session")["state"], "stopping")
        self.emit("closed", reason="requested")
        self.assertEqual(self.voice.session("lead", "session")["state"], "ended")
        self.assertEqual(self.runtime.actors["lead"]["status"], "complete")

    def test_canonical_transcripts_dedupe_and_cannot_resend(self):
        self.ready()
        item = {"id": "utterance", "type": "transcriptSegment", "realtimeSessionId": "session", "role": "user", "text": "Exact speech"}
        self.emit("item/completed", item=item)
        self.emit("item/completed", item=item)
        self.assertEqual(len(self.voice.records("lead")["records"]), 1)
        self.emit("item/completed", item={**item, "realtimeSessionId": "stale"})
        self.assertEqual(len(self.voice.records("lead")["records"]), 1)
        with self.assertRaisesRegex(ValueError, "already handles delegation"):
            self.voice.submit("lead", "duplicate", ["native:session:utterance"])
        self.voice.end("lead", "session")
        self.emit("item/completed", item={**item, "id": "tail", "role": "assistant", "text": "Final saved words"})
        self.assertEqual(len(self.voice.records("lead")["records"]), 2)

    def test_v3_transcript_events_save_text_and_disconnect_tail(self):
        self.ready()
        self.emit("transcript/delta", role="user", delta="Please inspect ")
        self.emit("transcript/delta", role="user", delta="this directory")
        self.emit("transcript/done", role="user", text="Please inspect this directory")
        self.emit("transcript/done", role="user", text="Please inspect this directory")
        self.emit("transcript/delta", role="assistant", delta="I will inspect it")
        self.voice.disconnected_native("chosen", "process-one")
        rows = self.voice.records("lead")["records"]
        self.assertEqual([r["text"] for r in rows], ["Please inspect this directory", "I will inspect it"])
        self.assertIn('"partial": true', rows[-1]["payload"])

    def test_stop_during_auth_cannot_resume_the_lead(self):
        self.server.read_gate = threading.Event()
        self.voice.start("lead", "session", "v=0\noffer")
        self.runtime.actors["lead"].update(epoch=3, autoWake=False)
        self.server.read_gate.set()
        self.until(lambda: self.voice.session("lead", "session")["state"] == "failed")
        self.assertFalse(self.runtime.actors["lead"]["autoWake"])
        self.assertEqual(self.server.submissions, [])

    def test_native_error_stops_voice_then_preserves_specific_reason(self):
        self.ready()
        self.emit("error", message="Account voice allowance reached")
        self.assertEqual(self.server.submissions[-1][0], "thread/realtime/stop")
        self.emit("closed", reason="requested")
        self.assertEqual(self.voice.session("lead", "session")["error"], "Account voice allowance reached")

    def test_deleted_chat_does_not_break_disconnect_cleanup(self):
        self.ready()
        self.emit("transcript/delta", role="user", delta="Unfinished speech")
        with self.runtime.db() as db:
            self.voice.delete_agent("lead", db)
        self.runtime.actors["lead"]["deletedAt"] = time.time()
        self.voice.end_active("lead")
        self.voice.disconnected_native("chosen", "process-one")
        with self.runtime.db() as db:
            self.assertEqual(db.execute("SELECT COUNT(*) FROM voice_records").fetchone()[0], 0)

    def test_wrong_connection_cannot_supply_answer(self):
        self.start()
        self.voice.native_notification({"method": "thread/realtime/sdp", "params": {"threadId": "thread", "sdp": "wrong"}}, "chosen", "old")
        self.assertIsNone(self.voice.session("lead", "session")["sdp"])
        self.voice.disconnected_native("chosen", "process-one")
        self.assertEqual(self.voice.session("lead", "session")["state"], "lost")
        self.runtime.connection_ids["chosen"] = "process-two"
        self.voice.end("lead", "session")
        self.assertEqual(len(self.server.submissions), 1)

    def test_native_speech_has_receipts_and_no_fake_playback(self):
        self.ready()
        spoken = self.voice.speak("lead", "A separate spoken update", "call-one", epoch=2)
        self.voice.speak("lead", "A separate spoken update", "call-one", epoch=2)
        self.assertEqual(spoken["playback"], "not confirmed")
        self.assertEqual(len(self.server.submissions), 2)
        self.assertEqual(self.server.submissions[-1][0:2], ("thread/realtime/appendSpeech", {"threadId": "thread", "text": "A separate spoken update"}))
        self.server.submissions[-1][2].set_result({})
        with self.assertRaises(ValueError): self.voice.speech("other", spoken["record"]["id"], "session")
        self.runtime.actors["lead"]["accountKey"] = "other"
        with self.assertRaisesRegex(ValueError, "account connection changed"):
            self.voice.speech("lead", spoken["record"]["id"], "session", "new-explicit-speech")

    def test_api_key_account_is_rejected_without_start(self):
        self.server.auth = "apiKey"
        self.voice.start("lead", "session", "v=0\noffer")
        self.until(lambda: self.voice.session("lead", "session")["state"] == "failed")
        self.assertEqual(self.server.submissions, [])

    def test_enable_existing_idle_thread_only_after_definitive_rejection(self):
        operation = self.start()
        operation[2].set_exception(ValueError("thread does not support realtime conversation"))
        self.until(lambda: len(self.server.submissions) == 2)
        self.assertIn("thread/unsubscribe", [c[0] for c in self.server.calls])
        self.assertEqual(self.runtime.prepared, 2)

    def test_feature_reload_preserves_background_commands(self):
        self.server.jobs = [{"id": "live-build"}]
        operation = self.start()
        operation[2].set_exception(ValueError("thread does not support realtime conversation"))
        self.until(lambda: self.voice.session("lead", "session")["state"] == "failed")
        self.assertNotIn("thread/unsubscribe", [c[0] for c in self.server.calls])
        self.assertEqual(len(self.server.submissions), 1)

    def test_restart_marks_pending_session_lost_without_replay(self):
        self.ready()
        recovered = VoiceStore(self.runtime)
        self.assertEqual(recovered.session("lead", "session")["state"], "lost")
        self.assertEqual(len(self.server.submissions), 1)


if __name__ == "__main__":
    unittest.main(verbosity=2)
