#!/usr/bin/env python3
"""Prove the legacy voice validation error precedes delivery reservation."""
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
        self.sends = []

    def db(self):
        db = sqlite3.connect(self.root / "voice.sqlite3")
        db.row_factory = sqlite3.Row
        return db

    def agent(self, agent):
        return {"isLead": True}

    def send(self, agent, text, message_id, delivery):
        self.sends.append((message_id, text))
        return {"id": message_id}


with tempfile.TemporaryDirectory() as root:
    runtime = Runtime(root)
    voice = VoiceStore(runtime)
    with runtime.db() as db:
        db.execute("INSERT INTO voice_sessions(id,agent,created) VALUES('session','lead',0)")
    voice.record("lead", "session", "original", "user", "Original speech")
    for text in ["", "   ", "x" * 32001]:
        try:
            voice.submit("lead", "invalid-attempt", ["original"], edited_text=text)
        except ValueError as error:
            assert str(error) == "Edited transcript must contain 1 to 32000 characters"
        else:
            raise AssertionError("Invalid text was accepted")
        with runtime.db() as db:
            assert db.execute("SELECT COUNT(*) FROM voice_deliveries").fetchone()[0] == 0
        assert runtime.sends == []
    voice.submit("lead", "corrected-attempt", ["original"], edited_text="Corrected speech")
    assert runtime.sends == [("corrected-attempt", "Corrected speech")]
    with runtime.db() as db:
        assert db.execute("SELECT COUNT(*) FROM voice_deliveries").fetchone()[0] == 1
    # The special error is not a generic identity-conflict response.
    try:
        voice.submit("lead", "corrected-attempt", ["original"], edited_text="Different speech")
    except ValueError as error:
        assert str(error) != "Edited transcript must contain 1 to 32000 characters"
    else:
        raise AssertionError("Changed request body was accepted")
print("voice validation before reservation: PASS")
