#!/usr/bin/env python3
"""Conversation YOLO settings observed through HTTP and provider protocol."""

from __future__ import annotations

import json
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
        last_state: object = None
        def idle_state() -> dict[str, object] | None:
            nonlocal last_state
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
                return last_state
            return None
        try:
            self.backend.wait_until(idle_state, "team records to become idle")
        except TimeoutError:
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
        def provider_received_message() -> list[dict[str, object]] | None:
            frames = self.backend.received_provider_frames()
            starts = [frame for frame in frames[len(frames_before):]
                      if frame.get("method") == "turn/start"
                      and (frame.get("params", {}).get("clientUserMessageId") == message_id
                           or text in json.dumps(frame.get("params", {}).get("input", [])))]
            if starts:
                return starts
            return None
        try:
            return self.backend.wait_until(
                provider_received_message, "provider turn to receive the message through HTTP",
            )
        except TimeoutError:
            self.fail(
                f"The provider did not receive message {message_id}; "
                f"provider_starts={self.backend.provider_start_times()!r}; "
                f"frames={self.backend.received_provider_frames()!r}; "
                f"stderr={self.backend.stderr[-4000:]!r}"
            )

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
        self.assertIs(self.entity(lead_id)["yoloMode"], True)
        lead_starts = self.send_message(
            lead_id, "10000000-0000-4000-8000-000000000212",
            "Use the default lead permissions.",
        )
        self.assertEqual(len(lead_starts), 1, lead_starts)
        lead_turn = lead_starts[0]
        self.assertEqual(lead_turn["params"]["approvalPolicy"], "never", lead_turn)
        self.assertEqual(lead_turn["params"]["sandboxPolicy"], {"type": "dangerFullAccess"}, lead_turn)
        worker = self.create_worker(lead_id, worker_id)
        self.assertIs(worker["yoloMode"], True)
        self.assertIs(self.entity(worker_id)["yoloMode"], True)
        self.wait_for_idle_entities(lead_id, worker_id)
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
        self.wait_for_idle_entities(lead_id, worker_id)
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

        def lead_idle() -> bool:
            current = self.backend.request(
                "GET", "/api/sync/pull?" + urlencode({
                    "scope": "state:entities:v1", "after": 0, "limit": 500,
                }),
            )
            self.assertEqual(current.status, 200, current.body[:1000])
            for document in current.json()["documents"]:
                if document["id"] == f"entity:agent:{identifier}":
                    agent = json.loads(document["payload"])["value"]
                    return (not agent.get("inFlight") and agent.get("status")
                            not in {"starting", "running"})
            return False

        self.backend.wait_until(lead_idle, "post-restart lead turn to settle")

        before = self.backend.received_provider_frames()
        message_id = "10000000-0000-4000-8000-000000000210"
        sent = self.backend.request(
            "POST", "/api/messages",
            body={"id": message_id, "room": identifier,
                  "text": "Use the persisted permission setting.",
                  "delivery": "queue", "assets": []},
            headers=self.headers(),
        )
        self.assertEqual(sent.status, 200, sent.body[:1000])
        starts: list[dict[str, object]] = []
        def provider_received_message() -> list[dict[str, object]] | None:
            nonlocal starts
            frames = self.backend.received_provider_frames()
            starts = [frame for frame in frames[len(before):]
                      if frame.get("method") == "turn/start"
                      and frame.get("params", {}).get("clientUserMessageId") == message_id]
            if starts:
                return starts
            return None
        try:
            self.backend.wait_until(provider_received_message, "post-restart provider turn input")
        except TimeoutError:
            self.fail(
                f"The post-restart provider did not receive {message_id}; "
                f"provider_starts={self.backend.provider_start_times()!r}; "
                f"frames={self.backend.received_provider_frames()!r}; "
                f"stderr={self.backend.stderr[-4000:]!r}"
            )
        turn = next(frame for frame in starts if frame.get("params", {}).get("input"))
        self.assertEqual(turn["params"]["approvalPolicy"], "on-request", turn)
        self.assertEqual(turn["params"]["sandboxPolicy"]["type"], "workspaceWrite", turn)


class YoloWorkerMessageOrderingCase(unittest.TestCase):
    hold_worker_thread_start = False

    @classmethod
    def setUpClass(cls) -> None:
        cls.backend = start_contract_backend(
            cls,
            hold_provider_method="thread/start" if cls.hold_worker_thread_start else None,
            hold_provider_occurrence=2,
        )

    def headers(self) -> dict[str, str]:
        return {
            "Origin": self.backend.origin or "",
            "X-Canvas-Token": self.backend.session["token"],
            "X-Canvas-Workspace": self.backend.identity["workspaceId"],
            "X-Studio-API-Schema": self.backend.schema_hash,
        }

    def create_lead(self, identifier: str) -> None:
        response = self.backend.request(
            "POST", "/api/leads",
            body={"id": identifier, "cwd": str(self.backend.cwd),
                  "workspaceMode": "worktree", "yolo_mode": True},
            headers=self.headers(),
        )
        self.assertEqual(response.status, 200, response.body[:1000])

    def create_worker(self, lead_id: str, identifier: str) -> None:
        response = self.backend.request(
            "POST", "/api/agents",
            body={"id": identifier, "parent": lead_id, "prompt": "Complete the fixture task.",
                  "name": "HTTP ordering worker", "role": "reviewer"},
            headers=self.headers(),
        )
        self.assertEqual(response.status, 200, response.body[:1000])

    def entity(self, identifier: str) -> dict[str, object] | None:
        response = self.backend.request("GET", "/api/sync/pull?" + urlencode({
            "scope": "state:entities:v1", "after": 0, "limit": 500,
        }))
        self.assertEqual(response.status, 200, response.body[:1000])
        return next((json.loads(row["payload"])["value"]
                     for row in response.json()["documents"]
                     if row["id"] == f"entity:agent:{identifier}"), None)

    def post_message(self, agent_id: str, message_id: str, message: str) -> None:
        response = self.backend.request(
            "POST", "/api/messages",
            body={"id": message_id, "room": agent_id, "text": message,
                  "delivery": "queue", "assets": []},
            headers=self.headers(),
        )
        self.assertEqual(response.status, 200, response.body[:1000])

    def wait_for_message_turn(self, agent_id: str, message_id: str,
                              message: str) -> dict[str, object]:
        observed: dict[str, object] | None = None

        def provider_received_message() -> dict[str, object] | None:
            nonlocal observed
            frames = self.backend.received_provider_frames()
            observed = next((frame for frame in frames
                if frame.get("method") == "turn/start"
                and (frame.get("params", {}).get("clientUserMessageId") == message_id
                     or message in json.dumps(frame.get("params", {}).get("input", [])))), None)
            return observed

        try:
            return self.backend.wait_until(
                provider_received_message, "provider turn carrying the queued message",
            )
        except TimeoutError:
            self.fail(
                f"No provider turn carried {message_id}; last turn={observed!r}; "
                f"provider_starts={self.backend.provider_start_times()!r}; "
                f"frames={self.backend.received_provider_frames()!r}; "
                f"stderr={self.backend.stderr[-4000:]!r}"
            )

    def assert_inherited_yolo_permissions(self, turn: dict[str, object]) -> None:
        self.assertEqual(turn["params"]["approvalPolicy"], "never", turn)
        self.assertEqual(turn["params"]["sandboxPolicy"], {"type": "dangerFullAccess"}, turn)

    def prepare_team(self, lead_id: str, worker_id: str,
                     lead_message_id: str) -> tuple[str, str]:
        self.create_lead(lead_id)
        lead_prompt = "Use the default lead permissions."
        self.post_message(lead_id, lead_message_id, lead_prompt)
        self.wait_for_message_turn(lead_id, lead_message_id, lead_prompt)
        self.create_worker(lead_id, worker_id)
        return lead_id, worker_id


class YoloWorkerMessageOrderingHTTPContract(YoloWorkerMessageOrderingCase):
    hold_worker_thread_start = True

    def test_message_ordering_before_and_after_worker_first_turn_is_deterministic(self) -> None:
        with self.subTest(order="message queued before worker first turn starts"):
            _lead_id, worker_id = self.prepare_team(
                "10000000-0000-4000-8000-000000000301",
                "10000000-0000-4000-8000-000000000302",
                "10000000-0000-4000-8000-000000000303",
            )
            self.backend.wait_until(self.backend.provider_gate_started,
                                    "worker thread/start provider gate")
            message_id = "10000000-0000-4000-8000-000000000311"
            message = "Use the inherited team permissions before the first turn starts."
            self.post_message(worker_id, message_id, message)
            self.backend.release_provider_gate()
            turn = self.wait_for_message_turn(worker_id, message_id, message)
            self.assert_inherited_yolo_permissions(turn)

        with self.subTest(order="message queued after worker first turn settles"):
            _lead_id, settled_worker_id = self.prepare_team(
                "10000000-0000-4000-8000-000000000321",
                "10000000-0000-4000-8000-000000000322",
                "10000000-0000-4000-8000-000000000323",
            )

            def worker_idle() -> bool:
                worker = self.entity(settled_worker_id)
                return bool(worker and not worker.get("inFlight")
                            and worker.get("status") not in {"starting", "running"})

            self.backend.wait_until(worker_idle, "worker first turn to settle")
            message_id = "10000000-0000-4000-8000-000000000324"
            message = "Use the inherited team permissions after the first turn settles."
            self.post_message(settled_worker_id, message_id, message)
            turn = self.wait_for_message_turn(settled_worker_id, message_id, message)
            self.assert_inherited_yolo_permissions(turn)


if __name__ == "__main__":
    unittest.main(verbosity=2)
