"""Persistent crypto lifecycle and total operation deadline."""
from __future__ import annotations

import base64
import subprocess
import sys
import threading
import time
import unittest
from unittest.mock import patch

from codex_multi_server_crypto import CryptoProcess


class PersistentCryptoTests(unittest.TestCase):
    def test_process_reuse_real_signatures_and_restart(self) -> None:
        helper = CryptoProcess()
        self.addCleanup(helper.close)
        pair = helper.call("generate")
        self.assertIsNotNone(helper.process)
        original = helper.process
        data = base64.b64encode("A signed request, café".encode()).decode()
        signature = helper.call("sign", privateKey=pair["privateKey"], data=data)["signature"]
        self.assertTrue(helper.call("verify", publicKey=pair["publicKey"], signature=signature, data=data)["valid"])
        self.assertIs(helper.process, original)
        self.assertFalse(helper.call("verify", publicKey=pair["publicKey"], signature=signature,
                                     data=base64.b64encode(b"different").decode())["valid"])
        assert original is not None
        original.kill()
        original.wait(timeout=2)
        self.assertTrue(helper.call("verify", publicKey=pair["publicKey"], signature=signature, data=data)["valid"])
        self.assertIsNot(helper.process, original)
        helper.close()
        self.assertIsNone(helper.process)
        helper.shutdown()
        with self.assertRaises(RuntimeError):
            helper.call("generate")
        self.assertIsNone(helper.process)

    def test_blocked_helper_has_a_total_deadline_and_can_restart(self) -> None:
        helper = CryptoProcess()
        self.addCleanup(helper.close)
        native_popen = subprocess.Popen

        def stalled(*args: object, **kwargs: object) -> subprocess.Popen[bytes]:
            return native_popen([sys.executable, "-c", "import time; time.sleep(30)"],
                                stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)

        begin = time.monotonic()
        with patch("codex_multi_server_crypto.subprocess.Popen", side_effect=stalled):
            with self.assertRaises(RuntimeError):
                helper.call("generate", timeout=0.05)
        self.assertLess(time.monotonic() - begin, 1)
        self.assertIsNone(helper.process)
        self.assertIn("publicKey", helper.call("generate"))

    def test_deadline_includes_the_crypto_queue(self) -> None:
        helper = CryptoProcess()
        self.addCleanup(helper.close)
        errors: list[Exception] = []

        def waiting() -> None:
            try:
                helper.call("generate", timeout=0.05)
            except RuntimeError as error:
                errors.append(error)

        with helper.lock:
            worker = threading.Thread(target=waiting)
            worker.start()
            worker.join(timeout=0.5)
            self.assertFalse(worker.is_alive())
        self.assertEqual(len(errors), 1)
        self.assertIsNone(helper.process)


if __name__ == "__main__":
    unittest.main()
