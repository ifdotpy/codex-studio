#!/usr/bin/env python3
"""Capture is opt-in and redacts credentials and machine paths."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
from codex_provider_transcript import TranscriptCapture, redact


class ProviderCaptureContract(unittest.TestCase):
    def test_capture_is_off_without_both_opt_in_values(self):
        with tempfile.TemporaryDirectory() as temp, patch.dict(os.environ, {"HOME": temp}, clear=True):
            target = Path(temp) / "provider.jsonl"
            with patch.dict(os.environ, {"HOME": temp,
                                         "CODEX_AGENTS_PROVIDER_CAPTURE_FILE": str(target)}, clear=True):
                capture = TranscriptCapture("codex")
                capture.record("out", {"method": "initialize"})
            capture.record("out", {"method": "initialize"})
            self.assertIsNone(capture.path)
            self.assertEqual(list(Path(temp).iterdir()), [])

    def test_capture_records_timestamp_direction_and_redacted_frame(self):
        with tempfile.TemporaryDirectory() as temp:
            target = Path(temp) / "nested" / "provider.jsonl"
            env = {"CODEX_AGENTS_PROVIDER_CAPTURE": "1",
                   "CODEX_AGENTS_PROVIDER_CAPTURE_FILE": str(target)}
            with patch.dict(os.environ, {"HOME": temp, **env}, clear=True):
                capture = TranscriptCapture("claude")
                capture.record("in", {"method": "thread/read", "params": {
                    "apiKey": "secret-value", "path": "/Users/igor/My Project/file.txt",
                    "text": "Bearer abc.def and sk-abcdefghijklmnop /private/var/folders/abc/tmp.txt",
                    "tokenUsage": {"last": {"reasoningTokens": 12}}}})
            row = json.loads(target.read_text())
            self.assertEqual(row["provider"], "claude")
            self.assertEqual(row["direction"], "in")
            self.assertIsInstance(row["timestamp"], (int, float))
            encoded = json.dumps(row)
            self.assertNotIn("secret-value", encoded)
            self.assertNotIn("/Users/igor", encoded)
            self.assertNotIn("/private/var", encoded)
            self.assertNotIn("abcdefghijklmnop", encoded)
            self.assertIn("<REDACTED>", encoded)
            self.assertIn("<PATH>", encoded)
            self.assertIn('"reasoningTokens": 12', encoded)

    def test_nested_sensitive_keys_and_bearer_values_are_redacted(self):
        value = redact({"nested": [{"access_token": "value"}],
                        "message": "Bearer token-value access_token=another-secret",
                        "jwt": "eyJabcdefghijk.eyJabcdefghijk.signaturevalue123"})
        self.assertEqual(value["nested"][0]["access_token"], "<REDACTED>")
        self.assertNotIn("token-value", value["message"])
        self.assertNotIn("another-secret", value["message"])
        self.assertNotIn("signaturevalue123", value["jwt"])


if __name__ == "__main__":
    unittest.main()
