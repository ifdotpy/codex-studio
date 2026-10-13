#!/usr/bin/env python3
"""Executable HTTP/provider contracts for disposable backend processes."""

from __future__ import annotations

from test_isolation import isolate_supervisor_environment

_clock = __import__("time")
_module_import_started = _clock.monotonic()

import base64
import json
import os
from pathlib import Path
import statistics
import time
import unittest
from urllib.parse import urlencode
from urllib.request import Request, urlopen


isolate_supervisor_environment()
import sys
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from runtime_test_environment import make_runtime

from http_backend_harness import (
    HTTP_WAIT_GUARD_SECONDS,
    BackendProcess,
    HttpResult,
)
from sync_http_contract_support import prepare_sync_store_for_rust_contract

_module_import_finished = time.monotonic()


def assert_status(response: HttpResult, expected: int) -> None:
    assert response.status == expected, (expected, response.status, response.body[:2000])


def message_receipt(value: dict[str, object]) -> dict[str, object]:
    """Select stable user-message receipt fields from an HTTP response."""
    fields = ("id", "status", "room", "author", "text", "at", "deliveries")
    return {field: value[field] for field in fields if field in value}


def create_backend(test_case: type[unittest.TestCase]) -> BackendProcess:
    scratch_value = os.environ.get("TMPDIR")
    if not scratch_value:
        raise RuntimeError("The server runner must provide a memory-backed TMPDIR")
    phase_started = time.monotonic()
    backend = BackendProcess(Path(scratch_value))
    backend.class_setup_phases["harnessConstructionSeconds"] = round(
        time.monotonic() - phase_started, 4)
    test_case.addClassCleanup(record_ready_times, backend)
    test_case.addClassCleanup(backend.close)
    phase_started = time.monotonic()
    seed_catalog_state(backend)
    backend.class_setup_phases["fakeProviderSetupSeconds"] = round(
        time.monotonic() - phase_started, 4)
    phase_started = time.monotonic()
    backend.ready_samples.append(backend.start())
    backend.class_setup_phases["backendStartAndReadinessSeconds"] = round(
        time.monotonic() - phase_started, 4)
    return backend


def record_ready_times(backend: BackendProcess) -> None:
    samples = backend.ready_samples
    print(json.dumps({
        "classSetupSeconds": round(backend.class_setup_seconds or 0, 4),
        "classSetupPhases": backend.class_setup_phases,
        "backendReadyMedianSeconds": round(statistics.median(samples), 4) if samples else None,
        "backendReadyWorstSeconds": round(max(samples), 4) if samples else None,
        "backendStarts": len(samples),
        "backendStartPhases": backend.start_phases,
        "backendStopPhases": backend.stop_phases,
        "stateCleanupSeconds": round(backend.cleanup_seconds or 0, 4),
    }, sort_keys=True), flush=True)


def record_process_import_time() -> None:
    print(json.dumps({"pythonImportsSeconds": round(
        _module_import_finished - _module_import_started, 4)}, sort_keys=True), flush=True)


class TimedBoundaryTest(unittest.TestCase):
    backend: BackendProcess

    def setUp(self) -> None:
        self.test_started = time.monotonic()

    def tearDown(self) -> None:
        print(json.dumps({"contract": self.id(),
                          "seconds": round(time.monotonic() - self.test_started, 4)},
                         sort_keys=True), flush=True)


class BoundaryContractTests(TimedBoundaryTest):
    @classmethod
    def setUpClass(cls) -> None:
        started = time.monotonic()
        cls.backend = create_backend(cls)
        prepare_sync_store_for_rust_contract(cls.backend)
        cls.backend.class_setup_seconds = time.monotonic() - started

    def test_session_schema_identity_and_mismatching_write(self) -> None:
        backend = self.backend
        response = backend.request("GET", "/api/session")
        assert_status(response, 200)
        session = response.json()
        self.assertIsInstance(session.get("token"), str)
        self.assertTrue(session["token"])
        schema_probe = backend.request(
            "GET", "/api/sync/identity", headers={"X-Studio-API-Schema": "probe"}
        )
        assert_status(schema_probe, 200)
        schema_hash = schema_probe.headers.get("x-studio-api-schema")
        self.assertIsNotNone(schema_hash)
        self.assertEqual(len(schema_hash or ""), 64)
        draft = {"rows": [{"newDocumentState": {"id": "session-device:session-id",
                 "payload": json.dumps({"device": "session-device", "session": "session-id",
                                        "text": "mismatch"})}}]}
        rejected = backend.request(
            "POST", "/api/sync/drafts", body=draft,
            headers={"Origin": backend.origin or "", "X-Canvas-Token": session["token"],
                     "X-Studio-API-Schema": "0" * 64},
        )
        assert_status(rejected, 426)
        self.assertEqual(rejected.headers.get("x-studio-api-schema-mismatch"), "1")
        self.assertEqual(rejected.headers.get("x-studio-api-schema"), schema_hash)

    def test_sync_v3_snapshot_delta_and_workspace_header(self) -> None:
        backend = self.backend
        headers = {"X-Studio-API-Schema": backend.schema_hash}
        identity = backend.request("GET", "/api/sync/identity", headers=headers)
        assert_status(identity, 200)
        workspace = identity.json()["workspaceId"]
        protocol = backend.request("GET", "/api/sync/protocol", headers=headers)
        assert_status(protocol, 200)
        self.assertEqual(protocol.json()["protocolVersion"], 3)
        self.assertEqual(protocol.json()["supportedVersions"], [3])
        self.assertEqual(protocol.json()["pullEndpoint"], "/api/sync/pull")
        snapshot = backend.request("GET", "/api/sync/pull?scope=drafts&after=0")
        assert_status(snapshot, 200)
        self.assertEqual(snapshot.json()["documents"], [])
        checkpoint = snapshot.json()["checkpoint"]["seq"]
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
        self.assertEqual(len(documents), 1)
        self.assertEqual(documents[0]["id"], row["newDocumentState"]["id"])
        self.assertEqual(json.loads(documents[0]["payload"])["text"], "boundary draft")

    def test_sync_stream_initial_schema_resources_token_rates(self) -> None:
        backend = self.backend
        query = urlencode({
            "protocol": "3",
            "apiSchema": backend.schema_hash,
            "resources": json.dumps([{"kind": "drafts"}], separators=(",", ":")),
        })
        started = time.monotonic()
        with urlopen(Request(backend.origin + "/api/sync/stream?" + query),
                     timeout=HTTP_WAIT_GUARD_SECONDS) as response:
            self.assertEqual(response.status, 200)
            raw_socket = response.fp.raw._sock
            frames: list[dict[str, object]] = []
            current: dict[str, object] = {}
            data_lines: list[str] = []
            deadline = started + HTTP_WAIT_GUARD_SECONDS
            expected = 2 if os.environ.get("STUDIO_HTTP_BACKEND_COMMAND") else 3
            while len(frames) < expected:
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
        expected_events = ["api-schema", "resources"]
        if not os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"):
            expected_events.append("token-rates")
        self.assertEqual([frame["event"] for frame in frames], expected_events, frames)

    def test_message_receipt_replay_conflict_and_single_effect(self) -> None:
        backend = self.backend
        headers = {"Origin": backend.origin or "", "X-Canvas-Token": backend.session["token"],
                   "X-Studio-API-Schema": backend.schema_hash}
        room_id = "10000000-0000-4000-8000-000000000001"
        room = backend.request("POST", "/api/chats", body={"id": room_id,
            "name": "HTTP boundary room", "members": []}, headers=headers)
        assert_status(room, 200)
        body = {"id": "10000000-0000-4000-8000-000000000002", "room": room_id,
                "text": "HTTP boundary receipt"}
        first = backend.request("POST", "/api/messages", body=body, headers=headers)
        assert_status(first, 200)
        self.assertEqual(first.json()["id"], body["id"])
        replay = backend.request("POST", "/api/messages", body=body, headers=headers)
        assert_status(replay, 200)
        self.assertEqual(message_receipt(replay.json()), message_receipt(first.json()))
        changed = backend.request("POST", "/api/messages",
                                  body={**body, "text": "changed payload"}, headers=headers)
        assert_status(changed, 400)
        self.assertIn("different content", changed.json()["error"])
        history = backend.request("GET", "/api/messages?room=" + room_id)
        assert_status(history, 200)
        self.assertEqual(sum(item["id"] == first.json()["id"] for item in history.json()), 1)

    def test_codex_provider_turn_uses_replay_executable(self) -> None:
        backend = self.backend
        headers = {"Origin": backend.origin or "", "X-Canvas-Token": backend.session["token"],
                   "X-Studio-API-Schema": backend.schema_hash}
        accounts = backend.request("GET", "/api/accounts")
        assert_status(accounts, 200)
        self.assertEqual(accounts.json()["accounts"][0]["status"], "ready")
        lead = backend.request("POST", "/api/leads", body={"cwd": str(backend.cwd),
            "workspaceMode": "worktree"}, headers=headers)
        assert_status(lead, 200)
        lead_id = lead.json()["id"]
        body = {"id": "20000000-0000-4000-8000-000000000003", "room": lead_id,
                "text": "Reply with the fixture result.", "delivery": "queue", "assets": []}
        provider_request_started = time.monotonic()
        sent = backend.request("POST", "/api/messages", body=body, headers=headers)
        assert_status(sent, 200)
        self.assertEqual(sent.json()["id"], body["id"])
        transcript_path = "/api/transcript?" + urlencode({"id": lead_id})
        frames: list[dict[str, object]] = []
        transcript: dict[str, object] = {}
        def turn_completed() -> bool:
            nonlocal frames, transcript
            frames = backend.received_provider_frames()
            transcript_response = backend.request(
                "GET", transcript_path, timeout=HTTP_WAIT_GUARD_SECONDS,
            )
            assert_status(transcript_response, 200)
            transcript = transcript_response.json()
            methods = [frame.get("method") for frame in frames]
            return "turn/start" in methods and "Recovered output." in json.dumps(transcript)
        try:
            backend.wait_until(turn_completed, "provider turn output through HTTP")
        except TimeoutError:
            pass
        history = backend.request("GET", "/api/messages?room=" + lead_id)
        assert_status(history, 200)
        self.assertTrue(any(item["id"] == body["id"] and item["text"] == body["text"]
                            for item in history.json()))
        methods = [frame.get("method") for frame in frames]
        diagnostic = backend.request("GET", "/api/desktop") if "turn/start" not in methods else None
        self.assertTrue({"initialize", "thread/start", "turn/start"}.issubset(methods),
                        (methods, backend.stderr,
                         None if diagnostic is None else diagnostic.json().get("nativeRuntime")))
        self.assertTrue(any(frame.get("method") == "turn/start" and
                            frame.get("params", {}).get("input") for frame in frames), frames)
        self.assertIn("Recovered output.", json.dumps(transcript), transcript)
        provider_starts = backend.provider_start_times()
        provider_start = next((value for value in provider_starts if value >= provider_request_started), None)
        self.assertIsNotNone(provider_start, "the fake Codex executable did not launch for this turn")
        print(json.dumps({
            "fakeCodexNativeSmokeLaunchAfterBackendStartSeconds": round(
                provider_starts[0] - backend.process_start_times[0], 4),
            "fakeCodexLaunchAfterMessageSeconds": round(
                (provider_start or provider_request_started) - provider_request_started, 4),
            "fakeCodexFramesAfterRequestSeconds": round(time.monotonic() - provider_request_started, 4),
        }, sort_keys=True), flush=True)


class RestartContractTests(TimedBoundaryTest):
    @classmethod
    def setUpClass(cls) -> None:
        started = time.monotonic()
        cls.backend = create_backend(cls)
        prepare_sync_store_for_rust_contract(cls.backend)
        cls.backend.class_setup_seconds = time.monotonic() - started

    def test_restart_serves_sync_receipt_and_provider_facts(self) -> None:
        backend = self.backend
        headers = {"Origin": backend.origin or "", "X-Canvas-Token": backend.session["token"],
                   "X-Studio-API-Schema": backend.schema_hash}
        identity = backend.request("GET", "/api/sync/identity",
                                   headers={"X-Studio-API-Schema": backend.schema_hash})
        assert_status(identity, 200)
        workspace_id = identity.json()["workspaceId"]
        protocol = backend.request("GET", "/api/sync/protocol",
                                   headers={"X-Studio-API-Schema": backend.schema_hash})
        assert_status(protocol, 200)
        self.assertEqual(protocol.json()["protocolVersion"], 3)
        snapshot = backend.request("GET", "/api/sync/pull?scope=drafts&after=0")
        assert_status(snapshot, 200)
        checkpoint = snapshot.json()["checkpoint"]["seq"]
        draft = {"newDocumentState": {"id": "restart-device:restart-session",
                 "payload": json.dumps({"device": "restart-device", "session": "restart-session",
                                        "text": "restart durable draft"})}}
        draft_response = backend.request(
            "POST", "/api/sync/drafts", body={"rows": [draft]},
            headers={**headers, "X-Canvas-Workspace": workspace_id},
        )
        assert_status(draft_response, 200)
        delta = backend.request("GET", f"/api/sync/pull?scope=drafts&after={checkpoint}")
        assert_status(delta, 200)
        self.assertEqual(delta.json()["documents"][0]["id"], draft["newDocumentState"]["id"])

        room_id = "20000000-0000-4000-8000-000000000001"
        room = backend.request("POST", "/api/chats", body={"id": room_id,
            "name": "Restart boundary room", "members": []}, headers=headers)
        assert_status(room, 200)
        body = {"id": "20000000-0000-4000-8000-000000000002", "room": room_id,
                "text": "restart receipt"}
        first = backend.request("POST", "/api/messages", body=body, headers=headers)
        assert_status(first, 200)
        replay_before = backend.request("POST", "/api/messages", body=body, headers=headers)
        assert_status(replay_before, 200)
        self.assertEqual(message_receipt(first.json()), message_receipt(replay_before.json()))
        history_before = backend.request("GET", "/api/messages?room=" + room_id)
        assert_status(history_before, 200)
        self.assertEqual(sum(item["id"] == body["id"] for item in history_before.json()), 1)

        lead = backend.request("POST", "/api/leads", body={"cwd": str(backend.cwd),
            "workspaceMode": "worktree"}, headers=headers)
        assert_status(lead, 200)
        lead_id = lead.json()["id"]
        provider_message = {"id": "20000000-0000-4000-8000-000000000004", "room": lead_id,
                            "text": "Reply with the fixture result.", "delivery": "queue", "assets": []}
        sent = backend.request("POST", "/api/messages", body=provider_message, headers=headers)
        assert_status(sent, 200)
        transcript_path = "/api/transcript?" + urlencode({"id": lead_id})
        transcript: dict[str, object] = {}
        def restarted_turn_completed() -> bool:
            nonlocal transcript
            transcript = backend.request(
                "GET", transcript_path, timeout=HTTP_WAIT_GUARD_SECONDS,
            )
            assert_status(transcript, 200)
            return "Recovered output." in json.dumps(transcript.json())
        try:
            backend.wait_until(restarted_turn_completed, "restart provider output through HTTP")
        except TimeoutError:
            pass
        transcript_text = json.dumps(transcript.json())
        if "Recovered output." not in transcript_text:
            native_status = backend.request("GET", "/api/desktop").json().get("nativeRuntime")
            self.fail(f"Provider turn did not reach the transcript: {native_status}")
        self.assertIn("turn/start", [frame.get("method")
                                    for frame in backend.received_provider_frames()])

        backend.stop()
        backend.ready_samples.append(backend.start())
        restarted_session = backend.request("GET", "/api/session")
        assert_status(restarted_session, 200)
        backend.session = restarted_session.json()
        pulled = backend.request("GET", "/api/sync/pull?scope=drafts&after=0")
        assert_status(pulled, 200)
        if rust_backend_configured:
            self.assertFalse(any(document["id"] == draft["newDocumentState"]["id"]
                                 for document in pulled.json()["documents"]))
        else:
            self.assertTrue(any(document["id"] == draft["newDocumentState"]["id"]
                                for document in pulled.json()["documents"]))
        restarted_identity = backend.request(
            "GET", "/api/sync/identity", headers={"X-Studio-API-Schema": backend.schema_hash}
        )
        assert_status(restarted_identity, 200)
        self.assertEqual(restarted_identity.json()["workspaceId"], workspace_id)
        restarted_protocol = backend.request(
            "GET", "/api/sync/protocol", headers={"X-Studio-API-Schema": backend.schema_hash}
        )
        if rust_backend_configured:
            self.assertEqual(restarted_protocol.status, 501, restarted_protocol.body[:1000])
            self.assertEqual(
                restarted_protocol.json()["error"],
                "Not implemented: requires sync-policy/runtime-capabilities",
            )
        else:
            assert_status(restarted_protocol, 200)
            self.assertEqual(restarted_protocol.json()["protocolVersion"], 3)
        headers = {"Origin": backend.origin or "", "X-Canvas-Token": backend.session["token"],
                   "X-Studio-API-Schema": backend.schema_hash}
        replay = backend.request("POST", "/api/messages", body=body, headers=headers)
        assert_status(replay, 200)
        self.assertEqual(message_receipt(replay.json()), message_receipt(first.json()))
        history = backend.request("GET", "/api/messages?room=" + room_id)
        assert_status(history, 200)
        self.assertEqual(sum(item["id"] == body["id"] for item in history.json()), 1)
        provider_history = backend.request("GET", "/api/messages?room=" + lead_id)
        assert_status(provider_history, 200)
        self.assertTrue(any(item["id"] == provider_message["id"]
                            for item in provider_history.json()))
        transcript = backend.request("GET", "/api/transcript?" + urlencode({"id": lead_id}))
        assert_status(transcript, 200)
        self.assertIn("Recovered output.", json.dumps(transcript.json()))
        methods = [frame.get("method") for frame in backend.received_provider_frames()]
        self.assertIn("turn/start", methods)

    def test_accounts_returns_the_rust_catalog_registry_snapshot(self) -> None:
        response = self.backend.request("GET", "/api/accounts")
        assert_status(response, 200)
        self.assertEqual(response.json()["accounts"][0]["status"], "ready")


def seed_catalog_state(backend: BackendProcess) -> None:
    backend.create_codex_replay("restart-mid-turn-supervisor-replay.json")
    claims = base64.urlsafe_b64encode(json.dumps({
        "https://api.openai.com/auth": {"chatgpt_account_id": "fixture-account"},
    }).encode()).rstrip(b"=").decode()
    (backend.codex_home / "auth.json").write_text(json.dumps({
        "tokens": {
            "account_id": "fixture-account",
            "access_token": f"e30.{claims}.fixture-signature",
        },
    }), encoding="utf-8")
    from codex_runtime import Runtime
    runtime = make_runtime(Runtime, backend)
    runtime.close()
    accounts = backend.state / "accounts"
    accounts.mkdir(parents=True, exist_ok=True)
    (accounts / "registry.json").write_text(json.dumps({
        "version": 1,
        "defaultAccountKey": "default",
        "accounts": {
            "default": {
                "id": "default", "home": str(backend.codex_home), "label": "Codex",
                "source": "Codex CLI", "provider": "codex", "accountId": "fixture-account",
                "status": "ready",
            },
        },
        "logins": {},
    }), encoding="utf-8")


if __name__ == "__main__":
    record_process_import_time()
    unittest.main(verbosity=2)
