#!/usr/bin/env python3
"""Project admission must guard every outbound voice request."""

import importlib.util
from pathlib import Path
import unittest


HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location(
    "account_project_runtime_fixture", HERE / "account-project-runtime.py"
)
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class CriticalVoiceContract(unittest.TestCase):
    setUp = fixture.ProjectRuntime.setUp
    tearDown = fixture.ProjectRuntime.tearDown

    def lead(self, **data):
        return self.runtime.create(
            {"name": "Voice lead", "cwd": str(self.root), "prompt": "", **data},
            draft=True,
        )

    def set_rules(self, allowed):
        previous = self.runtime.accounts.get("default")["projectRules"]
        self.runtime.accounts.set_project_rules(
            "default", allowed, previous["revision"]
        )

    def test_denied_start_makes_no_realtime_request(self):
        self.set_rules([])
        lead = self.lead()
        voice = self.runtime.voice()
        calls = []
        voice._openai = lambda endpoint, body, *args: calls.append(endpoint) or b"answer"

        with self.assertRaisesRegex(ValueError, "cannot use project"):
            voice.start(lead["id"], "denied-session", "v=0\nfixture")

        self.assertEqual(calls, [])
        with self.runtime.db() as db:
            self.assertIsNone(
                db.execute(
                    "SELECT 1 FROM voice_sessions WHERE id=?", ("denied-session",)
                ).fetchone()
            )

    def test_denied_speech_makes_no_tts_request(self):
        lead = self.lead()
        voice = self.runtime.voice()
        voice._openai = lambda endpoint, body, *args: b"answer"
        voice.start(lead["id"], "speech-session", "v=0\nfixture")
        spoken = voice.speak(lead["id"], "Exact speech", "speech-request")
        self.set_rules([])
        calls = []
        voice._openai = lambda endpoint, body, *args: calls.append(endpoint) or b"audio"

        with self.assertRaisesRegex(ValueError, "cannot use project"):
            voice.speech(lead["id"], spoken["record"]["id"], "speech-session")

        self.assertEqual(calls, [])

    def test_allowed_project_and_explicit_skip_can_use_voice(self):
        lead = self.lead()
        voice = self.runtime.voice()
        calls = []
        voice._openai = lambda endpoint, body, *args: calls.append(endpoint) or b"answer"
        voice.start(lead["id"], "allowed-session", "v=0\nfixture")
        self.assertEqual(calls, ["realtime/calls"])

        self.set_rules([])
        skipped = self.lead(dangerously_skip_rules=True)
        voice.start(skipped["id"], "skipped-session", "v=0\nfixture")
        self.assertEqual(calls, ["realtime/calls", "realtime/calls"])


if __name__ == "__main__":
    unittest.main(verbosity=2)
