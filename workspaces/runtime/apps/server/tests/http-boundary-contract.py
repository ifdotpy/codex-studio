#!/usr/bin/env python3
"""Executable HTTP/provider contract for a disposable backend process."""

from __future__ import annotations

import json
import os
from pathlib import Path
import statistics
import time
from urllib.parse import urlencode
from urllib.request import Request, urlopen
from typing import Callable

from test_isolation import isolate_supervisor_environment

isolate_supervisor_environment()

from http_backend_harness import BackendProcess, HttpResult


def assert_status(response: HttpResult, expected: int) -> None:
    assert response.status == expected, (expected, response.status, response.body[:2000])


def message_receipt(value: dict[str, object]) -> dict[str, object]:
    """Select the stable user-message receipt fields from an HTTP response."""
    fields = ("id", "status", "room", "author", "text", "at", "deliveries")
    return {field: value[field] for field in fields if field in value}


def test_session_schema_identity_and_mismatching_write(backend: BackendProcess) -> str:
    response = backend.request("GET", "/api/session")
    assert_status(response, 200)
    session = response.json()
    assert isinstance(session.get("token"), str) and session["token"]
    schema_probe = backend.request(
        "GET", "/api/sync/identity", headers={"X-Studio-API-Schema": "probe"}
    )
    assert_status(schema_probe, 200)
    schema_hash = schema_probe.headers.get("x-studio-api-schema")
    assert schema_hash and len(schema_hash) == 64
    draft = {"rows": [{"newDocumentState": {"id": "device:session", "payload":
        json.dumps({"device": "device", "session": "session", "text": "mismatch"})}}]}
    rejected = backend.request(
        "POST", "/api/sync/drafts", body=draft,
        headers={"Origin": backend.origin or "", "X-Canvas-Token": session["token"],
                 "X-Studio-API-Schema": "0" * 64},
    )
    assert_status(rejected, 426)
    assert rejected.headers.get("x-studio-api-schema-mismatch") == "1"
    assert rejected.headers.get("x-studio-api-schema") == schema_hash
    backend.session = session
    backend.schema_hash = schema_hash
    return schema_hash


def test_sync_v3_snapshot_delta_and_workspace_header(backend: BackendProcess) -> None:
    headers = {"X-Studio-API-Schema": backend.schema_hash}
    identity = backend.request("GET", "/api/sync/identity", headers=headers)
    assert_status(identity, 200)
    workspace = identity.json()["workspaceId"]
    backend.workspace_id = workspace
    protocol = backend.request("GET", "/api/sync/protocol", headers=headers)
    assert_status(protocol, 200)
    negotiation = protocol.json()
    assert negotiation["protocolVersion"] == 3
    assert negotiation["supportedVersions"] == [3]
    assert negotiation["pullEndpoint"] == "/api/sync/pull"
    first = backend.request("GET", "/api/sync/pull?scope=drafts&after=0")
    assert_status(first, 200)
    assert first.json()["documents"] == []
    checkpoint = first.json()["checkpoint"]["seq"]
    row = {"newDocumentState": {"id": "boundary-device:boundary-session",
            "payload": json.dumps({"device": "boundary-device", "session": "boundary-session",
                                   "text": "boundary draft"})}}
    common = {"Origin": backend.origin or "", "X-Canvas-Token": backend.session["token"],
              "X-Studio-API-Schema": backend.schema_hash}
    missing = backend.request("POST", "/api/sync/drafts", body={"rows": [row]}, headers=common)
    assert_status(missing, 409)
    written = backend.request(
        "POST", "/api/sync/drafts", body={"rows": [row]},
        headers={**common, "X-Canvas-Workspace": workspace},
    )
    assert_status(written, 200)
    delta = backend.request("GET", f"/api/sync/pull?scope=drafts&after={checkpoint}")
    assert_status(delta, 200)
    documents = delta.json()["documents"]
    assert len(documents) == 1 and documents[0]["id"] == row["newDocumentState"]["id"]
    assert json.loads(documents[0]["payload"])["text"] == "boundary draft"
    backend.draft_id = documents[0]["id"]


def test_sync_stream_initial_schema_resources_token_rates(backend: BackendProcess) -> None:
    query = urlencode({
        "protocol": "3",
        "apiSchema": backend.schema_hash,
        "resources": json.dumps([{"kind": "drafts"}], separators=(",", ":")),
    })
    request = Request(backend.origin + "/api/sync/stream?" + query)
    started = time.monotonic()
    with urlopen(request, timeout=2.0) as response:
        assert response.status == 200
        raw_socket = response.fp.raw._sock
        frames: list[dict[str, object]] = []
        current: dict[str, object] = {}
        data_lines: list[str] = []
        deadline = started + 2.0
        while len(frames) < 3 and time.monotonic() < deadline:
            raw_socket.settimeout(max(0.01, deadline - time.monotonic()))
            line = response.readline().decode("utf-8").rstrip("\r\n")
            if line.startswith("event: "):
                current = {"event": line.removeprefix("event: ")}
            elif line.startswith("data: "):
                data_lines.append(line.removeprefix("data: "))
            elif not line and current:
                current["data"] = json.loads("\n".join(data_lines))
                frames.append(current)
                current = {}
                data_lines = []
    events = [frame["event"] for frame in frames]
    assert events == ["api-schema", "resources", "token-rates"], frames


def test_work_receipt_replay_conflict_and_single_effect(backend: BackendProcess) -> None:
    headers = {"Origin": backend.origin or "", "X-Canvas-Token": backend.session["token"],
               "X-Studio-API-Schema": backend.schema_hash}
    room_id = "10000000-0000-4000-8000-000000000001"
    room = backend.request("POST", "/api/chats", body={"id": room_id,
        "name": "HTTP boundary room", "members": []}, headers=headers)
    assert_status(room, 200)
    assert room.json()["id"] == room_id, room.json()
    backend.room_id = room_id
    body = {"id": "10000000-0000-4000-8000-000000000002", "room": room_id,
            "text": "HTTP boundary receipt"}
    first = backend.request("POST", "/api/messages", body=body, headers=headers)
    assert_status(first, 200)
    assert first.json()["id"] == body["id"]
    replay = backend.request("POST", "/api/messages", body=body, headers=headers)
    assert_status(replay, 200)
    assert message_receipt(replay.json()) == message_receipt(first.json())
    changed = backend.request("POST", "/api/messages", body={**body, "text": "changed payload"}, headers=headers)
    assert_status(changed, 400)
    assert "different content" in changed.json()["error"]
    history = backend.request("GET", "/api/messages?room=" + room_id)
    assert_status(history, 200)
    matches = [item for item in history.json() if item["id"] == first.json()["id"]]
    assert len(matches) == 1
    backend.work_body = body
    backend.work_result = message_receipt(first.json())


def test_codex_provider_turn_uses_replay_executable(backend: BackendProcess) -> None:
    headers = {"Origin": backend.origin or "", "X-Canvas-Token": backend.session["token"],
               "X-Studio-API-Schema": backend.schema_hash}
    accounts = backend.request("GET", "/api/accounts")
    assert_status(accounts, 200)
    assert accounts.json()["accounts"][0]["status"] == "ready"
    lead = backend.request("POST", "/api/leads", body={"cwd": str(backend.cwd),
        "workspaceMode": "worktree"}, headers=headers)
    assert_status(lead, 200)
    backend.lead_id = lead.json()["id"]
    body = {"id": "http-contract-provider-message", "room": backend.lead_id,
            "text": "Reply with the fixture result.", "delivery": "queue", "assets": []}
    sent = backend.request("POST", "/api/messages", body=body, headers=headers)
    assert_status(sent, 200)
    assert sent.json()["id"] == body["id"]
    transcript_path = "/api/transcript?" + urlencode({"id": backend.lead_id})
    deadline = time.monotonic() + 2.5
    frames: list[dict[str, object]] = []
    transcript: dict[str, object] = {}
    while time.monotonic() < deadline:
        frames = backend.received_provider_frames()
        transcript_response = backend.request("GET", transcript_path, timeout=1.0)
        assert_status(transcript_response, 200)
        transcript = transcript_response.json()
        methods = [frame.get("method") for frame in frames]
        if "turn/start" in methods and "Recovered output." in json.dumps(transcript):
            break
    history = backend.request("GET", "/api/messages?room=" + backend.lead_id)
    assert_status(history, 200)
    assert any(item["id"] == body["id"] and item["text"] == body["text"]
               for item in history.json())
    methods = [frame.get("method") for frame in frames]
    assert "initialize" in methods and "thread/start" in methods and "turn/start" in methods, (
        methods, backend.stderr
    )
    assert any(frame.get("method") == "turn/start" and frame.get("params", {}).get("input")
               for frame in frames), frames
    assert "Recovered output." in json.dumps(transcript), transcript
    backend.provider_message = body


def test_restart_serves_sync_receipt_and_provider_facts(backend: BackendProcess) -> None:
    backend.stop()
    ready = backend.start()
    backend.ready_samples.append(ready)
    restarted_session = backend.request("GET", "/api/session")
    assert_status(restarted_session, 200)
    backend.session = restarted_session.json()
    pulled = backend.request("GET", "/api/sync/pull?scope=drafts&after=0")
    assert_status(pulled, 200)
    assert any(document["id"] == backend.draft_id for document in pulled.json()["documents"])
    identity = backend.request("GET", "/api/sync/identity",
                               headers={"X-Studio-API-Schema": backend.schema_hash})
    assert_status(identity, 200)
    assert identity.json()["workspaceId"] == backend.workspace_id
    protocol = backend.request("GET", "/api/sync/protocol",
                               headers={"X-Studio-API-Schema": backend.schema_hash})
    assert_status(protocol, 200)
    assert protocol.json()["protocolVersion"] == 3
    headers = {"Origin": backend.origin or "", "X-Canvas-Token": backend.session["token"],
               "X-Studio-API-Schema": backend.schema_hash}
    replay = backend.request("POST", "/api/messages", body=backend.work_body, headers=headers)
    assert_status(replay, 200)
    assert message_receipt(replay.json()) == backend.work_result
    history = backend.request("GET", "/api/messages?room=" + backend.room_id)
    assert_status(history, 200)
    assert any(item["id"] == backend.work_body["id"] for item in history.json())
    provider_history = backend.request("GET", "/api/messages?room=" + backend.lead_id)
    assert_status(provider_history, 200)
    assert any(item["id"] == backend.provider_message["id"] for item in provider_history.json())
    transcript = backend.request("GET", "/api/transcript?" + urlencode({"id": backend.lead_id}))
    assert_status(transcript, 200)
    assert "Recovered output." in json.dumps(transcript.json())
    methods = [frame.get("method") for frame in backend.received_provider_frames()]
    assert "turn/start" in methods


def run_contract(name: str, action: Callable[[], object]) -> float:
    started = time.monotonic()
    action()
    elapsed = time.monotonic() - started
    print(json.dumps({"contract": name, "seconds": round(elapsed, 4)}, sort_keys=True))
    return elapsed


def main() -> None:
    scratch_value = os.environ.get("TMPDIR")
    if not scratch_value:
        raise RuntimeError("The server runner must provide a memory-backed TMPDIR")
    backend = BackendProcess(Path(scratch_value))
    suite_started = time.monotonic()
    try:
        backend.create_codex_replay("restart-mid-turn-supervisor-replay.json")
        backend.ready_samples.append(backend.start())
        contracts = [
            ("session_schema_identity_and_mismatching_write",
             lambda: test_session_schema_identity_and_mismatching_write(backend)),
            ("sync_v3_snapshot_delta_and_workspace_header",
             lambda: test_sync_v3_snapshot_delta_and_workspace_header(backend)),
            ("sync_stream_initial_schema_resources_token_rates",
             lambda: test_sync_stream_initial_schema_resources_token_rates(backend)),
            ("message_receipt_replay_conflict_and_single_effect",
             lambda: test_work_receipt_replay_conflict_and_single_effect(backend)),
            ("codex_provider_turn_uses_replay_executable",
             lambda: test_codex_provider_turn_uses_replay_executable(backend)),
            ("restart_serves_sync_receipt_and_provider_facts",
             lambda: test_restart_serves_sync_receipt_and_provider_facts(backend)),
        ]
        durations = [run_contract(name, action) for name, action in contracts]
        readiness = backend.ready_samples
        print(json.dumps({
            "backendReadyMedianSeconds": round(statistics.median(readiness), 4),
            "backendReadyWorstSeconds": round(max(readiness), 4),
            "backendStarts": len(readiness),
            "perContractSeconds": dict(zip((name for name, _ in contracts),
                                            (round(value, 4) for value in durations))),
            "suiteSeconds": round(time.monotonic() - suite_started, 4),
            "backendProcesses": "one per suite; one sequential restart for durability contract",
        }, sort_keys=True))
    finally:
        backend.close()


if __name__ == "__main__":
    main()
