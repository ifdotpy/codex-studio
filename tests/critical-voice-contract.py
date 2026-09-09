#!/usr/bin/env python3
"""Native voice retains account ownership and does not support API-key fallback."""
import importlib.util
from pathlib import Path
import unittest
spec = importlib.util.spec_from_file_location("native_voice_fixture", Path(__file__).with_name("native-voice-contract.py"))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class CriticalVoiceContract(unittest.TestCase):
    setUp = fixture.NativeVoiceContract.setUp
    tearDown = fixture.NativeVoiceContract.tearDown
    until = fixture.NativeVoiceContract.until
    start = fixture.NativeVoiceContract.start
    emit = fixture.NativeVoiceContract.emit
    ready = fixture.NativeVoiceContract.ready
    test_selected_account = fixture.NativeVoiceContract.test_native_v3_uses_selected_account_and_no_custom_courier
    test_speech_account_boundary = fixture.NativeVoiceContract.test_native_speech_has_receipts_and_no_fake_playback
    test_no_api_key_fallback = fixture.NativeVoiceContract.test_api_key_account_is_rejected_without_start


if __name__ == "__main__":
    unittest.main(verbosity=2)
