#!/usr/bin/env python3
"""No network or paid model calls. Exercise voice persistence and authority."""
import json
import sqlite3
import sys
import tempfile
import threading
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
from codex_voice import VoiceStore


class Runtime:
    def __init__(self, root):
        self.root = Path(root)
        self.lock = threading.RLock()
        self.actors = {"lead": {"role": "orchestrator", "isLead": True, "epoch": 1, "autoWake": True}, "other": {"role": "orchestrator", "isLead": True, "epoch": 1, "autoWake": True}, "worker": {"role": "worker"}}
        self.sends = []
        with self.db() as db:
            db.executescript("CREATE TABLE runtime_events(id TEXT PRIMARY KEY); CREATE TABLE runtime_requests(id TEXT PRIMARY KEY,record TEXT);")
    def db(self):
        db = sqlite3.connect(self.root / "test.sqlite3")
        db.row_factory = sqlite3.Row
        return db
    def agent(self, key):
        return self.actors[key]
    def check_account_project(self, row):
        return None
    def send(self, agent, text, message_id, delivery):
        with self.db() as db:
            if not db.execute("SELECT 1 FROM runtime_events WHERE id=?", (message_id,)).fetchone():
                self.sends.append((agent,text,message_id))
                db.execute("INSERT INTO runtime_events VALUES(?)", (message_id,))
        return {"id": message_id}


def rejects(fn):
    try:
        fn()
    except ValueError:
        return
    raise AssertionError("Expected rejection")


with tempfile.TemporaryDirectory() as root:
    runtime = Runtime(root)
    voice = VoiceStore(runtime)
    with runtime.db() as db:
        db.execute("INSERT INTO voice_sessions(id,agent,created) VALUES('session','lead',0)")
    args = ("lead","session","one","user","Exact path: /a/b, budget 37")
    first = voice.record(*args)
    assert voice.record(*args)["seq"] == first["seq"]
    rejects(lambda: voice.record("lead","session","one","user","changed"))
    rejects(lambda: voice.record("other","session","two","user","cross-chat"))
    rejects(lambda: voice.record("lead","session","three","orchestrator","spoof"))
    rejects(lambda: voice.status("worker"))
    voice.record("lead","session","four","courier","Did you say 37?")
    voice.submit("lead","send-one",["one","four"])
    voice.submit("lead","send-one",["one","four"])
    assert len(runtime.sends) == 1
    assert "User: Exact path: /a/b, budget 37\n\nVoice courier: Did you say 37?" in runtime.sends[0][1]
    rejects(lambda: voice.submit("lead","send-one",["four","one"]))
    rejects(lambda: voice.submit("lead","send-two",["one"]))
    rejects(lambda: voice.submit("other","cross",["one"]))
    voice.record("lead","session","edit-original","user","Wrong path")
    voice.submit("lead","edited-send",["edit-original"],edited_text="User: Correct path")
    voice.submit("lead","edited-send",["edit-original"],edited_text="User: Correct path")
    rejects(lambda: voice.submit("lead","edited-send",["edit-original"],edited_text="User: Other path"))
    assert runtime.sends[-1][1] == "User: Correct path"
    assert set(voice.records("lead")["delivered"]) == {"one","four","edit-original"}
    spoken = voice.speak("lead","Read these exact words.","tool-one",epoch=1)
    assert voice.speak("lead","Read these exact words.","tool-one",epoch=1)["record"]["seq"] == spoken["record"]["seq"]
    rejects(lambda: voice.speak("lead","Changed words.","tool-one",epoch=1))
    rejects(lambda: voice.speak("lead","stale", "tool-two",epoch=0))
    rejects(lambda: voice.speech("lead",spoken["record"]["id"],"session"))
    voice.end("lead","session")
    rejects(lambda: voice.speech("lead",spoken["record"]["id"],"session"))
    assert VoiceStore(runtime).records("lead")["records"][-1]["id"] == spoken["record"]["id"]
print("voice contract: PASS")

# Real runtime identity and send path. The app-server is a deterministic fixture.
import importlib.util
spec = importlib.util.spec_from_file_location("voice_runtime_fixture", Path(__file__).with_name("runtime-contract.py"))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
from codex_runtime import Runtime as ActualRuntime
class QuietRuntime(ActualRuntime):
    def schedule(self):
        while not self.closed:
            self.changed.wait(.1)
            self.changed.clear()
with tempfile.TemporaryDirectory() as root:
    actual = QuietRuntime(root, fixture.FakeServer)
    try:
        lead = actual.create({"name": "Voice lead", "cwd": root, "prompt": "Wait for a message"}, defer=True)
        assert lead["isLead"] and lead["role"] == "orchestrator"
        voice = VoiceStore(actual)
        voice.status(lead["id"])
        with actual.db() as db:
            db.execute("INSERT INTO voice_sessions(id,agent,created) VALUES('real-session',?,0)", (lead["id"],))
        voice.record(lead["id"],"real-session","real-transcript","user","Inspect the exact path /tmp/example")
        voice.submit(lead["id"],"real-message",["real-transcript"])
        voice.submit(lead["id"],"real-message",["real-transcript"])
        with actual.db() as db:
            assert db.execute("SELECT COUNT(*) FROM runtime_events WHERE id='real-message'").fetchone()[0] == 1
            row = db.execute("SELECT text FROM runtime_events WHERE id='real-message'").fetchone()
            assert "Inspect the exact path /tmp/example" in row[0]
        rejects(lambda: voice.speak(lead["id"],"stale", "stale-tool",epoch=lead["epoch"]+1))
        with actual.lock, actual.db() as db:
            lead = actual.agent(lead["id"], db)
            lead.update(autoWake=True, threadId="voice-thread", turnId="voice-turn")
            actual.put(db,"agents",lead)
        request = {"id": "voice-permission", "method": "item/commandExecution/requestApproval", "agent": lead["id"], "epoch": lead["epoch"], "status": "pending", "rpcId": 90, "params": {"command": "echo test"}}
        with actual.db() as db:
            actual.put(db,"requests",request)
        permission = voice.approval_speech(lead["id"], request["id"])
        voice.record(lead["id"],"real-session","approval-too-early","user","разрешаю")
        rejects(lambda: voice.approve(lead["id"],"real-session",permission["id"],"approval-too-early"))
        voice.record(lead["id"],"real-session","permission-played","playback","played",payload={"record_id": permission["id"]})
        voice.record(lead["id"],"real-session","approval-yes","user","разрешаю")
        result = voice.approve(lead["id"],"real-session",permission["id"],"approval-yes")
        assert voice.approve(lead["id"],"real-session",permission["id"],"approval-yes") == result
        call = {"id": 77, "params": {"tool": "orchestration_speak", "threadId": "voice-thread", "turnId": "voice-turn", "callId": "voice-call", "arguments": {"text": "Exact orchestrator speech"}}}
        actual.dynamic(call)
        actual.dynamic(call)
        with actual.db() as db:
            assert db.execute("SELECT COUNT(*) FROM voice_records WHERE kind='orchestrator' AND id LIKE 'speak:%'").fetchone()[0] == 1
            receipt = json.loads(db.execute("SELECT result FROM runtime_tool_results WHERE id='voice-thread:voice-call'").fetchone()[0])
            assert receipt["success"], receipt

    finally:
        actual.close()
print("real runtime voice contract: PASS")
