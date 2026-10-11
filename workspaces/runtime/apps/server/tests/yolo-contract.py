#!/usr/bin/env python3
"""Conversation YOLO settings observed through HTTP and provider protocol."""

from __future__ import annotations

import json
import time
import unittest
from urllib.parse import urlencode

from test_isolation import isolate_supervisor_environment

isolate_supervisor_environment()

from http_backend_harness import start_contract_backend


class YoloHTTPContract(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.backend = start_contract_backend(cls)

    def headers(self) -> dict[str, str]:
        return {
            "Origin": self.backend.origin or "",
            "X-Canvas-Token": self.backend.session["token"],
            "X-Canvas-Workspace": self.backend.identity["workspaceId"],
            "X-Studio-API-Schema": self.backend.schema_hash,
        }

    def create_lead(self, identifier: str, *, yolo_mode: bool = True) -> dict[str, object]:
        response = self.backend.request(
            "POST", "/api/leads",
            body={"id": identifier, "cwd": str(self.backend.cwd),
                  "workspaceMode": "worktree", "yolo_mode": yolo_mode},
            headers=self.headers(),
        )
        self.assertEqual(response.status, 200, response.body[:1000])
        return response.json()

    def create_worker(self, lead_id: str, identifier: str) -> dict[str, object]:
        response = self.backend.request(
            "POST", "/api/agents",
            body={"id": identifier, "parent": lead_id, "prompt": "Complete the fixture task.",
                  "name": "HTTP contract worker", "role": "reviewer"},
            headers=self.headers(),
        )
        self.assertEqual(response.status, 200, response.body[:1000])
        return response.json()

    def entity(self, identifier: str) -> dict[str, object] | None:
        for document in self.state_documents():
            if document["id"] == f"entity:agent:{identifier}":
                return json.loads(document["payload"])["value"]
        return None

    def state_documents(self) -> list[dict[str, object]]:
        response = self.backend.request(
            "GET", "/api/sync/pull?" + urlencode({
                "scope": "state:entities:v1", "after": 0, "limit": 500,
            }),
        )
        self.assertEqual(response.status, 200, response.body[:1000])
        return response.json()["documents"]

    def wait_for_idle_entities(self, *identifiers: str) -> None:
        deadline = time.monotonic() + 3.0
        last_state: object = None
        while time.monotonic() < deadline:
            documents = self.state_documents()
            agents = {
                document["id"].removeprefix("entity:agent:"): json.loads(document["payload"])["value"]
                for document in documents if document["id"].startswith("entity:agent:")
            }
            states = {identifier: agents.get(identifier) for identifier in identifiers}
            root = states[identifiers[0]] if identifiers else None
            root_id = root.get("rootId") if isinstance(root, dict) else None
            team_ids = {
                identifier for identifier, state in agents.items()
                if isinstance(state, dict) and state.get("rootId") == root_id
            }
            busy_agents = {
                identifier: state for identifier, state in agents.items()
                if identifier in team_ids and isinstance(state, dict)
                and (state.get("inFlight") or state.get("workspaceOperation")
                     or state.get("status") in {"running", "starting", "approval"})
            }
            busy_events = [
                document["id"] for document in documents
                if document["id"].startswith("entity:event:")
                and (event := json.loads(document["payload"])["value"]).get("agent") in team_ids
                and event.get("status") in {"pending", "reserved", "dispatching"}
            ]
            last_state = {"states": states, "busyAgents": busy_agents, "busyEvents": busy_events}
            if (all(state is not None for state in states.values())
                    and not busy_agents and not busy_events):
                return
        self.fail(f"Team records did not settle through HTTP: {last_state!r}")

    def send_message(self, agent_id: str, message_id: str, text: str) -> list[dict[str, object]]:
        frames_before = self.backend.received_provider_frames()
        response = self.backend.request(
            "POST", "/api/messages",
            body={"id": message_id, "room": agent_id, "text": text,
                  "delivery": "queue", "assets": []},
            headers=self.headers(),
        )
        self.assertEqual(response.status, 200, response.body[:1000])
        deadline = time.monotonic() + 3.0
        transcript_path = "/api/transcript?" + urlencode({"id": agent_id})
        transcript: object = None
        while time.monotonic() < deadline:
            frames = self.backend.received_provider_frames()
            transcript_response = self.backend.request("GET", transcript_path, timeout=1.0)
            self.assertEqual(transcript_response.status, 200, transcript_response.body[:1000])
            transcript = transcript_response.json()
            current = self.entity(agent_id)
            starts = [frame for frame in frames[len(frames_before):]
                      if frame.get("method") == "turn/start"
                      and frame.get("params", {}).get("clientUserMessageId") == message_id]
            if (starts and current is not None and not current.get("inFlight")
                    and "Recovered output." in json.dumps(transcript)):
                return starts
        self.fail(f"The provider turn did not settle through HTTP: {transcript!r}")

    def test_lead_creation_replay_and_invalid_mode_are_http_contracts(self) -> None:
        identifier = "10000000-0000-4000-8000-000000000201"
        first = self.create_lead(identifier, yolo_mode=False)
        replay = self.create_lead(identifier, yolo_mode=False)
        self.assertEqual(replay["id"], first["id"])
        self.assertIs(first["yoloMode"], False)

        changed = self.backend.request(
            "POST", "/api/leads",
            body={"id": identifier, "cwd": str(self.backend.cwd),
                  "workspaceMode": "worktree", "yolo_mode": True},
            headers=self.headers(),
        )
        self.assertEqual(changed.status, 400, changed.body[:1000])
        invalid = self.backend.request(
            "POST", "/api/conversation",
            body={"id": identifier, "yolo_mode": "false"}, headers=self.headers(),
        )
        self.assertEqual(invalid.status, 400, invalid.body[:1000])
        self.assertIs(self.entity(identifier)["yoloMode"], False)

    def test_yolo_default_inherits_to_worker_and_selects_provider_permissions(self) -> None:
        lead_id = "10000000-0000-4000-8000-000000000202"
        worker_id = "10000000-0000-4000-8000-000000000203"
        lead = self.create_lead(lead_id)
        self.assertIs(lead["yoloMode"], True)
        worker = self.create_worker(lead_id, worker_id)
        self.assertIs(worker["yoloMode"], True)
        self.assertIs(self.entity(worker_id)["yoloMode"], True)
        starts = self.send_message(
            worker_id, "10000000-0000-4000-8000-000000000211",
            "Use the inherited team permissions.",
        )
        self.assertEqual(len(starts), 1, starts)
        turn = starts[0]
        self.assertEqual(turn["params"]["approvalPolicy"], "never", turn)
        self.assertEqual(turn["params"]["sandboxPolicy"], {"type": "dangerFullAccess"}, turn)

        rejected_id = "10000000-0000-4000-8000-000000000204"
        rejected = self.backend.request(
            "POST", "/api/agents",
            body={"id": rejected_id, "parent": lead_id, "prompt": "Invalid override",
                  "yolo_mode": False}, headers=self.headers(),
        )
        self.assertEqual(rejected.status, 400, rejected.body[:1000])
        self.assertIsNone(self.entity(rejected_id))

    def test_turns_after_yolo_is_disabled_use_restricted_provider_permissions(self) -> None:
        lead_id = "10000000-0000-4000-8000-000000000205"
        worker_id = "10000000-0000-4000-8000-000000000206"
        self.create_lead(lead_id)
        self.create_worker(lead_id, worker_id)
        self.send_message(worker_id, "10000000-0000-4000-8000-000000000207",
                          "Finish the worker's initial turn.")
        self.wait_for_idle_entities(lead_id, worker_id)

        changed = self.backend.request(
            "POST", "/api/conversation", body={"id": lead_id, "yolo_mode": False},
            headers=self.headers(),
        )
        self.assertEqual(changed.status, 200, changed.body[:1000])
        self.assertIs(changed.json()["yoloMode"], False)
        self.assertIs(self.entity(worker_id)["yoloMode"], False)

        starts = self.send_message(worker_id, "10000000-0000-4000-8000-000000000208",
                                   "Use the changed team permissions.")
        turn = next(frame for frame in starts if frame.get("params", {}).get("input"))
        self.assertEqual(turn["params"]["approvalPolicy"], "on-request", turn)
        self.assertEqual(turn["params"]["sandboxPolicy"]["type"], "readOnly", turn)


class YoloRestartHTTPContract(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.backend = start_contract_backend(cls)

    def headers(self) -> dict[str, str]:
        return {
            "Origin": self.backend.origin or "",
            "X-Canvas-Token": self.backend.session["token"],
            "X-Canvas-Workspace": self.backend.identity["workspaceId"],
            "X-Studio-API-Schema": self.backend.schema_hash,
        }

    def test_disabled_yolo_mode_is_served_after_backend_restart(self) -> None:
        identifier = "10000000-0000-4000-8000-000000000209"
        created = self.backend.request(
            "POST", "/api/leads", body={"id": identifier, "cwd": str(self.backend.cwd),
            "workspaceMode": "worktree", "yolo_mode": False}, headers=self.headers(),
        )
        self.assertEqual(created.status, 200, created.body[:1000])
        self.backend.stop()
        self.backend.start()

        state = self.backend.request(
            "GET", "/api/sync/pull?" + urlencode({
                "scope": "state:entities:v1", "after": 0, "limit": 500,
            }),
        )
        self.assertEqual(state.status, 200, state.body[:1000])
        entity = next(json.loads(document["payload"])["value"]
                      for document in state.json()["documents"]
                      if document["id"] == f"entity:agent:{identifier}")
        self.assertIs(entity["yoloMode"], False)

        before = self.backend.received_provider_frames()
        sent = self.backend.request(
            "POST", "/api/messages",
            body={"id": "10000000-0000-4000-8000-000000000210", "room": identifier,
                  "text": "Use the persisted permission setting.",
                  "delivery": "queue", "assets": []},
            headers=self.headers(),
        )
        self.assertEqual(sent.status, 200, sent.body[:1000])
        deadline = time.monotonic() + 3.0
        starts: list[dict[str, object]] = []
        while time.monotonic() < deadline:
            frames = self.backend.received_provider_frames()
            starts = [frame for frame in frames[len(before):]
                      if frame.get("method") == "turn/start"]
            transcript = self.backend.request(
                "GET", "/api/transcript?" + urlencode({"id": identifier}), timeout=1.0,
            )
            self.assertEqual(transcript.status, 200, transcript.body[:1000])
            if starts and "Recovered output." in json.dumps(transcript.json()):
                break
        else:
            self.fail("The post-restart provider turn did not complete")
        turn = next(frame for frame in starts if frame.get("params", {}).get("input"))
        self.assertEqual(turn["params"]["approvalPolicy"], "on-request", turn)
        self.assertEqual(turn["params"]["sandboxPolicy"]["type"], "workspaceWrite", turn)


if __name__ == "__main__":
    unittest.main(verbosity=2)
