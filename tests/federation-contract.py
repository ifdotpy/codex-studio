#!/usr/bin/env python3
"""Two-server federation contract tests. All state and HTTP listeners are private."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import importlib.util
import json
import sqlite3
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
sys.dont_write_bytecode = True
sys.path.insert(0, str(ROOT / "scripts"))
spec = importlib.util.spec_from_file_location("runtime_contract", ROOT / "tests" / "runtime-contract.py")
runtime_contract = importlib.util.module_from_spec(spec)
spec.loader.exec_module(runtime_contract)
from codex_runtime import Runtime
from codex_canvas import Canvas, make_server
from codex_federation import FederationService, PROTOCOL, _request_bytes, _sign, _json


class PrivateFederationHTTP:
    def __init__(self, service_getter):
        self.service_getter = service_getter
        owner = self

        class Handler(BaseHTTPRequestHandler):
            def do_POST(self):
                length = int(self.headers.get("Content-Length", "0"))
                raw = self.rfile.read(length)
                action = self.path.rsplit("/", 1)[-1]
                try:
                    result = owner.service_getter().route(action, self.headers,
                        self.client_address[0], raw)
                    status = 200
                except PermissionError as error:
                    result, status = {"error": str(error)}, 403
                except Exception as error:
                    result, status = {"error": str(error)}, 400
                data = json.dumps(result, separators=(",", ":")).encode()
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            def log_message(self, *_args):
                pass

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.origin = f"http://127.0.0.1:{self.server.server_port}"

    def close(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=2)


class FederationContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        base = Path(self.temp.name)
        self.roots = [base / "igor", base / "sasha"]
        self.runtimes = [Runtime(root, runtime_contract.FakeServer) for root in self.roots]
        self.services = [runtime.federation() for runtime in self.runtimes]
        for service in self.services:
            service.start = lambda: None
        self.leads = [runtime.create({"name": name, "cwd": str(root), "prompt": "Use the fake model"})
                      for runtime, root, name in zip(self.runtimes, self.roots, ("Igor", "Sasha"))]
        for runtime, lead in zip(self.runtimes, self.leads):
            runtime_contract.eventually(lambda runtime=runtime, lead=lead:
                                        runtime.agent(lead["id"])["status"] == "running")
            runtime.send(lead["id"], "Temporary fake-model fixture", "federation-fixture-" + lead["id"])
            runtime_contract.eventually(lambda runtime=runtime, lead=lead:
                                        runtime.agent(lead["id"]).get("turnId") is not None)
            running = runtime.agent(lead["id"])
            runtime.servers["default"].complete(running["threadId"], running["turnId"])
        self.http = [PrivateFederationHTTP(lambda i=i: self.runtimes[i].federation()) for i in range(2)]
        self._enable()
        self._pair()

    def tearDown(self):
        for server in getattr(self, "http", []):
            server.close()
        for runtime in getattr(self, "runtimes", []):
            runtime.close()
        self.temp.cleanup()

    def _enable(self):
        for service in self.services:
            service.action({"action": "set_enabled", "enabled": True})

    def _pair(self):
        from codex_remote import RemoteAccess, validate_origin
        with patch("codex_federation._whoami", return_value={"status": "unavailable", "user": None}):
            with patch.object(RemoteAccess, "origin", new=lambda _self: self.http[1].origin):
                invitation = self.services[1].create_invite({"label": "Sasha Studio"})["invitation"]
        with patch("codex_remote.validate_origin", side_effect=lambda v: v):
            with patch.object(RemoteAccess, "origin", new=lambda _self: self.http[0].origin):
                self.services[0].accept_peer(invitation)
        local_ids = (invitation["stateId"], self.services[0]._local_identity()["stateId"])
        for service, remote_id in zip(self.services, local_ids):
            with self.assertRaisesRegex(ValueError, "Explicitly accept the warning"):
                service.approve_peer(remote_id)
            service.approve_peer(remote_id, accept_missing_whois=True)
        # Igor can reach Sasha, but Sasha cannot initiate to Igor. Igor's signed
        # status poll observes Sasha's explicit approval and completes the pair.
        peer = self._peer(self.services[0], invitation["stateId"])
        with patch("codex_remote.validate_origin", side_effect=lambda v: v):
            response = self.services[0]._request(self.http[1].origin, "/api/federation/v1/status",
                {"protocol": PROTOCOL}, self.services[0]._local_identity(),
                expected_key=peer["publicKey"], expected_state=peer["stateId"])
        self.assertTrue(response["approved"])
        peer["remoteApproved"] = True
        peer["status"] = "approved"
        self.services[0]._save_peer(peer)
        states = [peer for service in self.services for peer in service.snapshot()["peers"]]
        self.assertTrue(all(peer["status"] == "approved" for peer in states), states)

    def _room(self, share_names=False, share_status=False, include_child=False):
        a, b = self.services
        peer_a = a.snapshot()["peers"][0]["stateId"]
        peer_b = b.snapshot()["peers"][0]["stateId"]
        members = [[lead["id"]] for lead in self.leads]
        if include_child:
            for index, (runtime, lead) in enumerate(zip(self.runtimes, self.leads)):
                child = runtime.create(
                    {"name": "Federated child", "prompt": "Fixture child", "role": "reviewer"},
                    lead["id"], defer=True,
                )
                members[index].append(child["id"])
        old_rooms = {room["id"] for room in a.snapshot()["rooms"]}
        a.create_room({"peer_id": peer_a, "local_members": members[0],
                       "share_names": share_names, "share_status": share_status})
        room_id = next(room["id"] for room in a.snapshot()["rooms"]
                       if room["id"] not in old_rooms)
        # Deliver invite B-side, explicitly approve on B, then exchange accept.
        local_a = a._local_identity()
        record = self._outbox_record(a, peer_a)
        envelope = a._outbound_envelope(record, local_a)
        receipt = b.receive_envelope(self._peer(b, peer_b), envelope)
        self.assertTrue(receipt["accepted"])
        b.approve_room({"room_id": room_id, "local_members": members[1],
                        "share_names": share_names, "share_status": share_status})
        record = self._outbox_record(b, peer_b)
        envelope = b._outbound_envelope(record, b._local_identity())
        a.receive_envelope(self._peer(a, peer_a), envelope)
        # Acknowledge room invite on B and room accept on A.
        self._mark_delivered(a, peer_a)
        self._mark_delivered(b, peer_b)
        return room_id

    def test_room_producers_emit_only_named_local_participant_fields(self):
        from studio_api.sync.models import SnapshotRoomDto

        cases = (
            (self._room(), {"id", "role", "name"}, {"lead"}),
            (self._room(share_names=True, share_status=True, include_child=True),
             {"id", "role", "name", "status"}, {"lead", "agent"}),
        )
        for room_id, expected_keys, expected_roles in cases:
            for service in self.services:
                with service.runtime.read_db() as db:
                    row = db.execute(
                        "SELECT record FROM runtime_federation_rooms WHERE id=?", (room_id,)
                    ).fetchone()
                record = json.loads(row[0])
                dto = SnapshotRoomDto.model_validate({
                    "id": room_id,
                    "localParticipants": record["localParticipants"],
                })
                participants = [
                    participant.model_dump(mode="json", exclude_unset=True)
                    for participant in dto.localParticipants or []
                ]
                self.assertEqual({participant["role"] for participant in participants}, expected_roles)
                self.assertTrue(all(set(participant) == expected_keys for participant in participants))
                with service.runtime.read_db() as db:
                    payload = db.execute("SELECT payload FROM sync_entities WHERE collection='room' AND id=?",
                                         (room_id,)).fetchone()[0]
                value = json.loads(payload)["value"]
                self.assertEqual(value["peerLabel"], record["peerLabel"])
                self.assertEqual(value["localMembers"], sorted(set(record["localMembers"])))

    @staticmethod
    def _outbox_record(service, peer_id):
        with service.runtime.read_db() as db:
            row = db.execute("SELECT id,record FROM runtime_federation_outbox WHERE peer_id=? AND json_extract(record,'$.status')='queued' ORDER BY json_extract(record,'$.created') LIMIT 1", (peer_id,)).fetchone()
        return {"id": row[0], **json.loads(row[1])}

    @staticmethod
    def _mark_delivered(service, peer_id):
        with service.runtime.lock, service.runtime.db() as db:
            rows = db.execute("SELECT id,record FROM runtime_federation_outbox WHERE peer_id=?", (peer_id,)).fetchall()
            for row in rows:
                item = json.loads(row[1])
                item["status"] = "delivered"
                db.execute("UPDATE runtime_federation_outbox SET record=? WHERE id=?", (json.dumps(item), row[0]))
                service._confirm_room_accept_locked(db, item)

    @staticmethod
    def _peer(service, state_id):
        with service.runtime.read_db() as db:
            return json.loads(db.execute("SELECT record FROM runtime_federation_peers WHERE id=?", (state_id,)).fetchone()[0])

    def test_two_private_servers_pair_and_exchange_messages_both_ways(self):
        room = self._room()
        user_message = self._canvas_user_message(0, room, "hello Sasha from Messages", "ui-message-igor")
        self.assertEqual(user_message["id"], "ui-message-igor")
        self.assertEqual(user_message["status"], "queued")
        self.services[0]._deliver_peer(self.services[0].snapshot()["peers"][0])
        with self.runtimes[0].lock, self.runtimes[0].db() as db:
            stopped = self.runtimes[0].agent(self.leads[0]["id"], db)
            stopped.update(autoWake=False, status="paused", epoch=1)
            self.runtimes[0].put(db, "agents", stopped)
        with self.assertRaisesRegex(ValueError, "Sender was stopped"):
            self.runtimes[0].chat_message(self.leads[0]["id"],
                "remote:" + self.services[0].snapshot()["peers"][0]["stateId"],
                "must not send", "stopped-agent-message", epoch=0)
        with self.runtimes[0].lock, self.runtimes[0].db() as db:
            active = self.runtimes[0].agent(self.leads[0]["id"], db)
            active.update(autoWake=True, status="running", epoch=1)
            self.runtimes[0].put(db, "agents", active)
        first = self.runtimes[0].chat_message(self.leads[0]["id"],
            "remote:" + self.services[0].snapshot()["peers"][0]["stateId"],
            "hello Sasha", "igor-1", epoch=1)
        self.assertEqual(first["status"], "queued")
        self.services[0]._deliver_peer(self.services[0].snapshot()["peers"][0])
        second = self.runtimes[1].chat_message(self.leads[1]["id"], room,
            "hello Igor", "sasha-1", epoch=0)
        self.assertEqual(second["status"], "queued")
        self.services[1]._deliver_peer(self.services[1].snapshot()["peers"][0])
        for runtime, expected in zip(self.runtimes,
                (("hello Igor",), ("hello Sasha from Messages", "hello Sasha"))):
            with runtime.read_db() as db:
                rows = db.execute("SELECT text FROM runtime_chat_messages WHERE room=? ORDER BY seq", (room,)).fetchall()
            self.assertTrue(all(message in [row[0] for row in rows] for message in expected))
        self.assertEqual(first["deliveries"].get("remote:" + self.services[0].snapshot()["peers"][0]["stateId"]), "queued")
        self.assertEqual(second["id"], self._message_id(self.services[1], "sasha-1"))

    def _canvas_user_message(self, runtime_index, room, text, message_id):
        canvas = Canvas(self.roots[runtime_index])
        canvas.runtime = self.runtimes[runtime_index]
        server = make_server(canvas, 0)
        worker = threading.Thread(target=server.serve_forever, daemon=True)
        worker.start()
        origin = f"http://127.0.0.1:{server.server_port}"
        try:
            with urllib.request.urlopen(origin + "/api/session", timeout=5) as response:
                token = json.loads(response.read())["token"]
            body = _json({"room": room, "text": text, "id": message_id}).encode()
            request = urllib.request.Request(origin + "/api/messages", data=body, method="POST",
                headers={"Content-Type": "application/json", "X-Canvas-Token": token})
            with urllib.request.urlopen(request, timeout=8) as response:
                return json.loads(response.read())
        finally:
            server.shutdown()
            server.server_close()
            worker.join(timeout=2)

    @staticmethod
    def _message_id(service, key):
        identity = service._local_identity()
        import uuid
        return str(uuid.uuid5(uuid.UUID(identity["stateId"]), key))

    def test_duplicate_message_is_exactly_once_and_replay_nonce_is_rejected(self):
        room = self._room()
        a, b = self.services
        peer = a.snapshot()["peers"][0]
        a.send_message(self.leads[0]["id"], room, "one copy", "dedupe-1")
        item = self._outbox_record(a, peer["stateId"])
        envelope = a._outbound_envelope(item, a._local_identity())
        remote_peer = self._peer(b, a._local_identity()["stateId"])
        first = b.receive_envelope(remote_peer, envelope)
        retry = a._outbound_envelope(item, a._local_identity())
        second = b.receive_envelope(remote_peer, retry)
        self.assertEqual(first, second)
        with self.assertRaisesRegex(PermissionError, "Repeated federation nonce"):
            b.receive_envelope(remote_peer, envelope)
        with b.runtime.read_db() as db:
            count = db.execute("SELECT count(*) FROM runtime_chat_messages WHERE room=? AND text='one copy'", (room,)).fetchone()[0]
        self.assertEqual(count, 1)

    def test_request_body_signature_and_request_nonce_are_enforced(self):
        room = self._room()
        a, b = self.services
        peer = a.snapshot()["peers"][0]
        a.send_message(self.leads[0]["id"], room, "signed request", "request-signature-1")
        item = self._outbox_record(a, peer["stateId"])
        envelope = a._outbound_envelope(item, a._local_identity())
        body = _json({"protocol": PROTOCOL, "envelope": envelope}).encode()
        headers = self._signed_headers(a, envelope)
        with self.assertRaisesRegex(PermissionError, "Invalid federation request signature"):
            b.route("message", headers, "127.0.0.1", body + b" ")
        accepted = b.route("message", headers, "127.0.0.1", body)
        self.assertTrue(accepted["result"]["accepted"])
        with self.assertRaisesRegex(PermissionError, "Repeated federation nonce"):
            b.route("message", headers, "127.0.0.1", body)

    def test_tailscale_whois_mismatch_refuses_pairing(self):
        with self.assertRaisesRegex(PermissionError, "identity mismatch"):
            self.services[0]._validate_whois({}, {"tailscaleUser": "igor@example.test"},
                {"Tailscale-User-Login": "someone-else@example.test"}, "100.64.0.4")

    def _signed_headers(self, service, envelope):
        import secrets
        local = service._local_identity()
        raw = _json({"protocol": PROTOCOL, "envelope": envelope}).encode()
        timestamp, nonce = int(time.time()), secrets.token_urlsafe(24)
        path = "/api/federation/v1/message"
        return {"X-Studio-Federation-State": local["stateId"],
                "X-Studio-Federation-Time": str(timestamp), "X-Studio-Federation-Nonce": nonce,
                "X-Studio-Federation-Signature": _sign(local["privateKey"], _request_bytes("POST", path, timestamp, nonce, raw))}

    def test_one_way_reachability_delivers_remote_outbox_by_pull(self):
        room = self._room()
        b = self.services[1]
        peer_b = b.snapshot()["peers"][0]
        b.send_message(self.leads[1]["id"], room, "reverse through pull", "pull-1")
        # Only A's route to B is used: A pulls B's durable outbox.
        a = self.services[0]
        peer_a = a.snapshot()["peers"][0]
        a._deliver_peer(peer_a)
        with self.runtimes[0].read_db() as db:
            rows = db.execute("SELECT text FROM runtime_chat_messages WHERE room=?", (room,)).fetchall()
        self.assertIn("reverse through pull", [row[0] for row in rows])
        self.assertTrue(peer_b["stateId"])

    def test_revoke_deletes_queued_outbox_and_nonpaired_caller_is_rejected(self):
        room = self._room()
        a = self.services[0]
        peer = a.snapshot()["peers"][0]
        a.send_message(self.leads[0]["id"], room, "queued", "revoke-1")
        a.revoke_peer(peer["stateId"])
        with a.runtime.read_db() as db:
            self.assertEqual(db.execute("SELECT count(*) FROM runtime_federation_outbox WHERE peer_id=? AND json_extract(record,'$.status')='queued'", (peer["stateId"],)).fetchone()[0], 0)
            deleted = db.execute("SELECT deleted FROM sync_entities WHERE collection='room' AND id=?",
                                 (room,)).fetchone()[0]
            self.assertEqual(deleted, 1)
        fake = {"stateId": "00000000-0000-4000-8000-000000000000"}
        body = _json({"protocol": PROTOCOL}).encode()
        headers = {"Content-Type": "application/json", "X-Studio-Federation-State": fake["stateId"],
                   "X-Studio-Federation-Time": str(int(time.time())),
                   "X-Studio-Federation-Nonce": "z" * 24,
                   "X-Studio-Federation-Signature": "not-a-signature"}
        request = urllib.request.Request(self.http[1].origin + "/api/federation/v1/status",
                                         data=body, method="POST", headers=headers)
        with self.assertRaises(urllib.error.HTTPError) as error:
            urllib.request.urlopen(request, timeout=3)
        self.assertEqual(error.exception.code, 403)

    def test_version_mismatch_and_schema_upgrade_are_opt_in_safe(self):
        service = self.services[0]
        remote = service._local_identity()
        other = self.services[1]._local_identity()
        with self.assertRaisesRegex(ValueError, "Unsupported federation protocol"):
            service._request(self.http[1].origin, "/api/federation/v1/status",
                {"protocol": PROTOCOL + 1}, remote, expected_key=other["publicKey"],
                expected_state=other["stateId"])

        legacy = sqlite3.connect(":memory:")
        legacy.execute("CREATE TABLE runtime_agents (id TEXT PRIMARY KEY, record TEXT NOT NULL)")
        legacy.execute("CREATE TABLE runtime_chat_messages (id TEXT PRIMARY KEY, room TEXT NOT NULL)")
        legacy_tables = {row[0] for row in legacy.execute("SELECT name FROM sqlite_master WHERE type='table'")}
        FederationService.ensure_tables(legacy)
        after = {row[0] for row in legacy.execute("SELECT name FROM sqlite_master WHERE type='table'")}
        added = after - legacy_tables
        self.assertTrue(added)
        self.assertTrue(all(name.startswith("runtime_federation_") for name in added))
        legacy.close()

        fresh_runtime = Runtime(self.roots[0].parent / "never-enabled", runtime_contract.FakeServer)
        self.runtimes.append(fresh_runtime)
        fresh_service = fresh_runtime.federation()
        lead = fresh_runtime.create({"name": "Unused lead", "cwd": str(self.roots[0]),
                                     "prompt": "Fake only"}, defer=True)
        with fresh_runtime.lock, fresh_runtime.db() as db:
            agent = fresh_runtime.agent(lead["id"], db)
            agent.update(autoWake=True, status="running")
            fresh_runtime.put(db, "agents", agent)
        with fresh_runtime.read_db() as db:
            self.assertFalse(fresh_service.enabled(db))
            self.assertFalse(any(room.get("federated") for room in fresh_runtime.chat_rooms(db)))
        tools = fresh_runtime.model_directory(lead["id"], "orchestration_peers", {"scope": "team"})
        self.assertNotIn("remotePeers", tools)

    def test_enabled_service_starts_on_init_and_stops_on_failed_init_or_close(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / "state"
            initial = Runtime(root, runtime_contract.FakeServer)
            initial.federation().action({"action": "set_enabled", "enabled": True})
            initial.close()

            original_start = FederationService.start
            started = []

            def start_then_fail(service):
                original_start(service)
                started.append(service)
                raise RuntimeError("fixture failure after federation start")

            with patch.object(FederationService, "start", start_then_fail):
                with self.assertRaisesRegex(RuntimeError, "fixture failure"):
                    Runtime(root, runtime_contract.FakeServer)
            self.assertEqual(len(started), 1)
            failed_service = started[0]
            self.assertTrue(failed_service.stop_event.is_set())
            self.assertFalse(failed_service.thread.is_alive())

            runtime = Runtime(root, runtime_contract.FakeServer)
            service = runtime._federation_service
            self.assertIsNotNone(service)
            self.assertTrue(service.thread.is_alive())
            runtime.close()
            self.assertTrue(service.stop_event.is_set())
            self.assertFalse(service.thread.is_alive())

    def test_restart_preserves_pending_delivery_for_retry(self):
        room = self._room()
        a = self.services[0]
        a.send_message(self.leads[0]["id"], room, "survives restart", "restart-1")
        old = self.runtimes[0]
        old.close()
        with patch.object(FederationService, "start", lambda _self: None):
            self.runtimes[0] = Runtime(self.roots[0], runtime_contract.FakeServer)
        self.services[0] = self.runtimes[0].federation()
        self.services[0].start = lambda: None
        self.services[0]._deliver_peer(self.services[0].snapshot()["peers"][0])
        with self.runtimes[1].read_db() as db:
            rows = db.execute("SELECT text FROM runtime_chat_messages WHERE room=?", (room,)).fetchall()
        self.assertIn("survives restart", [row[0] for row in rows])
        b = self.services[1]
        b.send_message(self.leads[1]["id"], room, "survives Sasha restart", "restart-2")
        old = self.runtimes[1]
        old.close()
        with patch.object(FederationService, "start", lambda _self: None):
            self.runtimes[1] = Runtime(self.roots[1], runtime_contract.FakeServer)
        self.services[1] = self.runtimes[1].federation()
        self.services[1].start = lambda: None
        self.services[1]._deliver_peer(self.services[1].snapshot()["peers"][0])
        with self.runtimes[0].read_db() as db:
            rows = db.execute("SELECT text FROM runtime_chat_messages WHERE room=?", (room,)).fetchall()
        self.assertIn("survives Sasha restart", [row[0] for row in rows])


if __name__ == "__main__":
    unittest.main(verbosity=2)
