#!/usr/bin/env python3
"""Replay recorded provider and supervisor frames through a disposable backend."""

from __future__ import annotations

import copy
import json
import os
from pathlib import Path
import socket
import unittest
from urllib.parse import urlencode

from test_isolation import isolate_supervisor_environment

isolate_supervisor_environment()

from http_backend_harness import HTTP_WAIT_GUARD_SECONDS, start_contract_backend


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


def supervisor_exchange(state: Path, frames: list[dict[str, object]]) -> list[dict[str, object]]:
    if os.name == "nt":
        raise unittest.SkipTest("The named-pipe transport is recorded but this checker currently uses Unix sockets")
    client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    client.settimeout(HTTP_WAIT_GUARD_SECONDS)
    client.connect(str(state / "supervisor.sock"))
    try:
        with client.makefile("rwb") as stream:
            observed: list[dict[str, object]] = []
            for item in frames:
                direction = item["direction"]
                frame = item["frame"]
                if direction == "client":
                    stream.write((json.dumps(frame, separators=(",", ":")).replace(
                        "$state", str(state)) + "\n").encode())
                    stream.flush()
                elif direction == "server":
                    line = stream.readline()
                    if not line:
                        raise AssertionError("supervisor closed before its recorded response frame")
                    response = json.loads(line)
                    assert_frame_matches(response, frame, state)
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
        supervisor_exchange(backend.state, self.protocol["supervisorFrames"])
        supervisor_exchange(backend.state, self.protocol["supervisorFailureFrames"])
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
        altered = copy.deepcopy(self.protocol["supervisorFrames"])
        altered[1]["frame"]["ok"] = False
        with self.assertRaises(AssertionError):
            assert_frame_matches({"ok": True}, altered[1]["frame"], self.backend.state)


if __name__ == "__main__":
    unittest.main(verbosity=2)
