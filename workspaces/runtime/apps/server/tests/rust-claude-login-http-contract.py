#!/usr/bin/env python3
"""Five sign-in operations through either backend, using only an owned scripted CLI."""
from __future__ import annotations
import os
from pathlib import Path
import sys
import unittest
import uuid

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()
from http_backend_harness import BackendProcess


class ClaudeLoginHTTPContract(unittest.TestCase):
    def test_add_status_code_reauth_and_cancel(self):
        backend = BackendProcess(Path(os.environ["TMPDIR"]))
        script = backend.root / "scripted-login"
        script.write_text("""#!/bin/sh
if [ "$2" = status ]; then
  printf '%s\\n' '{"loggedIn":true,"authMethod":"claude.ai","email":"fixture@example.test","subscriptionType":"max"}'
  exit 0
fi
printf '%s\\n' 'https://claude.com/cai/oauth/authorize?fixture=1'
IFS= read -r code || exit 7
exit 0
""")
        script.chmod(0o700)
        backend._claude_bin = str(script)
        backend.create_codex_replay("restart-mid-turn-supervisor-replay.json")
        try:
            backend.start()
            headers = {"Origin": backend.origin, "X-Canvas-Token": backend.session["token"], "X-Studio-API-Schema": backend.schema_hash}
            def request(method, path, body=None, expected=200):
                result = backend.request(method, path, body=body, headers=headers)
                self.assertEqual(result.status, expected, result.body[:2000])
                return result.json()
            rid = str(uuid.uuid4())
            body = {"login_id": rid, "email": "fixture@example.test", "label": "Fixture"}
            request("POST", "/api/accounts/claude/add", body)
            def status(wanted):
                value = request("GET", "/api/accounts/claude/login?request_id=" + rid)
                self.assertNotEqual(value["status"], "error", value)
                return value if value["status"] == wanted else None
            backend.wait_until(lambda: status("pending"), "scripted verification URL")
            request("POST", "/api/accounts/claude/login/code", {"login_id":rid,"code":"fixture-code"})
            ready = backend.wait_until(lambda: status("ready"), "scripted account registration")
            self.assertEqual(ready["email"], "fixture@example.test")
            self.assertEqual(request("POST", "/api/accounts/claude/add", body)["accountKey"], ready["accountKey"])
            rid = str(uuid.uuid4())
            request("POST", "/api/accounts/claude/login", {"login_id":rid,"account_key":ready["accountKey"]})
            backend.wait_until(lambda: status("pending"), "scripted reauthentication")
            cancelled = request("POST", "/api/accounts/claude/login/cancel", {"login_id":rid})
            self.assertEqual(cancelled["status"], "cancelled")
            self.assertEqual(request("GET", "/api/accounts/claude/login?request_id=" + rid)["status"], "cancelled")
        finally:
            backend.close()


if __name__ == "__main__":
    unittest.main()
