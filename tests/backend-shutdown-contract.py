"""Exercise real backend shutdown with idle, unread, and active clients."""
from test_isolation import isolate_supervisor_environment

isolate_supervisor_environment()

import http.client
import json
import os
from pathlib import Path
import select
import signal
import sqlite3
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
SECOND_SIGNAL_BOUND_SECONDS = 6.5
PROCESS_CLEANUP_BOUND_SECONDS = 45


class BackendShutdownContract(unittest.TestCase):
    def start_backend(
        self,
        root: Path,
        port: int,
        *,
        handler_delay: float = 0,
        startup_delay: float = 0,
        updates_delay: float = 0,
        cleanup_delay: float = 0,
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
import sys, time
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
if {updates_delay!r}:
    import codex_live_updates
    original_start_updates = codex_live_updates.start
    def delayed_start_updates(runtime):
        Path({str(marker)!r}).write_text('updates startup entered')
        time.sleep({updates_delay!r})
        return original_start_updates(runtime)
    codex_live_updates.start = delayed_start_updates
from codex_runtime import Runtime
original_runtime_close = Runtime.close
def fixture_runtime_close(self):
    with self.lock, self.db() as db:
        db.execute(
            'INSERT OR REPLACE INTO runtime_agents(id, record) VALUES (?, ?)',
            ('shutdown-test-agent', __import__('json').dumps({{
                'id': 'shutdown-test-agent', 'epoch': 1, 'accountKey': 'default',
                'threadId': 'shutdown-test-thread', 'turnId': 'shutdown-test-turn',
                'autoWake': True, 'status': 'running', 'inFlight': True,
            }})),
        )
    if {cleanup_delay!r}:
        Path({str(marker)!r}).write_text('runtime cleanup entered')
        time.sleep({cleanup_delay!r})
    return original_runtime_close(self)
Runtime.close = fixture_runtime_close
sys.argv = [{str(ROOT / 'scripts/codex-canvas')!r}, '--port', {str(port)!r}]
from codex_canvas import main
main()
"""
        command = [sys.executable, "-B", "-c", bootstrap]
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
        second_signal_delay: float | None = None,
        second_signal_number: int | None = None,
        handler_delay: float = 0,
        startup_delay: float = 0,
        updates_delay: float = 0,
        cleanup_delay: float = 0,
        first_signal: int = signal.SIGTERM,
    ) -> tuple[float, int, str, str]:
        with tempfile.TemporaryDirectory(prefix="backend-shutdown-") as directory:
            root = Path(directory)
            port = self.free_port()
            process, marker = self.start_backend(
                root, port, handler_delay=handler_delay, startup_delay=startup_delay,
                updates_delay=updates_delay,
                cleanup_delay=cleanup_delay,
            )
            streams = []
            request_thread: threading.Thread | None = None
            request_errors: list[BaseException] = []
            initial_output = ""
            try:
                if startup_delay or updates_delay:
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
                second_started = None
                if second_signal_delay is not None:
                    time.sleep(second_signal_delay)
                    if process.poll() is None:
                        second_started = time.monotonic()
                        process.send_signal(second_signal_number or first_signal)
                if stream_count and not unread_stream and second_signal_delay is None and not handler_delay:
                    for _connection, response in streams:
                        self.assertIn(
                            b"data:", response.read(),
                            "shutdown drains stream events through a clean HTTP EOF",
                        )
                timeout = (
                    SECOND_SIGNAL_BOUND_SECONDS + (second_signal_delay or 0)
                    if second_signal_delay is not None
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
                if startup_delay or updates_delay:
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
                self.assertNotRegex(stderr, r"Traceback|Exception in")
                if not handler_delay and second_signal_delay is None and not startup_delay and not updates_delay:
                    self.assertNotRegex(stderr, r"Cancel \d+ running task\(s\)")
                    self.assertLess(elapsed, NORMAL_SHUTDOWN_BOUND_SECONDS)
                if second_signal_delay is not None:
                    self.assertLess(elapsed, SECOND_SIGNAL_BOUND_SECONDS + second_signal_delay)
                    self.assertIsNotNone(second_started, "the second signal must be delivered")
                    self.assertLess(time.monotonic() - second_started, SECOND_SIGNAL_BOUND_SECONDS)
                expected_status = -(second_signal_number or first_signal) if second_signal_delay is not None else 0
                self.assertEqual(process.returncode, expected_status, stderr)
                socket_path = root / "state" / "canvas.sock"
                self.assertFalse(socket_path.exists(), "shutdown removes its owned Unix socket")
                row_count = None
                database = root / "state" / "canvas.sqlite3"
                if handler_delay:
                    with sqlite3.connect(database) as db:
                        row_count = db.execute(
                            "SELECT count(*) FROM sync_documents WHERE scope='drafts' AND id=?",
                            ("shutdown-test:chat",),
                        ).fetchone()[0]
                    self.assertEqual(row_count, 0 if second_signal_delay is not None else 1)
                restart_capture = None
                if not startup_delay:
                    with sqlite3.connect(database) as db:
                        restart_capture = db.execute(
                            "SELECT json_extract(record, '$.restartRecovery.stage') "
                            "FROM runtime_agents WHERE id='shutdown-test-agent'",
                        ).fetchone()
                    self.assertEqual(
                        restart_capture,
                        ("pending",),
                        "Runtime.close writes restart capture before a second-signal exit",
                    )
                print(json.dumps({
                    "case": {
                        "streams": stream_count,
                        "unread": unread_stream,
                        "handlerSeconds": handler_delay,
                        "startupSeconds": startup_delay,
                        "updatesStartupSeconds": updates_delay,
                        "firstSignal": signal.Signals(first_signal).name,
                        "secondSignal": signal.Signals(second_signal_number or first_signal).name if second_signal_delay is not None else None,
                        "secondSignalDelay": second_signal_delay,
                        "draftRows": row_count,
                        "restartCapture": restart_capture,
                    },
                    "exitCode": process.returncode,
                    "elapsedSeconds": round(elapsed, 3),
                    "elapsedAfterSecondSignal": round(time.monotonic() - second_started, 3) if second_started else None,
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

    def test_second_signal_closes_bounded_stream_drain(self):
        self.run_shutdown(stream_count=1, second_signal_delay=0.1)

    def test_second_sigterm_closes_bounded_two_stream_drain(self):
        self.run_shutdown(stream_count=2, second_signal_delay=0.1)

    def test_second_sigint_closes_bounded_stream_drain(self):
        self.run_shutdown(
            stream_count=1, second_signal_delay=0.1, first_signal=signal.SIGINT,
        )

    def test_second_signal_exits_five_second_handler(self):
        self.run_shutdown(handler_delay=5, second_signal_delay=0.1)

    def test_second_signal_exits_thirty_second_handler(self):
        self.run_shutdown(handler_delay=30, second_signal_delay=0.1)

    def test_thirty_second_handler_exits_after_second_signal_at_each_gap(self):
        for delay in (0.1, 1.0, 3.0):
            with self.subTest(delay=delay):
                self.run_shutdown(handler_delay=30, second_signal_delay=delay)

    def test_second_sigint_exits_thirty_second_handler_after_one_second(self):
        self.run_shutdown(
            handler_delay=30, second_signal_delay=1.0,
            second_signal_number=signal.SIGINT,
        )

    def test_single_signal_allows_five_second_handler_to_finish(self):
        elapsed, _, _, _ = self.run_shutdown(handler_delay=5)
        self.assertGreaterEqual(elapsed, 4.5)

    def test_single_signal_bounds_thirty_second_handler_after_completion(self):
        elapsed, _, _, _ = self.run_shutdown(handler_delay=30)
        self.assertGreaterEqual(elapsed, 29)

    def test_first_signal_during_runtime_construction_does_not_announce_url(self):
        self.run_shutdown(startup_delay=6)

    def test_second_signal_during_update_startup_exits_promptly(self):
        for delay in (0.1, 1.0, 3.0):
            with self.subTest(delay=delay):
                self.run_shutdown(updates_delay=30, second_signal_delay=delay)

    def test_third_signal_abandons_a_stuck_cleanup(self):
        with tempfile.TemporaryDirectory(prefix="backend-shutdown-third-") as directory:
            root = Path(directory)
            port = self.free_port()
            process, marker = self.start_backend(root, port, cleanup_delay=30)
            try:
                self.wait_for_start(process, port)
                process.send_signal(signal.SIGTERM)
                time.sleep(0.1)
                process.send_signal(signal.SIGTERM)
                self.wait_marker(marker, process)
                started = time.monotonic()
                process.send_signal(signal.SIGINT)
                stdout, stderr = process.communicate(timeout=3)
                elapsed = time.monotonic() - started
                self.assertEqual(process.returncode, -signal.SIGINT, stderr)
                self.assertLess(elapsed, 3)
                self.assertIn("cleanup abandoned after third shutdown signal", stderr)
                self.assertNotRegex(stderr, r"Traceback|Exception in")
                self.assertFalse((root / "state" / "canvas.sock").exists())
                print(json.dumps({
                    "case": {"cleanupSeconds": 30, "signals": ["SIGTERM", "SIGTERM", "SIGINT"]},
                    "exitCode": process.returncode,
                    "elapsedSeconds": round(elapsed, 3),
                    "stderr": stderr.splitlines()[-2:],
                    "stdout": stdout.splitlines()[-1:],
                }), flush=True)
            finally:
                if process.poll() is None:
                    process.kill()
                    process.wait(timeout=5)


if __name__ == "__main__":
    unittest.main()
