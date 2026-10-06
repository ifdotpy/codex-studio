"""Verify SIGTERM closes the real backend without server tracebacks."""
from test_isolation import isolate_supervisor_environment

isolate_supervisor_environment()

import os
from pathlib import Path
import select
import signal
import socket
import subprocess
import sys
import tempfile
import time
import unittest
import urllib.request
from urllib.parse import urlencode

ROOT = Path(__file__).resolve().parents[1]
SHUTDOWN_BOUND_SECONDS = 15


class BackendShutdownContract(unittest.TestCase):
    def run_shutdown(self, *, with_stream: bool, second_signal: bool = False) -> None:
        with tempfile.TemporaryDirectory(prefix="backend-shutdown-") as directory:
            root = Path(directory)
            env = os.environ.copy()
            for key, path in (
                ("CODEX_AGENTS_STATE_DIR", root / "state"),
                ("CODEX_HOME", root / "codex-home"),
                ("XDG_CACHE_HOME", root / "cache"),
                ("XDG_STATE_HOME", root / "xdg-state"),
                ("HOME", root / "home"),
            ):
                path.mkdir(parents=True, exist_ok=True)
                env[key] = str(path)
            env.update({
                "HTTPS_PROXY": "http://127.0.0.1:9",
                "HTTP_PROXY": "http://127.0.0.1:9",
                "ALL_PROXY": "http://127.0.0.1:9",
                "NO_PROXY": "127.0.0.1,localhost",
            })
            with socket.socket() as probe:
                probe.bind(("127.0.0.1", 0))
                port = probe.getsockname()[1]
            process = subprocess.Popen(
                [sys.executable, "-B", str(ROOT / "scripts/codex-canvas"),
                 "--port", str(port)],
                cwd=ROOT,
                env=env,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
            )
            stream = None
            try:
                deadline = time.monotonic() + 20
                output = ""
                while time.monotonic() < deadline:
                    ready, _, _ = select.select([process.stdout], [], [], 0.1)
                    if not ready:
                        if process.poll() is not None:
                            break
                        continue
                    line = process.stdout.readline()
                    output += line
                    if f"http://127.0.0.1:{port}" in line:
                        break
                    if process.poll() is not None:
                        break
                self.assertIn(f"http://127.0.0.1:{port}", output, output)
                if with_stream:
                    stream = urllib.request.urlopen(
                        f"http://127.0.0.1:{port}/api/sync/stream?"
                        + urlencode({"protocol": "3", "resources": '[{"kind":"drafts"}]'}),
                        timeout=10,
                    )
                    self.assertIn("text/event-stream", stream.headers.get("Content-Type", ""))
                    self.assertTrue(stream.readline(), "stream sends an initial event")
                started = time.monotonic()
                process.send_signal(signal.SIGTERM)
                if second_signal:
                    time.sleep(0.1)
                    process.send_signal(signal.SIGINT)
                try:
                    _stdout, stderr = process.communicate(timeout=SHUTDOWN_BOUND_SECONDS)
                except subprocess.TimeoutExpired:
                    process.kill()
                    _stdout, stderr = process.communicate(timeout=5)
                    self.fail(f"backend exceeded {SHUTDOWN_BOUND_SECONDS}s shutdown bound")
                self.assertLess(time.monotonic() - started, 5 if second_signal else SHUTDOWN_BOUND_SECONDS)
                if stream is not None:
                    # Consume queued initial events through the clean HTTP EOF.
                    stream.read()
                lines = stderr.splitlines()
                shutdown = next(
                    (index for index, line in enumerate(lines)
                     if '"event": "backend_shutdown"' in line),
                    None,
                )
                self.assertIsNotNone(shutdown, stderr)
                tail = "\n".join(lines[shutdown + 1:])
                self.assertNotRegex(tail, r"Traceback|Exception in")
                self.assertEqual(process.returncode, 0, stderr)
            finally:
                if stream is not None:
                    stream.close()
                if process.poll() is None:
                    process.kill()
                    process.wait(timeout=5)

    def test_sigterm_without_clients_is_clean(self):
        self.run_shutdown(with_stream=False)

    def test_sigterm_closes_sync_stream_cleanly(self):
        self.run_shutdown(with_stream=True)

    def test_second_signal_forces_bounded_stream_drain(self):
        self.run_shutdown(with_stream=True, second_signal=True)


if __name__ == "__main__":
    unittest.main()
