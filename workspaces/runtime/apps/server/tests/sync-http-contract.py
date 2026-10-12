#!/usr/bin/env python3
"""Sync projection, draft validation and voice receipts at the HTTP boundary."""

import json
import unittest
from urllib.parse import urlencode

from test_isolation import isolate_supervisor_environment

isolate_supervisor_environment()

from http_backend_harness import TimedHTTPContractTest, start_contract_backend


class SyncHTTPContract(TimedHTTPContractTest, unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.backend = start_contract_backend(cls)
        cls.session = cls.backend.session
        cls.identity = cls.backend.identity
        cls.schema_hash = cls.backend.schema_hash
        lead = cls.backend.request(
            "POST", "/api/leads",
            body={"cwd": str(cls.backend.cwd), "workspaceMode": "worktree"},
            headers=cls.write_headers(),
        )
        if lead.status != 200:
            raise AssertionError((lead.status, lead.body[:1000]))
        cls.lead_id = lead.json()["id"]

    @classmethod
    def write_headers(cls) -> dict[str, str]:
        return {
            "Origin": cls.backend.origin or "",
            "X-Canvas-Token": cls.session["token"],
            "X-Canvas-Workspace": cls.identity["workspaceId"],
            "X-Studio-API-Schema": cls.schema_hash,
        }

    def test_entity_pull_projects_the_public_agent_fields(self) -> None:
        backend = self.backend
        rows: list[dict[str, object]] = []
        after = 0
        maximum = 0
        while True:
            pulled = backend.request(
                "GET", f"/api/sync/pull?scope=state%3Aentities%3Av1&after={after}&limit=100",
            )
            self.assertEqual(pulled.status, 200, pulled.body[:1000])
            page = pulled.json()
            self.assertEqual(page["workspaceId"], self.identity["workspaceId"])
            self.assertTrue(page["documents"])
            self.assertTrue(all(row["id"].startswith("entity:") for row in page["documents"]))
            rows.extend(page["documents"])
            after = page["checkpoint"]["seq"]
            maximum = page["maxSeq"]
            if after >= maximum:
                break

        encoded = next((row["payload"] for row in rows
                        if row["id"].startswith("entity:agent:")), None)
        self.assertIsNotNone(encoded, "the entity pull omitted the seeded lead")
        agent = json.loads(encoded)["value"]
        self.assertTrue({"id", "name", "status"} <= agent.keys())
        self.assertNotIn("nativeToolCatalog", agent)
        self.assertNotIn("accountHistory", agent)

    def test_removed_sync_and_transcript_routes_stay_unavailable(self) -> None:
        for path in ("/api/sync/generations", "/api/transcript/stream"):
            with self.subTest(path=path):
                response = self.backend.request("GET", path)
                self.assertIn(response.status, (404, 405), response.body[:1000])

    def test_draft_push_rejects_a_foreign_workspace_and_malformed_row(self) -> None:
        body = {"rows": [{"newDocumentState": {
            "id": "phone:lead", "seq": 0, "deleted": False,
            "payload": json.dumps({"text": "draft", "session": "lead", "device": "phone"}),
        }}]}
        foreign = self.backend.request(
            "POST", "/api/sync/drafts", body=body,
            headers={**self.write_headers(), "X-Canvas-Workspace": "wrong-workspace"},
        )
        self.assertEqual(foreign.status, 409, foreign.body[:1000])
        malformed = self.backend.request(
            "POST", "/api/sync/drafts", body={"rows": [{}]}, headers=self.write_headers(),
        )
        self.assertEqual(malformed.status, 400, malformed.body[:1000])

    def test_voice_submit_replays_its_stable_message_id_once(self) -> None:
        self.voice(
            "record", session_id="", event_id="sync-http-voice-one", kind="user",
            text="Raw recognized words",
        )
        body = {
            "agent": self.lead_id,
            "message_id": "sync-http-voice-message",
            "record_ids": ["sync-http-voice-one"],
            "edited_text": "Edited user request",
        }
        first = self.backend.request("POST", "/api/voice/submit", body=body,
                                     headers=self.write_headers())
        self.assertEqual(first.status, 200, first.body[:1000])
        replay = self.backend.request("POST", "/api/voice/submit", body=body,
                                      headers=self.write_headers())
        self.assertEqual(replay.status, 200, replay.body[:1000])
        self.assertEqual(replay.json()["id"], first.json()["id"])
        receipts = self.backend.request(
            "GET", "/api/messages/receipts?" + urlencode({
                "agent": self.lead_id,
                "ids": json.dumps(["sync-http-voice-message"]),
            }),
        )
        self.assertEqual(receipts.status, 200, receipts.body[:1000])
        self.assertEqual(
            [item["id"] for item in receipts.json()["items"]],
            ["sync-http-voice-message"],
        )

    def test_voice_rejects_spoofed_or_changed_actions(self) -> None:
        status = self.backend.request(
            "POST", "/api/voice/status", body={"agent": self.lead_id},
            headers=self.write_headers(),
        )
        self.assertEqual(status.status, 200, status.body[:1000])
        self.assertIsInstance(status.json()["configured"], bool)
        self.voice("record", session_id="", event_id="sync-http-change-record",
                   kind="user", text="Original voice text")
        original = self.backend.request(
            "POST", "/api/voice/submit",
            body={"agent": self.lead_id, "message_id": "sync-http-change-message",
                  "record_ids": ["sync-http-change-record"], "edited_text": "Original edit"},
            headers=self.write_headers(),
        )
        self.assertEqual(original.status, 200, original.body[:1000])
        invalid = (
            ("speak", {"text": "spoof"}),
            ("record", {"session_id": "", "event_id": "sync-http-spoof", "kind": "orchestrator",
                        "text": "spoof", "_internal": True}),
            ("submit", {"message_id": "sync-http-change-message", "record_ids": ["sync-http-change-record"],
                         "edited_text": "Changed retry"}),
            ("start", {}),
        )
        for action, body in invalid:
            with self.subTest(action=action):
                response = self.backend.request(
                    "POST", f"/api/voice/{action}",
                    body={"agent": self.lead_id, **body}, headers=self.write_headers(),
                )
                self.assertEqual(response.status, 400, response.body[:1000])

    def voice(self, action: str, **body: object) -> dict[str, object]:
        response = self.backend.request(
            "POST", f"/api/voice/{action}",
            body={"agent": self.lead_id, **body}, headers=self.write_headers(),
        )
        self.assertEqual(response.status, 200, response.body[:1000])
        return response.json()


if __name__ == "__main__":
    unittest.main(verbosity=2)
