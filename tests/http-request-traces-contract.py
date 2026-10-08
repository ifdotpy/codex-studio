#!/usr/bin/env python3
"""Slow real HTTP reads identify their wait without retaining query values."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

from concurrent.futures import ThreadPoolExecutor
import asyncio
from contextlib import nullcontext
import http.client
import json
from pathlib import Path
import sys
import tempfile
import threading
from types import SimpleNamespace
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
from codex_canvas import Canvas, make_server
import codex_http_traces as traces
from studio_api.middleware import HttpTraceMiddleware


class HttpRequestTracesContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="studio-http-request-traces-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.journal = self.root / "diagnostics" / "http-requests.json"
        self.clock = 100.0
        clock = SimpleNamespace(monotonic=lambda: self.clock,
                                time=lambda: 1700000000 + self.clock)
        state = patch.multiple(traces, time=clock, SLOW_MS=50, _ACTIVE={}, _RECENT=[],
                               _SEQUENCE=0, _REVISION=0, _SAVED=None, _UNTRACKED=0,
                               _ARCHIVE_CHECKED=None)
        state.start()
        self.addCleanup(state.stop)
        self.entered = threading.Event()
        self.release = threading.Event()
        self.finished = threading.Event()
        self.server_error = threading.Event()
        self.server_errors = []
        finish = traces.finish

        def observed_finish(key, status, outcome):
            finish(key, status, outcome)
            if key is not None:
                self.finished.set()

        finished = patch.object(traces, "finish", side_effect=observed_finish)
        finished.start()
        self.addCleanup(finished.stop)
        self.canvas = Canvas(self.root)
        self.canvas.transcript = self.blocked_transcript
        self.server = make_server(self.canvas)
        self.addCleanup(self.server.server_close)
        self.server.handle_error = self.record_server_error
        self.server_thread = threading.Thread(target=self.server.serve_forever,
                                              kwargs={"poll_interval": .01}, daemon=True)
        self.server_thread.start()
        self.addCleanup(self.stop_server)
        self.pool = ThreadPoolExecutor(max_workers=1)
        self.addCleanup(self.pool.shutdown, wait=True)

    def stop_server(self):
        self.release.set()
        self.server.shutdown()
        self.server_thread.join(timeout=1)

    def record_server_error(self, _request, _address):
        self.server_errors.append(type(sys.exception()).__name__)
        self.server_error.set()

    def blocked_transcript(self, agent):
        self.entered.set()
        if not self.release.wait(2):
            raise RuntimeError("The private HTTP barrier did not release")
        return {"id": agent, "items": []}

    def request(self, path, method="GET", token=None, read_limit=None, include_headers=False):
        connection = http.client.HTTPConnection("127.0.0.1", self.server.server_port, timeout=1)
        try:
            headers = {"X-Canvas-Token": token} if token else {}
            if method == "POST":
                headers["Content-Type"] = "application/json"
            connection.request(method, path, body=b"{}" if method == "POST" else None,
                               headers=headers)
            response = connection.getresponse()
            result = {"status": response.status,
                      "body": response.read(read_limit) if read_limit else response.read()}
            if include_headers:
                result["headers"] = dict(response.getheaders())
            return result
        except http.client.RemoteDisconnected:
            return {"disconnected": True}
        finally:
            connection.close()

    def read_journal(self):
        return json.loads(self.journal.read_text())

    def test_active_wait_is_saved_before_a_real_http_response_then_finishes_with_status(self):
        path = "/api/sync/pull?scope=transcript%3Aprivate-agent-id&token=private-query-token"
        request = self.pool.submit(self.request, path)
        try:
            self.assertTrue(self.entered.wait(.5), "The real HTTP transcript callback did not enter")
            self.clock += .1
            traces.watchdog(self.root)
            self.assertFalse(request.done(), "The response finished before its wait was recorded")
            active = self.read_journal()
            self.assertEqual(len(active["active"]), 1, active)
            row = active["active"][0]
            self.assertEqual((row["endpoint"], row["method"], row["scope"]),
                             ("/api/sync/pull", "GET", "transcript"))
            self.assertEqual(row["durationMs"], 100)
            self.assertLessEqual(len(row["frames"]), 8)
            for frame in row["frames"]:
                self.assertEqual(set(frame), {"file", "function", "line"})
                self.assertEqual(frame["file"], Path(frame["file"]).name)
            text = self.journal.read_text()
            self.assertNotIn("private-agent-id", text)
            self.assertNotIn("private-query-token", text)
            self.assertNotIn("token=", text)
            self.assertEqual(self.journal.stat().st_mode & 0o777, 0o600)
            self.assertEqual(self.journal.parent.stat().st_mode & 0o777, 0o700)
        finally:
            self.release.set()
        response = request.result(timeout=1)
        self.assertEqual(response["status"], 200, response)
        self.assertTrue(self.finished.wait(.5))
        traces.watchdog(self.root)
        completed = self.read_journal()
        self.assertEqual(completed["active"], [])
        self.assertEqual(len(completed["recent"]), 1)
        saved = completed["recent"][0]
        self.assertEqual(saved["id"], row["id"])
        self.assertEqual((saved["status"], saved["outcome"], saved["durationMs"]),
                         (200, "complete", 100))
        self.assertEqual(saved["frames"], row["frames"])
        self.assertNotIn("private-agent-id", self.journal.read_text())
        self.assertNotIn("private-query-token", self.journal.read_text())

    def test_fast_get_post_and_sse_do_not_retain_history_or_rewrite_the_journal(self):
        traces.watchdog(self.root)
        before = (self.journal.read_bytes(), self.journal.stat().st_mtime_ns)
        response = self.request("/api/session")
        self.assertEqual(response["status"], 200)
        self.assertTrue(self.finished.wait(.5))
        token = json.loads(response["body"])["token"]
        # FastAPI's GET-only route returns method-not-allowed; the ASGI migration
        # replaced the old handler's path-level 404 behavior (92e2db1d).
        self.assertEqual(self.request("/api/session", "POST", token)["status"], 405)
        query = "protocol=3&resources=%5B%7B%22kind%22%3A%22drafts%22%7D%5D"
        stream = self.request("/api/sync/stream?" + query, read_limit=128)
        self.assertEqual(stream["status"], 200)
        self.assertIn(b"event: resources", stream["body"])
        self.assertNotIn(b"event: api-schema", stream["body"])
        self.assertEqual(traces._ACTIVE, {})
        self.assertEqual(traces._RECENT, [])
        self.assertEqual(traces._SEQUENCE, 1, "POST and SSE must not create tracking identities")
        traces.watchdog(self.root)
        self.assertEqual((self.journal.read_bytes(), self.journal.stat().st_mtime_ns), before)
        self.assertNotIn(token, self.journal.read_text())

    def test_handler_exception_and_broken_response_keep_the_error_outcome(self):
        for exception, status in ((RuntimeError, None), (BrokenPipeError, 200)):
            with self.subTest(exception=exception.__name__):
                traces._ACTIVE.clear()
                traces._RECENT.clear()
                sent = []

                async def failing_endpoint(_scope, _receive, send):
                    if status is not None:
                        await send({"type": "http.response.start", "status": status})
                        await send({"type": "http.response.body", "body": b"response"})
                    self.clock += .1
                    raise exception("Private transport fixture")

                async def receive():
                    return {"type": "http.disconnect"}

                async def send(message):
                    sent.append(message)

                scope = {"type": "http", "method": "GET", "path": "/api/session",
                         "query_string": b"token=private-error-token"}
                with self.assertRaises(exception):
                    asyncio.run(HttpTraceMiddleware(failing_endpoint)(scope, receive, send))
                traces.watchdog(self.root)
                journal = self.read_journal()
                self.assertEqual(journal["active"], [])
                row = journal["recent"][-1]
                self.assertEqual((row["status"], row["outcome"]), (status, "error"), (row, sent, traces._SEQUENCE))
                self.assertNotIn("private-error-token", self.journal.read_text())
                self.assertNotIn("Private transport fixture", self.journal.read_text())
                self.assertEqual(sent, [] if status is None else [
                    {"type": "http.response.start", "status": status},
                    {"type": "http.response.body", "body": b"response"},
                ])

    @unittest.expectedFailure
    def test_slow_asgi_request_trace_captures_the_blocking_transcript_callback(self):
        request = self.pool.submit(
            self.request,
            "/api/sync/pull?scope=transcript%3Aprivate-agent-id&token=private-query-token",
        )
        try:
            self.assertTrue(self.entered.wait(.5))
            self.clock += .1
            traces.watchdog(self.root)
            row = self.read_journal()["active"][0]
            self.assertIn("blocked_transcript", {frame["function"] for frame in row["frames"]})
        finally:
            self.release.set()
        self.assertEqual(request.result(timeout=1)["status"], 200)

    def test_tracking_limits_and_malformed_targets_remain_bounded(self):
        self.assertIsNone(traces.begin("GET", "http://[invalid"))
        self.assertIsNone(traces.begin("POST", "/api/session"))
        self.assertIsNone(traces.begin("GET", "/api/sync/stream"))
        keys = [traces.begin("GET", "/api/session") for _ in range(traces.ACTIVE_LIMIT)]
        self.assertTrue(all(keys))
        self.assertIsNone(traces.begin("GET", "/api/session"))
        self.assertEqual(len(traces._ACTIVE), traces.ACTIVE_LIMIT)
        self.assertEqual(traces._UNTRACKED, 1)
        self.clock += .1
        for key in keys:
            traces.finish(key, 200, "complete")
        self.assertEqual(traces._ACTIVE, {})
        self.assertEqual(len(traces._RECENT), traces.HISTORY_LIMIT)
        traces.watchdog(self.root)
        journal = self.read_journal()
        self.assertEqual(journal["untrackedStarts"], 1)
        self.assertEqual(len(journal["recent"]), traces.HISTORY_LIMIT)

    def test_begin_and_finish_failure_preserve_http_success_without_false_active_requests(self):
        discard = traces.discard
        for operation in ("begin", "finish"):
            with self.subTest(operation=operation):
                retired = threading.Event()
                trace_keys = []

                begin = traces.begin

                def observed_begin(*args):
                    key = begin(*args)
                    trace_keys.append(key)
                    return key

                discarded_keys = []

                def observed_discard(key):
                    discard(key)
                    discarded_keys.append(key)
                    if key in trace_keys:
                        retired.set()

                with self.assertLogs("codex.http", level="WARNING") as logs:
                    begin_patch = (patch.object(traces, "begin", side_effect=observed_begin)
                                   if operation == "finish" else nullcontext())
                    with begin_patch, \
                         patch.object(traces, operation, side_effect=RuntimeError("private-journal-failure")), \
                         patch.object(traces, "discard", side_effect=observed_discard):
                        response = self.request("/api/session?token=private-failed-query-token")
                        self.assertEqual(response["status"], 200, response)
                        self.assertTrue(json.loads(response["body"])["token"])
                        if operation == "finish":
                            self.assertTrue(retired.wait(30), "The failed completion did not retire its active trace")
                            self.assertEqual(discarded_keys, trace_keys)
                self.assertEqual(traces._ACTIVE, {})
                self.assertEqual(traces._RECENT, [])
                self.assertIn("RuntimeError", "\n".join(logs.output))
                self.assertNotIn("private-journal-failure", "\n".join(logs.output))
                self.assertNotIn("private-failed-query-token", "\n".join(logs.output))
        self.assertEqual(self.server_errors, [])

    def test_an_inherited_old_handler_frame_cannot_start_an_unfinishable_trace(self):
        # The former BaseHTTPRequestHandler frame no longer exists after the
        # ASGI server migration (92e2db1d). Keep the current-path lifecycle check.
        self.assertFalse(hasattr(self.server, "RequestHandlerClass"))
        self.assertEqual(self.request("/api/session")["status"], 200)
        self.assertTrue(self.finished.wait(.5))
        self.assertEqual(traces._SEQUENCE, 1)
        self.assertEqual(traces._ACTIVE, {})

    def test_useful_previous_coverage_is_archived_before_a_new_journal_replaces_it(self):
        key = traces.begin("GET", "/api/session")
        self.clock += .1
        traces.finish(key, 200, "complete")
        traces.watchdog(self.root)
        original = self.journal.read_bytes()
        old = self.read_journal()
        self.assertEqual(len(old["recent"]), 1)
        previous = self.journal.with_name("http-requests.previous.json")
        self.assertFalse(previous.exists())

        with patch.multiple(traces, _STARTED_AT=old["coverageStartedAt"] + 1,
                            _ARCHIVE_CHECKED=None, _SAVED=None, _ACTIVE={}, _RECENT=[],
                            _SEQUENCE=0, _REVISION=0, _UNTRACKED=0):
            traces.watchdog(self.root)
            self.assertEqual(previous.read_bytes(), original)
            self.assertEqual(json.loads(previous.read_bytes())["recent"][0]["id"], key)
            fresh = self.read_journal()
            self.assertEqual(fresh["coverageStartedAt"], old["coverageStartedAt"] + 1)
            self.assertEqual(fresh["active"], [])
            self.assertEqual(fresh["recent"], [])
            self.assertEqual(previous.stat().st_mode & 0o777, 0o600)
            before = self.journal.read_bytes()
            traces.watchdog(self.root)
            self.assertEqual(self.journal.read_bytes(), before)
            self.assertEqual(previous.read_bytes(), original)

    def test_an_oversized_previous_journal_is_preserved_and_reports_only_the_error_type(self):
        self.journal.parent.mkdir(mode=0o700)
        previous = self.journal.with_name("http-requests.previous.json")
        base = {"version": 1, "pid": -1, "coverageStartedAt": 0, "active": [], "recent": []}
        cases = {
            "bytes": b"x" * (traces.JOURNAL_LIMIT + 1),
            "active": json.dumps({**base, "active": [{"id": "private-old-request"}] *
                                  (traces.ACTIVE_LIMIT + 1)}).encode(),
            "recent": json.dumps({**base, "recent": [{"id": "private-old-request"}] *
                                  (traces.HISTORY_LIMIT + 1)}).encode(),
        }
        for kind, data in cases.items():
            with self.subTest(limit=kind):
                self.journal.write_bytes(data)
                with self.assertLogs("codex.http", level="WARNING") as logs:
                    traces.watchdog(self.root)
                self.assertEqual(self.journal.read_bytes(), data)
                self.assertFalse(previous.exists())
                self.assertIsNone(traces._SAVED)
                self.assertIsNone(traces._ARCHIVE_CHECKED)
                self.assertEqual(list(self.journal.parent.glob(".http-requests-*")), [])
                self.assertIn("OSError", "\n".join(logs.output))
                self.assertNotIn("private-old-request", "\n".join(logs.output))


if __name__ == "__main__":
    unittest.main()
