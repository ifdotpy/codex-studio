#!/usr/bin/env python3
"""The control CLI reads request receipts through the current HTTP API only."""

# Support direct execution without runner-provided PYTHONPATH.
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

from contextlib import redirect_stderr, redirect_stdout
import io
import json
import os
from pathlib import Path
import runpy
import socket
import sys
import unittest
from unittest.mock import patch
from urllib.error import HTTPError
import codex_api_client

SCRIPT = SERVER_SOURCE_ROOT / "codex-control"


class ControlRequests(unittest.TestCase):
    def invoke(self, arguments, response=None, error=None):
        output, errors = io.StringIO(), io.StringIO()
        with patch.dict(os.environ, {"CODEX_AGENT_OWNER": "", "CODEX_BOARD_OWNER": ""}), \
                patch.object(sys, "argv", [str(SCRIPT), *arguments]), \
                patch.object(codex_api_client, "open_request", side_effect=error, return_value=response) as network, \
                patch.object(socket, "create_connection",
                             side_effect=AssertionError("network escape")) as connect, \
                patch.object(socket.socket, "connect",
                             side_effect=AssertionError("socket escape")) as socket_connect, \
                redirect_stdout(output), redirect_stderr(errors):
            try:
                runpy.run_path(str(SCRIPT), run_name="__main__")
                status = 0
            except SystemExit as exit_code:
                status = exit_code.code
        connect.assert_not_called()
        socket_connect.assert_not_called()
        return status, output.getvalue(), errors.getvalue(), network.call_args_list

    def test_receipts_use_the_current_api_and_exact_encoded_identity(self):
        expected = {"id": "account:thread:call", "outcome": "unknown"}
        status, output, errors, calls = self.invoke(
            ["requests", "agent/a", "account:thread:call"],
            response=io.BytesIO(json.dumps(expected).encode()),
        )
        self.assertEqual(status, 0, errors)
        self.assertEqual(json.loads(output), expected)
        self.assertEqual(len(calls), 1)
        self.assertEqual(calls[0].args[0].full_url,
                         "http://127.0.0.1:4620/api/tool-requests?agent=agent/a&request_id=account%3Athread%3Acall")

    def test_requests_does_not_bootstrap_workspace_state(self):
        status, output, errors, calls = self.invoke(
            ["requests", "agent"], response=io.BytesIO(b'{"requests":[]}'))
        self.assertEqual(status, 0, errors)
        self.assertEqual(json.loads(output), {"requests": []})
        self.assertEqual(len(calls), 1)
        self.assertEqual(calls[0].args[0].full_url,
                         "http://127.0.0.1:4620/api/tool-requests?agent=agent")

    def test_old_server_and_other_http_errors_are_returned_without_fallback(self):
        for origin in ("http://127.0.0.1:4620", "https://example.invalid"):
            for code in (401, 403, 404, 408, 500):
                with self.subTest(origin=origin, code=code):
                    error = HTTPError(origin + "/api/tool-requests", code, "Rejected", {},
                                      io.BytesIO(b'{"error":"Receipt endpoint rejected the request"}'))
                    status, output, errors, calls = self.invoke(
                        ["--url", origin, "requests", "agent", "call"], error=error)
                    self.assertEqual(status, 1)
                    self.assertEqual(output, "")
                    self.assertIn("Receipt endpoint rejected the request", errors)
                    self.assertEqual(len(calls), 1)
                    self.assertEqual(calls[0].args[0].full_url,
                                     origin + "/api/tool-requests?agent=agent&request_id=call")


if __name__ == "__main__":
    unittest.main()
