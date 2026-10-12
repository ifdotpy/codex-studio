#!/usr/bin/env python3
"""Replay recorded provider and supervisor frames through a disposable backend."""

from __future__ import annotations

import copy
import json
import os
from pathlib import Path
import socket
import sys
import unittest
from urllib.parse import urlencode

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))

from test_isolation import isolate_supervisor_environment

isolate_supervisor_environment()

from http_backend_harness import HTTP_WAIT_GUARD_SECONDS, start_contract_backend
from server_test_waits import wait_for_condition


FIXTURES = Path(__file__).parent / "fixtures" / "provider-replay"
PROTOCOL_FIXTURE = FIXTURES / "supervisor-v1-http.json"


def assert_frame_matches(actual: object, expected: object, state: Path) -> None:
    """Compare recorded frame fields recursively; fixture dictionaries are projections."""
    if isinstance(expected, dict):
        if not isinstance(actual, dict):
            raise AssertionError(f"expected frame object, got {actual!r}")
        for key, value in expected.items():
            assert_frame_matches(actual.get(key), value, state)
    elif isinstance(expected, list):
        if not isinstance(actual, list) or len(actual) != len(expected):
            raise AssertionError(f"expected frame list {expected!r}, got {actual!r}")
        for actual_item, expected_item in zip(actual, expected):
            assert_frame_matches(actual_item, expected_item, state)
    else:
        resolved = str(state) if expected == "$state" else expected
        if actual != resolved:
            raise AssertionError(f"expected frame field {resolved!r}, got {actual!r}")


def substitute(value: object, state: Path) -> object:
    if isinstance(value, str):
        return {"$state": str(state), "$python": os.path.realpath(sys.executable),
                "$cwd": str(state.parent / "workspace"), "$path": os.environ.get("PATH", "")}.get(value, value)
    if isinstance(value, list):
        return [substitute(item, state) for item in value]
    if isinstance(value, dict):
        return {key: substitute(item, state) for key, item in value.items()}
    return value


def supervisor_exchange(
    state: Path,
    handshake: list[dict[str, object]],
    frames: list[dict[str, object]],
    *,
    altered_health_expectation: bool = False,
) -> list[dict[str, object]]:
    """Replay handshake and request/event frames over one supervisor connection."""
    if os.name == "nt":
        raise unittest.SkipTest("The named-pipe transport is recorded but this checker currently uses Unix sockets")
    client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    client.settimeout(HTTP_WAIT_GUARD_SECONDS)
    client.connect(str(state / "supervisor.sock"))
    try:
        with client.makefile("rwb") as stream:
            observed: list[dict[str, object]] = []
            for item in [*handshake, *frames]:
                direction = item.get("direction")
                if direction == "client":
                    request = substitute(item["frame"], state)
                    stream.write((json.dumps(request, separators=(",", ":")) + "\n").encode())
                    stream.flush()
                elif direction == "server":
                    line = stream.readline()
                    if not line:
                        raise AssertionError("supervisor closed before its recorded response frame")
                    response = json.loads(line)
                    assert_frame_matches(response, item["frame"], state)
                    observed.append(response)
                elif "request" in item and "events" not in item:
                    request = substitute(item["request"], state)
                    request_id = request.get("requestId", 1)
                    request["requestId"] = request_id
                    expected = copy.deepcopy(item["expected"])
                    expected["requestId"] = request_id
                    if altered_health_expectation and request.get("action") == "health":
                        expected["result"]["protocol"] = 999
                    stream.write((json.dumps(request, separators=(",", ":")) + "\n").encode())
                    stream.flush()
                    response = json.loads(stream.readline())
                    assert_frame_matches(response, expected, state)
                    observed.append(response)
                elif "events" in item:
                    pending = {frame["eventKind"]: frame for frame in item["events"]}
                    cursor = 0
                    next_request = substitute(item["request"], state)
                    while pending:
                        response_id = len(observed) + 1

                        def receive_event() -> dict[str, object] | None:
                            # Supervisor.handle(next) returns event=null without
                            # waiting; retry this cursor under the shared guard.
                            nonlocal response_id
                            current = {**next_request, "cursor": cursor, "requestId": response_id}
                            stream.write((json.dumps(current, separators=(",", ":")) + "\n").encode())
                            stream.flush()
                            response_id += 1
                            value = json.loads(stream.readline())
                            if value.get("error"):
                                raise AssertionError(f"supervisor rejected next frame: {value['error']}")
                            event = (value.get("result") or {}).get("event")
                            return value if isinstance(event, dict) else None

                        response = wait_for_condition(
                            receive_event,
                            f"supervisor next event for cursor {cursor}",
                            timeout_exception=TimeoutError,
                        )
                        event = response["result"]["event"]
                        kind = event.get("kind")
                        recorded = pending.pop(kind, None)
                        if recorded is None:
                            raise AssertionError(f"unexpected supervisor event kind: {kind!r}")
                        expected = copy.deepcopy(recorded["expected"])
                        expected["requestId"] = response["requestId"]
                        assert_frame_matches(response, expected, state)
                        cursor = event["sequence"]
                        observed.append(response)
                else:
                    raise AssertionError(f"unknown supervisor frame direction: {direction!r}")
            return observed
    finally:
        client.close()


class ProtocolFixturesContract(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.protocol = json.loads(PROTOCOL_FIXTURE.read_text(encoding="utf-8"))
        cls.provider = json.loads((FIXTURES / cls.protocol["providerFixture"]).read_text(encoding="utf-8"))
        cls.backend = start_contract_backend(
            cls,
            cls.protocol["providerFixture"],
            supervisor_protocol=True,
        )

    def test_supervisor_and_provider_fixtures_match_http_observations(self) -> None:
        backend = self.backend
        headers = {
            "Origin": backend.origin or "",
            "X-Canvas-Token": backend.session["token"],
            "X-Canvas-Workspace": backend.identity["workspaceId"],
            "X-Studio-API-Schema": backend.schema_hash,
        }
        lead = backend.request(
            "POST", "/api/leads",
            body={"cwd": str(backend.cwd), "workspaceMode": "worktree"}, headers=headers,
        )
        self.assertEqual(lead.status, 200, lead.body[:1000])
        lead_id = lead.json()["id"]
        message_id = "protocol-fixture-message"
        sent = backend.request(
            "POST", "/api/messages",
            body={"id": message_id, "room": lead_id, "text": "Run the recorded protocol frames.",
                  "delivery": "queue", "assets": []},
            headers=headers,
        )
        self.assertEqual(sent.status, 200, sent.body[:1000])
        self.assertEqual(sent.json()["id"], message_id)
        transcript_path = "/api/transcript?" + urlencode({"id": lead_id})
        transcript: dict[str, object] = {}

        def completed() -> bool:
            nonlocal transcript
            response = backend.request("GET", transcript_path)
            self.assertEqual(response.status, 200, response.body[:1000])
            transcript = response.json()
            return self.provider["assert"]["expectedOutput"] in json.dumps(transcript)

        backend.wait_until(completed, "recorded provider response visible through HTTP")
        backend.wait_until(
            lambda: (backend.state / "supervisor.sock").exists(),
            "supervisor socket created for the provider process",
        )
        handshake = self.protocol["supervisorFrames"][:2]
        supervisor_exchange(
            backend.state,
            handshake,
            self.protocol["supervisorFrames"][2:] + self.protocol["processScenario"],
        )
        supervisor_exchange(backend.state, self.protocol["supervisorFailureFrames"], [])
        self.assertIn(self.provider["assert"]["expectedOutput"], json.dumps(transcript))
        requests = backend.received_provider_frames()
        observed_methods = [frame.get("method") for frame in requests]
        previous = -1
        for method in self.provider["assert"]["providerRequestMethods"]:
            self.assertIn(method, observed_methods)
            position = observed_methods.index(method)
            self.assertGreater(position, previous, observed_methods)
            previous = position
        history = backend.request("GET", "/api/messages?room=" + lead_id)
        self.assertEqual(history.status, 200, history.body[:1000])
        self.assertTrue(any(item["id"] == message_id for item in history.json()))
        backend.stop()
        backend.ready_samples.append(backend.start())
        recovered = backend.request("GET", transcript_path)
        self.assertEqual(recovered.status, 200, recovered.body[:1000])
        self.assertIn(self.provider["assert"]["expectedOutput"], json.dumps(recovered.json()))

    def test_checker_rejects_an_altered_recorded_expectation(self) -> None:
        altered = copy.deepcopy(self.protocol["processScenario"][1:2])
        with self.assertRaises(AssertionError):
            supervisor_exchange(
                self.backend.state,
                self.protocol["supervisorFrames"][:2],
                altered,
                altered_health_expectation=True,
            )


if __name__ == "__main__":
    unittest.main(verbosity=2)
