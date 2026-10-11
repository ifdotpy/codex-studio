#!/usr/bin/env python3
"""Recover provider tool requests through the process and HTTP boundaries."""

import json
import unittest
from urllib.parse import urlencode

from test_isolation import isolate_supervisor_environment

isolate_supervisor_environment()

from http_backend_harness import TimedHTTPContractTest, start_contract_backend


class ToolRequestHTTPContract(TimedHTTPContractTest, unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.backend = start_contract_backend(cls, "tool-request-http.json")
        cls.session = cls.backend.session
        cls.identity = cls.backend.identity
        cls.schema_hash = cls.backend.schema_hash

    def headers(self) -> dict[str, str]:
        return {
            "Origin": self.backend.origin or "",
            "X-Canvas-Token": self.session["token"],
            "X-Canvas-Workspace": self.identity["workspaceId"],
            "X-Studio-API-Schema": self.schema_hash,
        }

    def create_lead(self) -> str:
        response = self.backend.request(
            "POST", "/api/leads",
            body={"cwd": str(self.backend.cwd), "workspaceMode": "worktree"},
            headers=self.headers(),
        )
        self.assertEqual(response.status, 200, response.body[:1000])
        return response.json()["id"]

    def test_provider_tool_request_is_recoverable_and_scoped_by_actor(self) -> None:
        lead = self.create_lead()
        message_id = "tool-request-http-contract-message"
        sent = self.backend.request(
            "POST", "/api/messages",
            body={"id": message_id, "room": lead, "text": "Exercise the replayed tool request.",
                  "delivery": "queue", "assets": []},
            headers=self.headers(),
        )
        self.assertEqual(sent.status, 200, sent.body[:1000])
        self.assertEqual(sent.json()["id"], message_id)

        result_frame: dict[str, object] | None = None
        def received_result() -> dict[str, object] | None:
            nonlocal result_frame
            frames = self.backend.received_provider_frames()
            result_frame = next((frame for frame in frames
                                 if frame.get("id") == "boundary-tool-rpc-1"
                                 and isinstance(frame.get("result"), dict)), None)
            return result_frame
        try:
            self.backend.wait_until(received_result, "provider tool result frame")
        except TimeoutError:
            self.fail("The fake Codex did not receive the tool result frame")
        self.assertIsNotNone(result_frame, "the fake Codex did not receive the tool result frame")
        provider_result = result_frame["result"]
        self.assertTrue(provider_result["success"], provider_result)
        operation = json.loads(provider_result["contentItems"][0]["text"])
        initial_agents = operation["agents"]
        self.assertEqual(len(initial_agents), 2)
        self.assertTrue(all(agent["status"] == "queued" for agent in initial_agents))
        listing = self.backend.request(
            "GET", "/api/tool-requests?" + urlencode({"agent": lead}),
        )
        self.assertEqual(listing.status, 200, listing.body[:1000])
        requests = listing.json()["requests"]
        self.assertTrue(requests, "the fixture app-server tool call was not recorded")
        self.assertEqual(len(requests), 1)
        listed = requests[0]
        self.assertEqual(listed["tool"], "orchestration_spawn")
        self.assertEqual(listed["callId"], "boundary-tool-call-1")
        self.assertEqual(listed["outcome"], "applied")
        self.assertEqual(listed["request_id"], "batch-1")
        self.assertNotIn("result", listed)
        self.assertNotIn("signature", listed)

        receipt = self.backend.request(
            "GET", "/api/tool-requests?" + urlencode({"agent": lead, "request_id": listed["id"]}),
        )
        self.assertEqual(receipt.status, 200, receipt.body[:1000])
        self.assertEqual(receipt.json()["id"], listed["id"])
        self.assertEqual(receipt.json()["outcome"], listed["outcome"])
        self.assertEqual(receipt.json()["agentIds"], listed["agentIds"])
        states = receipt.json()["agents"]
        self.assertEqual({agent["id"] for agent in states}, set(listed["agentIds"]))
        self.assertTrue(all(isinstance(agent["status"], str) for agent in states))
        self.assertTrue(all(agent["deleted"] is False for agent in states))

        other = self.create_lead()
        other_list = self.backend.request(
            "GET", "/api/tool-requests?" + urlencode({"agent": other}),
        )
        self.assertEqual(other_list.status, 200, other_list.body[:1000])
        self.assertEqual(other_list.json()["requests"], [])
        hidden = self.backend.request(
            "GET", "/api/tool-requests?" + urlencode({"agent": other, "request_id": listed["id"]}),
        )
        self.assertEqual(hidden.status, 200, hidden.body[:1000])
        self.assertEqual(hidden.json()["stage"], "not_found")
        missing = self.backend.request(
            "GET", "/api/tool-requests?" + urlencode({"agent": lead, "request_id": "missing"}),
        )
        self.assertEqual(missing.status, 200, missing.body[:1000])
        self.assertEqual(missing.json()["outcome"], "unknown")
        denied = self.backend.request(
            "GET", "/api/tool-requests?" + urlencode({"agent": lead}),
            headers={"Origin": "https://outside.invalid"},
        )
        self.assertEqual(denied.status, 403, denied.body[:1000])

    def test_cancel_endpoint_rejects_missing_token_wrong_token_and_foreign_origin(self) -> None:
        lead = self.create_lead()
        body = {"agent": lead, "request_id": "missing-request"}
        headers = self.headers()
        for denied_headers in (
                {},
                {"X-Canvas-Token": "wrong"},
                {"X-Canvas-Token": headers["X-Canvas-Token"],
                 "Origin": "https://outside.invalid"}):
            with self.subTest(headers=denied_headers):
                response = self.backend.request(
                    "POST", "/api/tool-requests/cancel", body=body,
                    headers=denied_headers,
                )
                self.assertEqual(response.status, 403, response.body[:1000])


if __name__ == "__main__":
    unittest.main(verbosity=2)
