#!/usr/bin/env python3
"""Verify app-server log rotation stays bounded and retains newest bytes."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
from codex_log_rotation import RotatingLog
from codex_runtime import AppServer


class LogRotationContract(unittest.TestCase):
    def test_rotates_live_writes_with_bounded_retention(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "app-server.log"
            writer = RotatingLog(path, max_bytes=8, backups=2)
            writer.write(b"abcdefghijklmnopqrstuv")
            writer.close()

            files = [path] + [path.with_name(path.name + f".{n}") for n in (1, 2)]
            self.assertTrue(all(file.stat().st_size <= 8 for file in files if file.exists()))
            retained = b"".join(file.read_bytes() for file in reversed(files) if file.exists())
            self.assertEqual(retained, b"abcdefghijklmnopqrstuv")

    def test_adopts_oversized_existing_log_by_moving_it_whole(self):
        # A rename keeps the old history; the backup window bounds it later.
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "app-server.log"
            path.write_bytes(b"0123456789")
            writer = RotatingLog(path, max_bytes=4, backups=1)
            self.assertEqual(path.read_bytes(), b"")
            self.assertEqual(path.with_name(path.name + ".1").read_bytes(), b"0123456789")
            writer.write(b"abcd")
            writer.close()
            self.assertEqual(path.read_bytes(), b"abcd")
            self.assertEqual(path.with_name(path.name + ".1").read_bytes(), b"0123456789")

    def test_app_server_stderr_is_drained_to_rotating_log(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            executable = root / "fake-app-server"
            executable.write_text(
                "#!" + sys.executable + "\n"
                "import json,sys\n"
                "sys.stderr.write('native diagnostic\\n'); sys.stderr.flush()\n"
                "for line in sys.stdin:\n"
                " message=json.loads(line)\n"
                " if message.get('method') == 'initialize':\n"
                "  print(json.dumps({'id':message['id'],'result':{}}),flush=True)\n"
                " elif message.get('method') == 'initialized': break\n"
            )
            executable.chmod(0o755)
            server = AppServer(root, lambda _: None, lambda _: None, lambda: None,
                               executable=str(executable))
            try:
                server.reader.join(timeout=3)
                self.assertFalse(server.reader.is_alive())
                if server.stderr_writer:
                    server.stderr_writer.join(timeout=3)
                self.assertIn("native diagnostic", (root / "app-server.log").read_text())
            finally:
                server.close()


if __name__ == "__main__":
    unittest.main()
