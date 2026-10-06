"""Exercise real backend shutdown with idle, unread, and active clients."""
from test_isolation import isolate_supervisor_environment

isolate_supervisor_environment()

import http.client
import json
import os
from pathlib import Path
import select
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
import unittest
import urllib.error
import urllib.request
from urllib.parse import urlencode

ROOT = Path(__file__).resolve().parents[1]
NORMAL_SHUTDOWN_BOUND_SECONDS = 1.5
FORCED_SHUTDOWN_BOUND_SECONDS = 3.0
PROCESS_CLEANUP_BOUND_SECONDS = 45


class BackendShutdownContract(unittest.TestCase):
    def start_backend(
        self,
        root: Path,
        port: int,
        *,
        handler_delay: float = 0,
        startup_delay: float = 0,
    ) -> tuple[subprocess.Popen[str], Path]:
        env = os.environ.copy()
        for key, leaf in (
            ("CODEX_AGENTS_STATE_DIR", "state"),
            ("CODEX_HOME", "codex-home"),
            ("XDG_CACHE_HOME", "cache"),
            ("XDG_STATE_HOME", "xdg-state"),
            ("HOME", "home"),
        ):
            path = root / leaf
            path.mkdir(parents=True, exist_ok=True)
            env[key] = str(path)
        env.update({
            "HTTPS_PROXY": "http://127.0.0.1:9",
            "HTTP_PROXY": "http://127.0.0.1:9",
            "ALL_PROXY": "http://127.0.0.1:9",
            "NO_PROXY": "127.0.0.1,localhost",
        })
        marker = root / "backend-test-marker"
        bootstrap = f"""
import runpy, sys, time
from pathlib import Path
sys.path.insert(0, {str(ROOT / 'scripts')!r})
if {handler_delay!r}:
    from codex_sync import SyncStore
    original_push_drafts = SyncStore.push_drafts
    def delayed_push_drafts(self, rows):
        Path({str(marker)!r}).write_text('handler entered')
        time.sleep({handler_delay!r})
        return original_push_drafts(self, rows)
    SyncStore.push_drafts = delayed_push_drafts
if {startup_delay!r}:
    from codex_runtime import Runtime
    original_runtime_init = Runtime.__init__
    def delayed_runtime_init(self, *args, **kwargs):
        Path({str(marker)!r}).write_text('runtime construction entered')
        time.sleep({startup_delay!r})
        original_runtime_init(self, *args, **kwargs)
    Runtime.__init__ = delayed_runtime_init
sys.argv = [{str(ROOT / 'scripts/codex-canvas')!r}, '--port', {str(port)!r}]
runpy.run_path(sys.argv[0], run_name='__main__')
"""
        command = (
            [sys.executable, "-B", "-c", bootstrap]
            if handler_delay or startup_delay
            else [sys.executable, "-B", str(ROOT / "scripts/codex-canvas"), "--port", str(port)]
        )
        process = subprocess.Popen(
            command,
            cwd=ROOT,
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        return process, marker

    @staticmethod
    def free_port() -> int:
        with socket.socket() as probe:
            probe.bind(("127.0.0.1", 0))
            return int(probe.getsockname()[1])

    def wait_for_start(self, process: subprocess.Popen[str], port: int) -> str:
        deadline = time.monotonic() + 20
        output = ""
        assert process.stdout is not None
        while time.monotonic() < deadline:
            ready, _, _ = select.select([process.stdout], [], [], 0.1)
            if not ready:
                if process.poll() is not None:
                    break
                continue
            line = process.stdout.readline()
            output += line
            if f"http://127.0.0.1:{port}" in line:
                return output
            if process.poll() is not None:
                break
        self.fail(f"backend did not announce its ready port: {output}")

    def open_streams(self, port: int, count: int, *, read_initial: bool = True):
        connections = []
        for _ in range(count):
            connection = http.client.HTTPConnection("127.0.0.1", port, timeout=10)
            connection.request(
                "GET",
                "/api/sync/stream?" + urlencode({
                    "protocol": "3",
                    "resources": '[{"kind":"drafts"}]',
                }),
            )
            response = connection.getresponse()
            self.assertEqual(response.status, 200)
            self.assertIn("text/event-stream", response.getheader("Content-Type", ""))
            if read_initial:
                self.assertTrue(response.readline(), "stream sends an initial event")
            connections.append((connection, response))
        return connections

    @staticmethod
    def wait_marker(marker: Path, process: subprocess.Popen[str], timeout: float = 20) -> None:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if marker.exists():
                return
            if process.poll() is not None:
                raise AssertionError(f"backend exited before reaching marker: {process.returncode}")
            time.sleep(0.01)
        raise AssertionError(f"backend did not reach marker: {marker}")

    def run_shutdown(
        self,
        *,
        stream_count: int = 0,
        unread_stream: bool = False,
        second_signal: bool = False,
        handler_delay: float = 0,
        startup_delay: float = 0,
        first_signal: int = signal.SIGTERM,
    ) -> tuple[float, int, str, str]:
        with tempfile.TemporaryDirectory(prefix="backend-shutdown-") as directory:
            root = Path(directory)
            port = self.free_port()
            process, marker = self.start_backend(
                root, port, handler_delay=handler_delay, startup_delay=startup_delay,
            )
            streams = []
            request_thread: threading.Thread | None = None
            request_errors: list[BaseException] = []
            initial_output = ""
            try:
                if startup_delay:
                    self.wait_marker(marker, process)
                else:
                    initial_output = self.wait_for_start(process, port)
                if stream_count:
                    streams = self.open_streams(
                        port, stream_count, read_initial=not unread_stream,
                    )
                if handler_delay:
                    with urllib.request.urlopen(f"http://127.0.0.1:{port}/api/session", timeout=5) as response:
                        token = json.load(response)["token"]
                    with urllib.request.urlopen(f"http://127.0.0.1:{port}/api/sync/identity", timeout=5) as response:
                        workspace_id = json.load(response)["workspaceId"]
                    payload = json.dumps({"rows": [{
                        "newDocumentState": {
                            "id": "shutdown-test:chat",
                            "payload": json.dumps({"device": "shutdown-test", "session": "chat", "text": "x"}),
                        },
                    }]}).encode()
                    request = urllib.request.Request(
                        f"http://127.0.0.1:{port}/api/sync/drafts",
                        data=payload,
                        headers={
                            "Content-Type": "application/json",
                            "X-Canvas-Token": token,
                            "X-Canvas-Workspace": workspace_id,
                        },
                    )

                    def post_slow_request() -> None:
                        try:
                            with urllib.request.urlopen(request, timeout=handler_delay + 10) as response:
                                response.read()
                        except (OSError, urllib.error.URLError) as error:
                            request_errors.append(error)

                    request_thread = threading.Thread(target=post_slow_request, daemon=True)
                    request_thread.start()
                    self.wait_marker(marker, process)
                started = time.monotonic()
                process.send_signal(first_signal)
                if second_signal:
                    time.sleep(0.1)
                    process.send_signal(first_signal)
                if stream_count and not unread_stream and not second_signal and not handler_delay:
                    for _connection, response in streams:
                        self.assertIn(
                            b"data:", response.read(),
                            "shutdown drains stream events through a clean HTTP EOF",
                        )
                timeout = (
                    FORCED_SHUTDOWN_BOUND_SECONDS
                    if second_signal
                    else max(PROCESS_CLEANUP_BOUND_SECONDS, handler_delay + 10)
                )
                try:
                    stdout, stderr = process.communicate(timeout=timeout)
                except subprocess.TimeoutExpired:
                    process.kill()
                    stdout, stderr = process.communicate(timeout=5)
                    self.fail(f"backend did not shut down within {timeout}s; stderr={stderr}")
                elapsed = time.monotonic() - started
                combined_stdout = initial_output + (stdout or "")
                if startup_delay:
                    self.assertNotIn("Codex Canvas: http://", combined_stdout)
                    self.assertIn('"event": "backend_shutdown"', stderr)
                lines = stderr.splitlines()
                shutdown = next(
                    (index for index, line in enumerate(lines)
                     if '"event": "backend_shutdown"' in line),
                    None,
                )
                self.assertIsNotNone(shutdown, stderr)
                tail = "\n".join(lines[shutdown + 1:])
                self.assertNotRegex(tail, r"Traceback|Exception in")
                if not handler_delay and not second_signal and not startup_delay:
                    self.assertNotRegex(stderr, r"Cancel \d+ running task\(s\)")
                    self.assertLess(elapsed, NORMAL_SHUTDOWN_BOUND_SECONDS)
                if second_signal:
                    self.assertLess(elapsed, FORCED_SHUTDOWN_BOUND_SECONDS)
                self.assertEqual(process.returncode, 0, stderr)
                print(json.dumps({
                    "case": {
                        "streams": stream_count,
                        "unread": unread_stream,
                        "handlerSeconds": handler_delay,
                        "startupSeconds": startup_delay,
                        "firstSignal": signal.Signals(first_signal).name,
                        "secondSignal": second_signal,
                    },
                    "exitCode": process.returncode,
                    "elapsedSeconds": round(elapsed, 3),
                    "stderr": stderr.splitlines()[-3:],
                }), flush=True)
                if request_thread is not None:
                    request_thread.join(timeout=1)
                return elapsed, process.returncode, combined_stdout, stderr
            finally:
                for connection, response in streams:
                    response.close()
                    connection.close()
                if process.poll() is None:
                    process.kill()
                    process.wait(timeout=5)

    def test_sigterm_without_clients_is_clean_and_fast(self):
        self.run_shutdown()

    def test_sigint_without_clients_is_clean_and_fast(self):
        self.run_shutdown(first_signal=signal.SIGINT)

    def test_sigterm_closes_one_idle_sync_stream_immediately(self):
        self.run_shutdown(stream_count=1)

    def test_sigterm_closes_five_idle_sync_streams_immediately(self):
        self.run_shutdown(stream_count=5)

    def test_sigterm_closes_a_stream_even_when_the_client_does_not_read(self):
        self.run_shutdown(stream_count=1, unread_stream=True)

    def test_second_signal_forces_bounded_stream_drain(self):
        self.run_shutdown(stream_count=1, second_signal=True)

    def test_second_sigterm_forces_bounded_two_stream_drain(self):
        self.run_shutdown(stream_count=2, second_signal=True)

    def test_second_sigint_forces_bounded_stream_drain(self):
        self.run_shutdown(
            stream_count=1, second_signal=True, first_signal=signal.SIGINT,
        )

    def test_second_signal_forces_five_second_handler_to_exit(self):
        self.run_shutdown(handler_delay=5, second_signal=True)

    def test_second_signal_forces_thirty_second_handler_to_exit(self):
        self.run_shutdown(handler_delay=30, second_signal=True)

    def test_single_signal_allows_five_second_handler_to_finish(self):
        elapsed, _, _, _ = self.run_shutdown(handler_delay=5)
        self.assertGreaterEqual(elapsed, 4.5)

    def test_single_signal_bounds_thirty_second_handler_after_completion(self):
        elapsed, _, _, _ = self.run_shutdown(handler_delay=30)
        self.assertGreaterEqual(elapsed, 29)

    def test_second_signal_during_startup_does_not_announce_url(self):
        self.run_shutdown(startup_delay=3, second_signal=True)


if __name__ == "__main__":
    unittest.main()
