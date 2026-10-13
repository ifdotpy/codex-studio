#!/usr/bin/env python3
"""Native action receipts and provider targets observed through HTTP."""

from __future__ import annotations

import json
import os
from pathlib import Path
import tempfile
import unittest
import uuid
from urllib.parse import urlencode
from unittest.mock import patch

from test_isolation import isolate_supervisor_environment

isolate_supervisor_environment()

from http_backend_harness import (
    HTTP_WAIT_GUARD_SECONDS,
    BackendProcess,
    TimedHTTPContractTest,
    start_contract_backend,
)


class BackendStartupReport(unittest.TestCase):
    def test_missing_native_module_names_the_backend_interpreter_setup(self) -> None:
        # The backend child gets no PYTHONPATH, so a missing module needs a
        # setup hint that a copy on PYTHONPATH cannot satisfy.
        with tempfile.TemporaryDirectory(dir=os.environ.get("TMPDIR")) as folder:
            backend = BackendProcess(Path(folder), initialize_workspace=False)
            try:
                backend._backend_python = Path("/fixture/bin/python")
                backend._stderr = [
                    "codex-canvas: The backend requires studio_operations_native.\n",
                ]
                with self.assertRaises(RuntimeError) as caught:
                    backend._raise_backend_not_ready(
                        "process exited before the ready line", 1,
                    )
                backend._stderr = ["backend bootstrap failed\n"]
                with self.assertRaises(RuntimeError) as unrelated:
                    backend._raise_backend_not_ready(
                        "process exited before the ready line", 1,
                    )
            finally:
                backend.close()
        message = str(caught.exception)
        self.assertIn("/fixture/bin/python", message)
        self.assertIn("without the caller's PYTHONPATH", message)
        self.assertIn("install-cli.py --dev", message)
        self.assertIn(".github/actions/fast-tier-setup", message)
        self.assertNotIn("install-cli.py", str(unrelated.exception))


class NativeActionHTTPContract(TimedHTTPContractTest, unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.backend = start_contract_backend(cls)
        cls.headers = {
            "Origin": cls.backend.origin or "",
            "X-Canvas-Token": cls.backend.session["token"],
            "X-Canvas-Workspace": cls.backend.identity["workspaceId"],
            "X-Studio-API-Schema": cls.backend.schema_hash,
        }

    def test_backend_startup_error_reports_exit_status_and_stderr_tail(self) -> None:
        previous_stderr = self.backend._stderr
        self.backend._stderr = ["backend bootstrap failed\n"]
        try:
            with patch.object(self.backend, "stop"):
                with self.assertRaisesRegex(RuntimeError, "child exit status=19") as caught:
                    self.backend._raise_backend_not_ready(
                        "process exited before the ready line", 19,
                    )
        finally:
            self.backend._stderr = previous_stderr
        self.assertIn("stderr tail:", str(caught.exception))
        self.assertIn("backend bootstrap failed", str(caught.exception))

    def create_lead(self, identifier: str) -> dict[str, object]:
        response = self.backend.request(
            "POST", "/api/leads",
            body={"id": identifier, "cwd": str(self.backend.cwd), "workspaceMode": "worktree"},
            headers=self.headers,
        )
        self.assertEqual(response.status, 200, response.body[:1000])
        return response.json()

    def entity(self, identifier: str) -> dict[str, object]:
        response = self.backend.request("GET", "/api/sync/pull?" + urlencode({
            "scope": "state:entities:v1", "after": 0, "limit": 500,
        }))
        self.assertEqual(response.status, 200, response.body[:1000])
        for document in response.json()["documents"]:
            if document["id"] == f"entity:agent:{identifier}":
                return json.loads(document["payload"])["value"]
        self.fail(f"The entity feed omitted agent {identifier}")

    def send_instruction_and_wait_for_completion(self, identifier: str) -> None:
        message_id = str(uuid.uuid5(uuid.NAMESPACE_URL, f"http-native-instruction:{identifier}"))
        sent = self.backend.request(
            "POST", "/api/messages",
            body={"id": message_id, "room": identifier,
                  "text": "Reply with the fixture result.", "delivery": "queue", "assets": []},
            headers=self.headers,
        )
        self.assertEqual(sent.status, 200, sent.body[:1000])
        transcript_path = "/api/transcript?" + urlencode({"id": identifier})
        transcript: object = None
        def settled_turn() -> bool:
            nonlocal transcript
            frames = self.backend.received_provider_frames()
            response = self.backend.request(
                "GET", transcript_path, timeout=HTTP_WAIT_GUARD_SECONDS,
            )
            self.assertEqual(response.status, 200, response.body[:1000])
            transcript = response.json()
            current = self.entity(identifier)
            return (any(frame.get("method") == "turn/start" for frame in frames)
                    and "Recovered output." in json.dumps(transcript)
                    and not current.get("inFlight")
                    and current.get("status") not in {"starting", "running"})
        try:
            self.backend.wait_until(settled_turn, "provider turn completion and idle state")
        except TimeoutError:
            self.fail(f"The provider turn did not complete through HTTP: {transcript!r}")

    def action_once(self, action: str, identifier: str) -> None:
        lead = self.create_lead(identifier)
        self.send_instruction_and_wait_for_completion(lead["id"])
        request_id = f"http-native-{action}-{identifier}"
        body = {"id": lead["id"], "action": action, "request_id": request_id}
        first = self.backend.request("POST", "/api/action", body=body, headers=self.headers)
        self.assertEqual(first.status, 200, first.body[:1000])
        result = first.json()
        self.assertEqual(result["outcome"]["status"], "acknowledged", result)

        method = "review/start" if action == "review" else "thread/compact/start"
        frames = self.backend.received_provider_frames()
        calls = [frame for frame in frames if frame.get("method") == method]
        self.assertEqual(len(calls), 1, calls)
        current = self.entity(lead["id"])
        self.assertEqual(calls[0]["params"]["threadId"], current["threadId"])
        self.assertEqual(result["receipt"]["requestId"], request_id)

        replay = self.backend.request("POST", "/api/action", body=body, headers=self.headers)
        self.assertEqual(replay.status, 200, replay.body[:1000])
        self.assertEqual(replay.json()["receipt"], result["receipt"])
        self.assertEqual(replay.json()["outcome"], result["outcome"])
        self.assertTrue(replay.json()["replayed"])
        frames = self.backend.received_provider_frames()
        self.assertEqual(sum(frame.get("method") == method for frame in frames), 1)

    def test_review_request_receipt_replay_targets_one_provider_thread(self) -> None:
        self.action_once("review", "10000000-0000-4000-8000-000000000101")

    def test_compact_request_receipt_replay_targets_one_provider_thread(self) -> None:
        self.action_once("compact", "10000000-0000-4000-8000-000000000102")


if __name__ == "__main__":
    unittest.main(verbosity=2)
