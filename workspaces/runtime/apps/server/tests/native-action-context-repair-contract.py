#!/usr/bin/env python3
"""Native action receipts and provider targets observed through HTTP."""

from __future__ import annotations

import json
import time
import unittest
from urllib.parse import urlencode

from test_isolation import isolate_supervisor_environment

isolate_supervisor_environment()

from http_backend_harness import TimedHTTPContractTest, start_contract_backend


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
        message_id = f"http-native-instruction-{identifier}"
        sent = self.backend.request(
            "POST", "/api/messages",
            body={"id": message_id, "room": identifier,
                  "text": "Reply with the fixture result.", "delivery": "queue", "assets": []},
            headers=self.headers,
        )
        self.assertEqual(sent.status, 200, sent.body[:1000])
        deadline = time.monotonic() + 3.0
        transcript_path = "/api/transcript?" + urlencode({"id": identifier})
        transcript: object = None
        while time.monotonic() < deadline:
            frames = self.backend.received_provider_frames()
            response = self.backend.request("GET", transcript_path, timeout=1.0)
            self.assertEqual(response.status, 200, response.body[:1000])
            transcript = response.json()
            if (any(frame.get("method") == "turn/start" for frame in frames)
                    and "Recovered output." in json.dumps(transcript)):
                return
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
