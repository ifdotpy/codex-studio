"""Opt-in peer federation. Wire protocol v1; message IDs and receipts are transport independent."""
from __future__ import annotations
from collections.abc import Mapping

import base64
import hashlib
import hmac
import ipaddress
import json
import os
from pathlib import Path
import secrets
import shutil
import sqlite3
import subprocess
import threading
import time
import urllib.error
import urllib.request
import uuid

from codex_remote import RemoteAccess
from codex_executables import tailscale as resolve_tailscale, command as executable_command
from codex_records import (
    FederationIdentityRecord, FederationInviteRecord, FederationOutboxRecord,
    AgentRecord, FederationPeerRecord, JsonObject, JsonValue, RecordStore, RoomRecord,
)

PROTOCOL = 1
MAX_CLOCK_SKEW = 300
MAX_BODY = 256 * 1024
MAX_MESSAGE = 12_000
MAX_BATCH = 50
MAX_BATCH_BYTES = 200 * 1024
CRYPTO_HELPER = Path(__file__).with_name("codex_federation_crypto.mjs")


def _json(value: JsonValue) -> str:
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False)


def _crypto(operation: str, **fields: JsonValue) -> JsonObject:
    node = os.environ.get("CODEX_NODE") or shutil.which("node")
    if not node:
        raise RuntimeError("The Studio Node runtime is required for Ed25519 federation and is unavailable")
    request = json.dumps({"operation": operation, **fields}, ensure_ascii=False).encode()
    try:
        env = {**os.environ, "ELECTRON_RUN_AS_NODE": "1"}
        result = subprocess.run([node, str(CRYPTO_HELPER)], input=request, env=env,
                                capture_output=True, timeout=8, check=True)
        value = json.loads(result.stdout)
        if not isinstance(value, dict):
            raise ValueError
        return value
    except (subprocess.SubprocessError, OSError, ValueError, json.JSONDecodeError):
        raise RuntimeError("Ed25519 operation failed") from None


def _sign(private_key: str, data: bytes) -> str:
    return _crypto("sign", privateKey=private_key,
                   data=base64.b64encode(data).decode())["signature"]


def _verify(public_key: str, data: bytes, signature: str) -> bool:
    try:
        return _crypto("verify", publicKey=public_key,
                       data=base64.b64encode(data).decode(),
                       signature=signature).get("valid") is True
    except RuntimeError:
        return False


def _record(row: sqlite3.Row | None) -> JsonObject | None:
    return json.loads(row[0]) if row else None


def _safe_peer(peer: FederationPeerRecord) -> JsonObject:
    return {key: peer.get(key) for key in (
        "stateId", "label", "origin", "publicKey", "status", "localApproved",
        "remoteApproved", "whoisStatus", "whoisUser", "created", "updated", "lastError")}


def _peer_allowed(peer: FederationPeerRecord) -> bool:
    return (peer and peer.get("status") == "approved"
            and peer.get("localApproved") is True
            and peer.get("remoteApproved") is True)


def _fresh(timestamp: int) -> bool:
    now = int(time.time())
    return type(timestamp) is int and now - MAX_CLOCK_SKEW <= timestamp <= now + MAX_CLOCK_SKEW


def _request_bytes(method: str, path: str, timestamp: int, nonce: str, body: JsonObject) -> bytes:
    digest = hashlib.sha256(body).hexdigest()
    return f"studio-federation-v1\n{method}\n{path}\n{timestamp}\n{nonce}\n{digest}".encode()


def _message_bytes(envelope: JsonObject) -> bytes:
    return b"studio-federation-message-v1\n" + _json(
        {key: value for key, value in envelope.items() if key != "signature"}
    ).encode()


def _whoami() -> JsonObject:
    executable = resolve_tailscale()
    if not executable:
        return {"status": "unavailable", "warning": "Tailscale identity could not be checked"}
    try:
        result = subprocess.run(executable_command(executable, ["whoami", "--json"]), capture_output=True,
                                text=True, timeout=5, check=True)
        data = json.loads(result.stdout)
        user_data = data.get("UserProfile") or data.get("User") or {}
        user = user_data.get("LoginName") or user_data.get("Name")
        return {"status": "verified" if isinstance(user, str) and user else "unavailable",
                "user": user if isinstance(user, str) else None,
                "node": (data.get("Self") or data.get("Machine") or {}).get("ID")}
    except (OSError, subprocess.SubprocessError, ValueError, AttributeError):
        return {"status": "unavailable", "warning": "Tailscale identity could not be checked"}


def _observed_whois(headers: Mapping[str, str], remote_address: str) -> str | None:
    # Serve removes identity headers supplied by clients. Never trust X-Forwarded-
    # For unless RemoteAccess has already authenticated its loopback proxy path.
    login = headers.get("Tailscale-User-Login")
    if login:
        return login.strip().lower()
    forwarded = headers.get("X-Forwarded-For")
    try:
        address = ipaddress.ip_address(forwarded or remote_address)
    except ValueError:
        return None
    if not (address in ipaddress.ip_network("100.64.0.0/10")
            or address in ipaddress.ip_network("fd7a:115c:a1e0::/48")):
        return None
    executable = resolve_tailscale()
    if not executable:
        return None
    try:
        result = subprocess.run(executable_command(executable, ["whois", str(address)]), capture_output=True,
                                text=True, timeout=5, check=True)
        in_user = False
        for line in result.stdout.splitlines():
            if line.strip() == "User:":
                in_user = True
            elif line.strip().startswith("Capabilities:"):
                in_user = False
            elif in_user and line.strip().startswith("Name:"):
                return line.split(":", 1)[1].strip().lower()
    except (OSError, subprocess.SubprocessError):
        pass
    return None


class FederationService:
    """Durable state and a restartable HTTP delivery pump for one server identity."""

    def __init__(self, runtime: RecordStore) -> None:
        self.runtime = runtime
        self.stop_event = threading.Event()
        self.thread = None

    @staticmethod
    def ensure_tables(db: sqlite3.Connection) -> None:
        db.executescript("""
        CREATE TABLE IF NOT EXISTS runtime_federation_settings (
          id TEXT PRIMARY KEY, record TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS runtime_federation_identity (
          id TEXT PRIMARY KEY, record TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS runtime_federation_peers (
          id TEXT PRIMARY KEY, record TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS runtime_federation_invites (
          id TEXT PRIMARY KEY, record TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS runtime_federation_rooms (
          id TEXT PRIMARY KEY, record TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS runtime_federation_outbox (
          id TEXT PRIMARY KEY, peer_id TEXT NOT NULL, record TEXT NOT NULL);
        CREATE INDEX IF NOT EXISTS runtime_federation_outbox_peer
          ON runtime_federation_outbox(peer_id);
        CREATE TABLE IF NOT EXISTS runtime_federation_inbox (
          peer_id TEXT NOT NULL, message_id TEXT NOT NULL, record TEXT NOT NULL,
          PRIMARY KEY(peer_id,message_id));
        CREATE TABLE IF NOT EXISTS runtime_federation_nonces (
          peer_id TEXT NOT NULL, nonce TEXT NOT NULL, created INTEGER NOT NULL,
          PRIMARY KEY(peer_id,nonce));
        CREATE INDEX IF NOT EXISTS runtime_federation_nonce_age
          ON runtime_federation_nonces(created);
        """)

    def enabled(self, db: sqlite3.Connection=None) -> bool:
        if db is not None:
            value = _record(db.execute("SELECT record FROM runtime_federation_settings WHERE id='global'").fetchone())
        else:
            with self.runtime.read_db() as read:
                value = _record(read.execute("SELECT record FROM runtime_federation_settings WHERE id='global'").fetchone())
        return bool(value and value.get("enabled") is True)

    def ensure_identity(self, db: sqlite3.Connection) -> FederationIdentityRecord:
        identity = _record(db.execute("SELECT record FROM runtime_federation_identity WHERE id='local'").fetchone())
        if identity:
            return identity
        keys = _crypto("generate")
        state_id = str(uuid.uuid4())
        from codex_runtime import LIVE_AGENT_SQL
        lead_row = db.execute("SELECT record FROM runtime_agents WHERE json_extract(record,'$.isLead')=1 "
                              f"AND {LIVE_AGENT_SQL} ORDER BY rowid LIMIT 1").fetchone()
        lead = json.loads(lead_row[0]) if lead_row else None
        identity = {"stateId": state_id, "privateKey": keys["privateKey"],
                    "publicKey": keys["publicKey"], "label": "Studio server",
                    "leadName": lead.get("name", "Lead") if lead else "Lead",
                    "created": time.time()}
        db.execute("INSERT INTO runtime_federation_identity(id,record) VALUES('local',?)", (_json(identity),))
        return identity

    def _local_identity(self, *, create: bool=False) -> FederationIdentityRecord:
        with self.runtime.lock, self.runtime.db() as db:
            identity = _record(db.execute("SELECT record FROM runtime_federation_identity WHERE id='local'").fetchone())
            if identity or not create:
                return identity
            db.execute("BEGIN IMMEDIATE")
            return self.ensure_identity(db)

    def start(self) -> None:
        if self.thread and self.thread.is_alive():
            return
        self.stop_event.clear()
        self.thread = threading.Thread(target=self.run, name="studio-federation", daemon=True)
        self.thread.start()

    def close(self) -> None:
        self.stop_event.set()
        if self.thread and threading.current_thread() is not self.thread:
            self.thread.join()

    def run(self) -> None:
        while not self.stop_event.wait(1):
            if self.runtime.closed or not self.enabled():
                continue
            try:
                self.pump_once()
            except (OSError, ValueError, RuntimeError):
                # Durable rows retain their retry identity. The UI reads lastError.
                continue

    def snapshot(self) -> JsonObject:
        with self.runtime.read_db() as db:
            settings = _record(db.execute("SELECT record FROM runtime_federation_settings WHERE id='global'").fetchone()) or {"enabled": False}
            peers = [_safe_peer(json.loads(row[0])) for row in db.execute("SELECT record FROM runtime_federation_peers ORDER BY id")]
            invites = [{k: v for k, v in json.loads(row[0]).items()
                        if k in {"id", "created", "expires", "status", "expectedUser"}}
                       for row in db.execute("SELECT record FROM runtime_federation_invites")]
            rooms = [json.loads(row[0]) for row in db.execute("SELECT record FROM runtime_federation_rooms ORDER BY id")]
            pending = db.execute("SELECT count(*) FROM runtime_federation_outbox WHERE json_extract(record,'$.status')='queued'").fetchone()[0]
            identity = _record(db.execute("SELECT record FROM runtime_federation_identity WHERE id='local'").fetchone())
        return {"version": PROTOCOL, "enabled": settings.get("enabled") is True,
                "identity": ({"stateId": identity["stateId"], "label": identity["label"],
                              "fingerprint": hashlib.sha256(identity["publicKey"].encode()).hexdigest()[:32]}
                             if identity else None),
                "peers": peers, "invites": invites,
                "rooms": [self.public_room(room) for room in rooms],
                "queued": pending}

    @staticmethod
    def public_room(room: RoomRecord) -> JsonObject:
        return {k: room.get(k) for k in (
            "id", "peerId", "peerLabel", "name", "status", "localMembers",
            "remoteMembers", "shareNames", "shareStatus", "created")}

    def action(self, body: JsonObject) -> JsonObject:
        action = body.get("action")
        if action == "status":
            return self.snapshot()
        if action == "set_enabled":
            enabled = body.get("enabled")
            if type(enabled) is not bool:
                raise ValueError("enabled must be a boolean")
            with self.runtime.lock, self.runtime.db() as db:
                db.execute("BEGIN IMMEDIATE")
                db.execute("INSERT INTO runtime_federation_settings(id,record) VALUES('global',?) "
                           "ON CONFLICT(id) DO UPDATE SET record=excluded.record",
                           (_json({"enabled": enabled, "updated": time.time()}),))
            if enabled:
                self.start()
            return self.snapshot()
        if action == "create_invite":
            if not self.enabled():
                raise ValueError("Enable federation before creating an invitation")
            return self.create_invite(body)
        if action == "accept_peer":
            if not self.enabled():
                raise ValueError("Enable federation before pairing")
            return self.accept_peer(body.get("invitation"))
        if action == "approve_peer":
            return self.approve_peer(body.get("state_id"))
        if action == "revoke_peer":
            return self.revoke_peer(body.get("state_id"))
        if action == "create_room":
            return self.create_room(body)
        if action == "approve_room":
            return self.approve_room(body)
        if action == "room_options":
            return self.room_options(body)
        raise ValueError("Unknown federation action")

    def create_invite(self, body: JsonObject) -> JsonObject:
        label = body.get("label", "Studio server")
        if not isinstance(label, str) or not 1 <= len(label.strip()) <= 80:
            raise ValueError("Server label must be 1 to 80 characters")
        origin = RemoteAccess(self.runtime.root).origin()
        if not origin:
            raise ValueError("Configure Tailscale Serve before creating a pairing invitation")
        expected_user = body.get("expected_user")
        if expected_user is not None and (not isinstance(expected_user, str) or len(expected_user) > 254):
            raise ValueError("Invalid expected Tailscale user")
        with self.runtime.lock, self.runtime.db() as db:
            db.execute("BEGIN IMMEDIATE")
            identity = self.ensure_identity(db)
            whoami = _whoami()
            identity.update(label=label.strip(), leadName=self.local_lead_name(db),
                            tailscaleUser=whoami.get("user"), tailscaleStatus=whoami["status"])
            db.execute("UPDATE runtime_federation_identity SET record=? WHERE id='local'", (_json(identity),))
            invite_id = str(uuid.uuid4())
            token = secrets.token_urlsafe(32)
            expires = int(time.time()) + 900
            record = {"id": invite_id, "tokenHash": hashlib.sha256(token.encode()).hexdigest(),
                      "created": time.time(), "expires": expires, "status": "open",
                      "expectedUser": expected_user.strip().lower() if expected_user else None}
            db.execute("INSERT INTO runtime_federation_invites(id,record) VALUES(?,?)", (invite_id, _json(record)))
            invitation = {"protocol": PROTOCOL, "inviteId": invite_id, "token": token,
                          "stateId": identity["stateId"], "label": identity["label"],
                          "leadName": identity["leadName"], "origin": origin,
                          "publicKey": identity["publicKey"], "tailscaleUser": identity.get("tailscaleUser"),
                          "tailscaleStatus": identity.get("tailscaleStatus"), "expires": expires}
        return {"invitation": invitation, "expires": expires,
                "warning": "Share this invitation privately. It expires in 15 minutes and can be used once."}

    @staticmethod
    def local_lead_name(db: sqlite3.Connection) -> str | None:
        row = db.execute("SELECT record FROM runtime_agents WHERE json_extract(record,'$.isLead')=1 "
                         "AND (json_extract(record,'$.deletedAt') IS NULL OR json_extract(record,'$.deletedAt')=0) "
                         "ORDER BY id LIMIT 1").fetchone()
        return json.loads(row[0]).get("name", "Lead") if row else "Lead"

    def accept_peer(self, invitation: JsonObject) -> JsonObject:
        if not isinstance(invitation, dict) or invitation.get("protocol") != PROTOCOL:
            raise ValueError("Unsupported federation invitation version")
        required = ("inviteId", "token", "stateId", "label", "origin", "publicKey", "expires")
        if any(not isinstance(invitation.get(key), str if key != "expires" else int) for key in required):
            raise ValueError("Invalid federation invitation")
        if invitation["expires"] < time.time() or invitation["expires"] > time.time() + 900:
            raise ValueError("Federation invitation has expired")
        if len(invitation["token"]) > 128 or len(invitation["publicKey"]) > 4096:
            raise ValueError("Federation invitation is too large")
        from codex_remote import validate_origin
        origin = validate_origin(invitation["origin"])
        if len(invitation["label"]) > 80 or not invitation["stateId"]:
            raise ValueError("Invalid peer identity")
        local = self._local_identity(create=True)
        peer = {"stateId": invitation["stateId"], "label": invitation["label"].strip(),
                "leadName": invitation.get("leadName"),
                "origin": origin, "publicKey": invitation["publicKey"],
                "inviteId": invitation["inviteId"], "inviteToken": invitation["token"],
                "status": "pending", "localApproved": True, "remoteApproved": False,
                "whoisStatus": "pending", "whoisUser": invitation.get("tailscaleUser"),
                "created": time.time(), "updated": time.time(), "protocol": PROTOCOL}
        if not peer["stateId"] or peer["stateId"] == local["stateId"]:
            raise ValueError("Cannot pair a server with itself")
        try:
            uuid.UUID(peer["stateId"])
        except ValueError:
            raise ValueError("Peer state identity is invalid") from None
        self._save_peer(peer)
        try:
            result = self._send_pair(peer)
            peer["whoisStatus"] = result.get("whoisStatus", "missing")
            peer["remoteApproved"] = result.get("remoteApproved") is True
            if peer["whoisStatus"] == "missing":
                peer.update(localApproved=False,
                            lastError="Tailscale identity was unavailable; explicitly accept this warning to finish pairing")
            peer["status"] = "approved" if _peer_allowed(peer) else "pending"
            self._save_peer(peer)
        except (OSError, ValueError, RuntimeError, urllib.error.URLError):
            peer["lastError"] = "Connection unavailable; the pairing attempt will retry"
            self._save_peer(peer)
        self.start()
        return self.snapshot()

    def _save_peer(self, peer: FederationPeerRecord) -> None:
        if _peer_allowed(peer):
            peer.pop("inviteId", None)
            peer.pop("inviteToken", None)
        with self.runtime.lock, self.runtime.db() as db:
            db.execute("BEGIN IMMEDIATE")
            db.execute("INSERT INTO runtime_federation_peers(id,record) VALUES(?,?) "
                       "ON CONFLICT(id) DO UPDATE SET record=excluded.record",
                       (peer["stateId"], _json(peer)))

    def _send_pair(self, peer: FederationPeerRecord) -> None:
        local = self._local_identity(create=False)
        if not local:
            raise ValueError("Local federation identity is missing")
        body = {"protocol": PROTOCOL, "inviteId": peer["inviteId"],
                "token": peer["inviteToken"], "stateId": local["stateId"],
                "label": local["label"], "leadName": local["leadName"],
                "origin": RemoteAccess(self.runtime.root).origin(),
                "publicKey": local["publicKey"],
                "tailscaleUser": local.get("tailscaleUser"), "tailscaleStatus": local.get("tailscaleStatus")}
        return self._request(peer["origin"], "/api/federation/v1/pair", body, local,
                             expected_key=peer["publicKey"], expected_state=peer["stateId"])

    def approve_peer(self, state_id: str, accept_missing_whois: bool=False) -> JsonObject:
        with self.runtime.lock, self.runtime.db() as db:
            db.execute("BEGIN IMMEDIATE")
            peer = _record(db.execute("SELECT record FROM runtime_federation_peers WHERE id=?", (state_id,)).fetchone())
            if not peer or peer.get("status") == "revoked":
                raise ValueError("Unknown pending peer")
            if peer.get("whoisStatus") == "missing" and accept_missing_whois is not True:
                raise ValueError("The Tailscale whois result is missing. Explicitly accept the warning to pair.")
            peer.update(localApproved=True, status="approved" if peer.get("remoteApproved") else "pending",
                        updated=time.time())
            db.execute("UPDATE runtime_federation_peers SET record=? WHERE id=?", (_json(peer), state_id))
        if _peer_allowed(peer):
            self.start()
        return self.snapshot()

    def revoke_peer(self, state_id: str) -> JsonObject:
        if not isinstance(state_id, str):
            raise ValueError("Select a peer to revoke")
        with self.runtime.lock, self.runtime.db() as db:
            db.execute("BEGIN IMMEDIATE")
            peer = _record(db.execute("SELECT record FROM runtime_federation_peers WHERE id=?", (state_id,)).fetchone())
            if not peer:
                raise ValueError("Unknown peer")
            peer.update(status="revoked", localApproved=False, remoteApproved=False, revoked=time.time())
            peer.pop("inviteId", None)
            peer.pop("inviteToken", None)
            db.execute("UPDATE runtime_federation_peers SET record=? WHERE id=?", (_json(peer), state_id))
            rows = db.execute("SELECT id,record FROM runtime_federation_outbox WHERE peer_id=?", (state_id,)).fetchall()
            for row in rows:
                record = json.loads(row[1])
                if record.get("status") == "queued":
                    db.execute("DELETE FROM runtime_federation_outbox WHERE id=?", (row[0],))
                    message = db.execute("SELECT deliveries FROM runtime_chat_messages WHERE id=?", (row[0],)).fetchone()
                    if message:
                        deliveries = json.loads(message[0]); deliveries["remote:" + state_id] = "cancelled"
                        db.execute("UPDATE runtime_chat_messages SET deliveries=? WHERE id=?", (_json(deliveries), row[0]))
            db.execute("UPDATE runtime_federation_rooms SET record=json_set(record,'$.status','revoked') "
                       "WHERE json_extract(record,'$.peerId')=?", (state_id,))
            self._sync_peer_rooms(db, state_id)
        return self.snapshot()

    def _request(self, origin: str, path: str, body: JsonObject, identity: FederationIdentityRecord, *, expected_key: str, expected_state: str | None=None, method: str="POST") -> JsonObject:
        raw = _json(body).encode()
        if len(raw) > MAX_BODY:
            raise ValueError("Federation request exceeds the size limit")
        timestamp = int(time.time())
        nonce = secrets.token_urlsafe(24)
        signature = _sign(identity["privateKey"], _request_bytes(method, path, timestamp, nonce, raw))
        request = urllib.request.Request(origin + path, data=raw, method=method, headers={
            "Content-Type": "application/json", "Accept": "application/json",
            "X-Studio-Federation-State": identity["stateId"],
            "X-Studio-Federation-Time": str(timestamp),
            "X-Studio-Federation-Nonce": nonce,
            "X-Studio-Federation-Signature": signature,
        })
        try:
            with urllib.request.urlopen(request, timeout=8) as response:
                payload = response.read(MAX_BODY + 1)
                if len(payload) > MAX_BODY:
                    raise ValueError("Federation response exceeds the size limit")
                wrapper = json.loads(payload)
                if not isinstance(wrapper, dict) or not isinstance(wrapper.get("result"), dict):
                    raise ValueError("Invalid federation response")
                proof = {key: wrapper.get(key) for key in ("stateId", "timestamp", "nonce", "result")}
                if ((expected_state is not None and wrapper.get("stateId") != expected_state)
                        or not _fresh(wrapper.get("timestamp"))
                        or not isinstance(wrapper.get("nonce"), str)
                        or not _verify(expected_key, b"studio-federation-response-v1\n" + _json(proof).encode(),
                                       wrapper.get("signature", ""))):
                    raise ValueError("Federation response identity or signature is invalid")
                return wrapper["result"]
        except urllib.error.HTTPError as error:
            detail = error.read(2048).decode("utf-8", "replace")
            raise ValueError(f"Federation peer rejected the request ({error.code}): {detail[:300]}") from None

    def signed_response(self, result: JsonObject) -> JsonObject:
        identity = self._local_identity(create=False)
        if not identity:
            raise RuntimeError("Federation identity is unavailable")
        proof = {"stateId": identity["stateId"], "timestamp": int(time.time()),
                 "nonce": secrets.token_urlsafe(24), "result": result}
        proof["signature"] = _sign(identity["privateKey"],
                                   b"studio-federation-response-v1\n" + _json(proof).encode())
        return proof

    def route(self, action: str, headers: Mapping[str, str], remote_address: str, raw: bytes) -> JsonObject:
        if not self.enabled():
            raise PermissionError("Federation is disabled on this server")
        if len(raw) > MAX_BODY:
            raise ValueError("Federation request exceeds the size limit")
        try:
            body = json.loads(raw)
        except (UnicodeError, json.JSONDecodeError):
            raise ValueError("Invalid federation JSON") from None
        if not isinstance(body, dict):
            raise ValueError("Federation JSON object required")
        method_path = {"pair": "/api/federation/v1/pair", "message": "/api/federation/v1/message",
                       "pull": "/api/federation/v1/pull", "status": "/api/federation/v1/status"}.get(action)
        if not method_path:
            raise ValueError("Unknown federation endpoint")
        peer_id = headers.get("X-Studio-Federation-State", "")
        timestamp_text = headers.get("X-Studio-Federation-Time", "")
        nonce = headers.get("X-Studio-Federation-Nonce", "")
        signature = headers.get("X-Studio-Federation-Signature", "")
        try:
            timestamp = int(timestamp_text)
        except ValueError:
            raise PermissionError("Invalid federation timestamp") from None
        if not peer_id or len(peer_id) > 100 or not _fresh(timestamp) or not 16 <= len(nonce) <= 128:
            raise PermissionError("Expired or invalid federation request")
        if action == "pair":
            if body.get("protocol") != PROTOCOL:
                raise ValueError("Unsupported federation protocol version")
            invite_id, token = body.get("inviteId"), body.get("token")
            with self.runtime.lock, self.runtime.db() as db:
                invite = _record(db.execute("SELECT record FROM runtime_federation_invites WHERE id=?", (invite_id,)).fetchone())
                identity = _record(db.execute("SELECT record FROM runtime_federation_identity WHERE id='local'").fetchone())
                if (not invite or not identity or invite.get("expires", 0) < time.time()
                        or not hmac.compare_digest(invite.get("tokenHash", ""), hashlib.sha256(str(token).encode()).hexdigest())):
                    raise PermissionError("Pairing invitation is invalid, expired, or already used")
                key = body.get("publicKey")
                if not isinstance(key, str) or not _verify(key, _request_bytes("POST", method_path, timestamp, nonce, raw), signature):
                    raise PermissionError("Invalid pairing signature")
                self._consume_nonce(db, peer_id, nonce, timestamp)
                whois_status = self._validate_whois(invite, body, headers, remote_address)
                if body.get("stateId") != peer_id or not isinstance(body.get("label"), str):
                    raise PermissionError("Pairing identity does not match its signature")
                try:
                    uuid.UUID(peer_id)
                except ValueError:
                    raise PermissionError("Invalid federation state identity") from None
                try:
                    from codex_remote import validate_origin
                    origin = validate_origin(body.get("origin"))
                except ValueError:
                    raise ValueError("Pairing invitation has an invalid server URL") from None
                existing = _record(db.execute("SELECT record FROM runtime_federation_peers WHERE id=?", (peer_id,)).fetchone())
                if existing and existing.get("publicKey") != key:
                    raise PermissionError("A different key is already registered for this server")
                if invite.get("status") != "open" and not (
                        invite.get("status") == "used" and invite.get("usedBy") == peer_id and existing):
                    raise PermissionError("Pairing invitation is invalid, expired, or already used")
                peer = existing or {"stateId": peer_id, "label": body["label"][:80], "origin": origin,
                                    "publicKey": key, "status": "pending", "localApproved": False,
                                    "remoteApproved": True, "created": time.time()}
                peer.update(label=body["label"][:80], origin=origin, remoteApproved=True,
                            whoisStatus=whois_status,
                            whoisUser=self._observed_login(headers, remote_address), updated=time.time())
                peer["status"] = "approved" if (peer.get("localApproved") is True
                                                   and peer.get("remoteApproved") is True) else "pending"
                db.execute("INSERT INTO runtime_federation_peers(id,record) VALUES(?,?) "
                           "ON CONFLICT(id) DO UPDATE SET record=excluded.record", (peer_id, _json(peer)))
                invite["status"] = "used"; invite["usedBy"] = peer_id
                db.execute("UPDATE runtime_federation_invites SET record=? WHERE id=?", (_json(invite), invite_id))
                return self.signed_response({"protocol": PROTOCOL, "accepted": True,
                        "remoteApproved": peer.get("localApproved") is True,
                        "whoisStatus": peer["whoisStatus"], "stateId": identity["stateId"]})
        peer = self._verified_peer(peer_id, method_path, timestamp, nonce, signature, raw,
                                   allow_revoked=action == "status")
        self._validate_peer_origin(peer, headers, remote_address)
        if body.get("protocol") != PROTOCOL:
            raise ValueError("Unsupported federation protocol version")
        if action == "status":
            return self.signed_response({"protocol": PROTOCOL, "approved": _peer_allowed(peer),
                    "revoked": peer.get("status") == "revoked"})
        if action == "message":
            return self.signed_response(self.receive_envelope(peer, body.get("envelope")))
        if action == "pull":
            return self.signed_response(self.pull(peer, body))
        raise ValueError("Unknown federation endpoint")

    def _observed_login(self, headers: Mapping[str, str], remote_address: str) -> str | None:
        return _observed_whois(headers, remote_address)

    def _validate_whois(self, invite: FederationInviteRecord, body: JsonObject, headers: Mapping[str, str], remote_address: str) -> None:
        observed = self._observed_login(headers, remote_address)
        claimed = body.get("tailscaleUser")
        if observed and claimed and observed.lower() != claimed.lower():
            raise PermissionError("Tailscale whois identity mismatch")
        expected = invite.get("expectedUser")
        if observed and expected and observed.lower() != expected.lower():
            raise PermissionError("Tailscale whois identity does not match the invitation")
        if not observed:
            # The user must explicitly accept this warning on the pairing panel.
            return "missing"
        return "verified"

    def _validate_peer_origin(self, peer: FederationPeerRecord, headers: Mapping[str, str], remote_address: str) -> None:
        observed = self._observed_login(headers, remote_address)
        expected = peer.get("whoisUser")
        if observed and expected and observed.lower() != expected.lower():
            raise PermissionError("Tailscale whois identity mismatch")

    def _verified_peer(self, peer_id: str, path: str, timestamp: int, nonce: str, signature: str, raw: bytes, *, allow_revoked: bool=False) -> FederationPeerRecord:
        with self.runtime.lock, self.runtime.db() as db:
            peer = _record(db.execute("SELECT record FROM runtime_federation_peers WHERE id=?", (peer_id,)).fetchone())
            if not _peer_allowed(peer) and not (allow_revoked and peer and peer.get("status") == "revoked"):
                raise PermissionError("Caller is not an approved federation peer")
            if not _verify(peer["publicKey"], _request_bytes("POST", path, timestamp, nonce, raw), signature):
                raise PermissionError("Invalid federation request signature")
            self._consume_nonce(db, peer_id, nonce, timestamp)
            return peer

    @staticmethod
    def _consume_nonce(db: sqlite3.Connection, peer_id: str, nonce: str, timestamp: int) -> bool:
        now = int(time.time())
        db.execute("DELETE FROM runtime_federation_nonces WHERE created<?", (now - MAX_CLOCK_SKEW * 2,))
        recent = db.execute("SELECT count(*) FROM runtime_federation_nonces WHERE peer_id=? AND created>=?",
                            (peer_id, now - 60)).fetchone()[0]
        if recent >= 240:
            raise PermissionError("Federation peer rate limit exceeded")
        try:
            db.execute("INSERT INTO runtime_federation_nonces(peer_id,nonce,created) VALUES(?,?,?)",
                       (peer_id, nonce, timestamp))
        except sqlite3.IntegrityError:
            raise PermissionError("Repeated federation nonce") from None

    def create_room(self, body: JsonObject) -> JsonObject:
        peer_id = body.get("peer_id")
        local_members = body.get("local_members")
        share_names = body.get("share_names", False)
        share_status = body.get("share_status", False)
        if type(share_names) is not bool or type(share_status) is not bool:
            raise ValueError("Room disclosure options must be booleans")
        if not isinstance(local_members, list) or not local_members or len(local_members) > 50:
            raise ValueError("Select one or more local room members")
        with self.runtime.lock, self.runtime.db() as db:
            db.execute("BEGIN IMMEDIATE")
            peer = _record(db.execute("SELECT record FROM runtime_federation_peers WHERE id=?", (peer_id,)).fetchone())
            if not self.enabled(db) or not _peer_allowed(peer):
                raise ValueError("Select an approved federation peer")
            local = self.ensure_identity(db)
            unique_members = sorted(set(local_members))
            if len(unique_members) != len(local_members):
                raise ValueError("Room members must be unique")
            agents = {key: agent for key, agent in self.runtime.named_agents(db, unique_members).items()
                      if not agent.get("deletedAt")}
            roots = set()
            members = []
            for agent_id in unique_members:
                agent = agents.get(agent_id)
                if not agent:
                    raise ValueError("Select live local agents only")
                roots.add(agent.get("rootId"))
                if agent.get("isLead") or share_names:
                    participant = {"id": agent_id, "role": "lead" if agent.get("isLead") else "agent"}
                    if agent.get("isLead") or share_names:
                        participant["name"] = agent.get("name", "Agent")
                    if share_status:
                        participant["status"] = agent.get("status", "idle")
                    members.append(participant)
                elif not share_names:
                    raise ValueError("Sharing additional agent membership requires opting in to their names")
            if not any(agents[i].get("isLead") for i in unique_members):
                raise ValueError("Include the local lead in a shared room")
            if len(roots) != 1 or agents[next(i for i in unique_members if agents[i].get("isLead"))]["id"] not in roots:
                raise ValueError("Room members must belong to one local team")
            room_id = "federated:" + str(uuid.uuid4())
            now = time.time()
            room = {"id": room_id, "kind": "federated", "peerId": peer_id,
                    "peerLabel": peer["label"], "name": f"{peer['label']} · Shared room",
                    "status": "pending", "localMembers": unique_members,
                    "remoteMembers": [], "shareNames": share_names,
                    "shareStatus": share_status, "remoteApproved": False,
                    "localApproved": True, "created": now, "updated": now,
                    "sequence": 0, "remoteSequence": 0}
            room["localParticipants"] = members
            db.execute("INSERT INTO runtime_federation_rooms(id,record) VALUES(?,?)", (room_id, _json(room)))
            self.runtime.put(db, "rooms", {"id": room_id, "kind": "federated",
                "members": unique_members, "updated": now, "name": room["name"],
                "federation": True, "peerId": peer_id, "peerLabel": peer["label"]})
            payload = {"room": room_id, "participants": members,
                       "shareNames": share_names, "shareStatus": share_status}
            self._queue_locked(db, peer, local, room, "room-invite", payload, "room:" + room_id)
        self.start()
        return self.snapshot()

    def approve_room(self, body: JsonObject) -> JsonObject:
        room_id = body.get("room_id")
        local_members = body.get("local_members")
        if not isinstance(local_members, list) or not local_members or len(local_members) > 50:
            raise ValueError("Select local members for the room")
        with self.runtime.lock, self.runtime.db() as db:
            db.execute("BEGIN IMMEDIATE")
            room = _record(db.execute("SELECT record FROM runtime_federation_rooms WHERE id=?", (room_id,)).fetchone())
            if not room or room.get("status") != "pending" or room.get("localApproved"):
                raise ValueError("This shared room is unavailable or already approved")
            peer = _record(db.execute("SELECT record FROM runtime_federation_peers WHERE id=?", (room["peerId"],)).fetchone())
            if not _peer_allowed(peer):
                raise ValueError("The federation peer is not approved")
            members = sorted(set(local_members))
            if len(members) != len(local_members):
                raise ValueError("Room members must be unique")
            agents = {key: agent for key, agent in self.runtime.named_agents(db, members).items()
                      if not agent.get("deletedAt")}
            if not all(agent in agents for agent in members):
                raise ValueError("Select live local agents only")
            if not any(agents[agent].get("isLead") for agent in members):
                raise ValueError("Include the local lead in a shared room")
            roots = {agents[agent].get("rootId") for agent in members}
            root_id = next(iter(roots)) if len(roots) == 1 else None
            team_root = self.runtime.named_agents(db, [root_id]).get(root_id) if root_id else None
            if len(roots) != 1 or not team_root or not team_root.get("isLead") or team_root.get("deletedAt"):
                raise ValueError("Room members must belong to one local team")
            local = self.ensure_identity(db)
            share_names = body.get("share_names", False)
            share_status = body.get("share_status", False)
            if type(share_names) is not bool or type(share_status) is not bool:
                raise ValueError("Room disclosure options must be booleans")
            if not share_names and any(not agents[agent].get("isLead") for agent in members):
                raise ValueError("Sharing additional agents requires opting in to their names")
            participants = []
            for agent_id in members:
                agent = agents[agent_id]
                lead = agent.get("isLead") is True
                entry = {"id": agent_id, "role": "lead" if lead else "agent"}
                if lead or share_names:
                    entry["name"] = agent.get("name", "Agent")
                if share_status:
                    entry["status"] = agent.get("status", "idle")
                participants.append(entry)
            room.update(localMembers=members, localApproved=True, remoteApproved=False,
                        shareNames=share_names, shareStatus=share_status,
                        localParticipants=participants, status="pending", updated=time.time())
            db.execute("UPDATE runtime_federation_rooms SET record=? WHERE id=?", (_json(room), room_id))
            standard = {"id": room_id, "kind": "federated", "members": members,
                        "updated": room["updated"], "name": room["name"], "federation": True,
                        "peerId": room["peerId"], "peerLabel": room["peerLabel"]}
            self.runtime.put(db, "rooms", standard)
            self._queue_locked(db, peer, local, room, "room-accept",
                {"room": room_id, "participants": participants,
                 "shareNames": share_names, "shareStatus": share_status}, "room-accept:" + room_id)
        self.start()
        return self.snapshot()

    def room_options(self, body: JsonObject) -> JsonObject:
        room_id = body.get("room_id")
        with self.runtime.lock, self.runtime.db() as db:
            db.execute("BEGIN IMMEDIATE")
            room = _record(db.execute("SELECT record FROM runtime_federation_rooms WHERE id=?", (room_id,)).fetchone())
            if not room or room.get("status") not in {"approved", "pending"}:
                raise ValueError("Shared room is unavailable")
            disclosure = body.get("disclosure")
            if not isinstance(disclosure, dict) or set(disclosure) - {"shareNames", "shareStatus"}:
                raise ValueError("Invalid room disclosure settings")
            if any(type(value) is not bool for value in disclosure.values()):
                raise ValueError("Room disclosure settings must be booleans")
            room.update({key: disclosure.get(key, room.get(key, False)) for key in ("shareNames", "shareStatus")}, updated=time.time())
            db.execute("UPDATE runtime_federation_rooms SET record=? WHERE id=?", (_json(room), room_id))
        return self.snapshot()

    def _queue_locked(self, db: sqlite3.Connection, peer: FederationPeerRecord, identity: FederationIdentityRecord, room: RoomRecord, kind: str, payload: JsonObject, key: str) -> str:
        if not _peer_allowed(peer) or not self.enabled(db):
            raise ValueError("Federation is disabled or this peer is not approved")
        # Idempotent queue IDs make retries and restart recovery exact.
        message_id = str(uuid.uuid5(uuid.UUID(identity["stateId"]), key))
        value = {"id": message_id, "kind": kind, "room": room["id"],
                 "payload": payload, "created": time.time(), "status": "queued",
                 "attempts": 0, "nextAt": 0, "error": None}
        existing = _record(db.execute("SELECT record FROM runtime_federation_outbox WHERE id=?", (message_id,)).fetchone())
        if existing:
            if existing.get("kind") != kind or existing.get("payload") != payload:
                raise ValueError("Federation message ID already has different content")
            return message_id
        pending = db.execute("SELECT count(*),COALESCE(sum(length(record)),0) FROM runtime_federation_outbox "
                             "WHERE peer_id=? AND json_extract(record,'$.status')='queued'",
                             (peer["stateId"],)).fetchone()
        if pending[0] >= 10000 or pending[1] + len(_json(value).encode()) > 32 * 1024 * 1024:
            raise ValueError("Federation queue is full. Retry after earlier messages are delivered.")
        db.execute("INSERT INTO runtime_federation_outbox(id,peer_id,record) VALUES(?,?,?)",
                   (message_id, peer["stateId"], _json(value)))
        return message_id

    def send_message(self, sender_id: str, room_id: str, text: str, key: str) -> JsonObject:
        if not isinstance(text, str) or not 1 <= len(text.strip()) <= MAX_MESSAGE:
            raise ValueError("Message must have 1 to 12000 characters")
        text = text.strip()
        with self.runtime.lock, self.runtime.db() as db:
            db.execute("BEGIN IMMEDIATE")
            room = _record(db.execute("SELECT record FROM runtime_federation_rooms WHERE id=?", (room_id,)).fetchone())
            if (not self.enabled(db) or not room or room.get("status") != "approved"
                    or sender_id not in room.get("localMembers", [])):
                raise ValueError("Chat is unavailable or you are not a room participant")
            peer = _record(db.execute("SELECT record FROM runtime_federation_peers WHERE id=?", (room["peerId"],)).fetchone())
            if not _peer_allowed(peer):
                raise ValueError("Federation peer is unavailable")
            sender = self.runtime.agent(sender_id, db)
            participant = next((p for p in room.get("localParticipants", []) if p["id"] == sender_id), None)
            if sender_id not in room["localMembers"]:
                raise ValueError("Sender is not a participant in this room")
            name = participant.get("name") if participant else None
            if name is None and sender.get("isLead"):
                name = sender.get("name", "Lead")
            room["sequence"] = room.get("sequence", 0) + 1
            created = time.time()
            local = self.ensure_identity(db)
            message_id = str(uuid.uuid5(uuid.UUID(local["stateId"]), key))
            payload = {"sender": sender_id, "senderName": name, "text": text,
                       "created": created, "sequence": room["sequence"]}
            existing = db.execute("SELECT * FROM runtime_chat_messages WHERE id=?", (message_id,)).fetchone()
            if existing:
                if (existing["room"], existing["sender"], existing["text"]) != (room_id, sender_id, text):
                    raise ValueError("This message ID has different content")
                deliveries = json.loads(existing["deliveries"])
                status = "delivered" if deliveries and all(value == "delivered" for value in deliveries.values()) else "queued"
                return {"id": message_id, "room": room_id, "deliveries": deliveries, "status": status}
            db.execute("INSERT INTO runtime_chat_messages(id,room,sender,text,created,deliveries) VALUES(?,?,?,?,?,?)",
                       (message_id, room_id, sender_id, text, created, _json({"remote:" + room["peerId"]: "queued"})))
            self.runtime.put(db, "rooms", {"id": room_id, "kind": "federated",
                "members": room["localMembers"], "updated": created, "name": room["name"],
                "federation": True, "peerId": room["peerId"], "peerLabel": room["peerLabel"]},
                )
            event_text = _json({"room": room_id, "message_id": message_id,
                                "sender": sender_id, "sender_name": name,
                                "text": text, "untrusted_remote": False,
                                "authority": "room-data-only"})
            for member in room["localMembers"]:
                if member == sender_id:
                    continue
                recipient = self.runtime.agent(member, db)
                if not recipient.get("deletedAt"):
                    self.runtime.enqueue(db, recipient, "agent_message", event_text,
                        "federation-local:" + room_id + ":" + message_id + ":" + member)
            self._queue_locked(db, peer, local, room, "chat-message", payload, key)
            room["updated"] = created
            db.execute("UPDATE runtime_federation_rooms SET record=? WHERE id=?", (_json(room), room_id))
        self.start()
        return {"id": message_id, "room": room_id,
                "deliveries": {"remote:" + room["peerId"]: "queued"}, "status": "queued"}

    def user_message(self, room_id: str, text: str, key: str) -> JsonObject:
        with self.runtime.lock, self.runtime.db() as db:
            room = _record(db.execute("SELECT record FROM runtime_federation_rooms WHERE id=?", (room_id,)).fetchone())
            if not room or room.get("status") != "approved":
                raise ValueError("Shared room is unavailable")
            leads = [a["id"] for a in self.runtime.named_agents(
                db, room.get("localMembers", [])).values()
                     if a.get("isLead") and not a.get("deletedAt")]
            if not leads:
                raise ValueError("The local lead is not a room participant")
            sender = leads[0]
        return self.send_message(sender, room_id, text, key)

    def has_room(self, room_id: str) -> bool:
        if not isinstance(room_id, str) or not room_id.startswith("federated:"):
            return False
        with self.runtime.read_db() as db:
            return db.execute("SELECT 1 FROM runtime_federation_rooms WHERE id=?", (room_id,)).fetchone() is not None

    def rooms_for_peer(self, agent_id: str, peer_id: str) -> list[JsonObject]:
        with self.runtime.read_db() as db:
            rows = db.execute("SELECT record FROM runtime_federation_rooms WHERE json_extract(record,'$.peerId')=?",
                              (peer_id,)).fetchall()
            return [room for room in (json.loads(row[0]) for row in rows)
                    if room.get("status") == "approved" and agent_id in room.get("localMembers", [])]

    def model_peers(self, db: sqlite3.Connection, agent_id: str) -> list[FederationPeerRecord]:
        if not self.enabled(db):
            return []
        rooms = [json.loads(row[0]) for row in db.execute(
            "SELECT record FROM runtime_federation_rooms WHERE json_extract(record,'$.status')='approved'")]
        visible = {room.get("peerId") for room in rooms
                   if agent_id in room.get("localMembers", []) and room.get("localApproved")
                   and room.get("remoteApproved")}
        result = []
        for state_id in sorted(key for key in visible if key):
            peer = _record(db.execute("SELECT record FROM runtime_federation_peers WHERE id=?", (state_id,)).fetchone())
            if _peer_allowed(peer):
                result.append({"id": "remote:" + state_id,
                               "name": peer.get("leadName") or peer.get("label"),
                               "serverLabel": peer.get("label"), "remote": True,
                               "status": "available"})
        return result

    def _hash_envelope(self, envelope: JsonObject) -> str:
        immutable = {key: value for key, value in envelope.items()
                     if key not in {"timestamp", "nonce", "signature"}}
        return hashlib.sha256(_json(immutable).encode()).hexdigest()

    def _outbound_envelope(self, outbox_record: FederationOutboxRecord, local: FederationIdentityRecord) -> JsonObject:
        envelope = {"protocol": PROTOCOL, "senderServer": local["stateId"],
                    "id": outbox_record["id"], "kind": outbox_record["kind"],
                    "room": outbox_record["room"], "payload": outbox_record["payload"],
                    "created": outbox_record["created"], "timestamp": int(time.time()),
                    "nonce": secrets.token_urlsafe(24)}
        envelope["signature"] = _sign(local["privateKey"], _message_bytes(envelope))
        return envelope

    def receive_envelope(self, peer: FederationPeerRecord, envelope: JsonObject) -> JsonObject:
        if not isinstance(envelope, dict) or envelope.get("protocol") != PROTOCOL:
            raise ValueError("Unsupported federation message version")
        timestamp, nonce = envelope.get("timestamp"), envelope.get("nonce")
        if not _fresh(timestamp) or not isinstance(nonce, str) or not 16 <= len(nonce) <= 128:
            raise PermissionError("Expired or invalid federation message")
        if envelope.get("senderServer") != peer["stateId"] or not _verify(
                peer["publicKey"], _message_bytes(envelope), envelope.get("signature", "")):
            raise PermissionError("Invalid federation message signature")
        message_id, kind, room_id = envelope.get("id"), envelope.get("kind"), envelope.get("room")
        if (not isinstance(message_id, str) or len(message_id) > 128 or
                not isinstance(room_id, str) or len(room_id) > 128 or
                kind not in {"chat-message", "room-invite", "room-accept"}):
            raise ValueError("Invalid federation message envelope")
        payload_hash = self._hash_envelope(envelope)
        with self.runtime.lock, self.runtime.db() as db:
            db.execute("BEGIN IMMEDIATE")
            current_peer = _record(db.execute("SELECT record FROM runtime_federation_peers WHERE id=?", (peer["stateId"],)).fetchone())
            if not self.enabled(db) or not _peer_allowed(current_peer):
                raise PermissionError("Federation is disabled or the peer was revoked")
            self._consume_nonce(db, peer["stateId"], nonce, timestamp)
            previous = _record(db.execute("SELECT record FROM runtime_federation_inbox WHERE peer_id=? AND message_id=?",
                                          (peer["stateId"], message_id)).fetchone())
            if previous:
                if previous.get("hash") != payload_hash:
                    raise ValueError("Federation message ID was replayed with different content")
                return previous["receipt"]
            payload = envelope.get("payload")
            if not isinstance(payload, dict):
                raise ValueError("Invalid federation message payload")
            if kind == "room-invite":
                if payload.get("room") != room_id:
                    raise ValueError("Room identity does not match its invitation")
                participants = payload.get("participants")
                if (set(payload) != {"room", "participants", "shareNames", "shareStatus"}
                        or type(payload.get("shareNames")) is not bool
                        or type(payload.get("shareStatus")) is not bool
                        or not self.valid_participants(participants,
                            share_names=payload["shareNames"], share_status=payload["shareStatus"])):
                    raise ValueError("Invalid room participant list")
                existing = _record(db.execute("SELECT record FROM runtime_federation_rooms WHERE id=?", (room_id,)).fetchone())
                if existing and existing.get("peerId") != peer["stateId"]:
                    raise PermissionError("Room ID is already owned by another peer")
                room = existing or {"id": room_id, "kind": "federated", "peerId": peer["stateId"],
                    "peerLabel": peer["label"], "name": f"{peer['label']} · Shared room",
                    "status": "pending", "localMembers": [], "remoteMembers": participants,
                    "localParticipants": [], "shareNames": payload.get("shareNames") is True,
                    "shareStatus": payload.get("shareStatus") is True, "localApproved": False,
                    "remoteApproved": True, "created": time.time(), "updated": time.time(),
                    "sequence": 0, "remoteSequence": 0}
                room.update(remoteMembers=participants, remoteApproved=True, updated=time.time())
                db.execute("INSERT INTO runtime_federation_rooms(id,record) VALUES(?,?) "
                           "ON CONFLICT(id) DO UPDATE SET record=excluded.record", (room_id, _json(room)))
                self.runtime.put(db, "rooms", {"id": room_id, "kind": "federated", "members": [],
                    "updated": room["updated"], "name": room["name"], "federation": True,
                    "peerId": peer["stateId"], "peerLabel": peer["label"]})
            elif kind == "room-accept":
                room = _record(db.execute("SELECT record FROM runtime_federation_rooms WHERE id=? AND json_extract(record,'$.peerId')=?",
                                          (room_id, peer["stateId"])).fetchone())
                participants = payload.get("participants")
                if (payload.get("room") != room_id or not room or not room.get("localApproved")
                        or set(payload) != {"room", "participants", "shareNames", "shareStatus"}
                        or type(payload.get("shareNames")) is not bool
                        or type(payload.get("shareStatus")) is not bool
                        or not self.valid_participants(participants,
                            share_names=payload["shareNames"], share_status=payload["shareStatus"])):
                    raise PermissionError("Room is not awaiting this peer's approval")
                room.update(remoteMembers=participants, remoteApproved=True, status="approved", updated=time.time())
                db.execute("UPDATE runtime_federation_rooms SET record=? WHERE id=?", (_json(room), room_id))
                self._sync_room_entity(db, room_id)
            else:
                receipt = self._receive_chat_locked(db, peer, room_id, envelope, payload)
                db.execute("INSERT INTO runtime_federation_inbox(peer_id,message_id,record) VALUES(?,?,?)",
                           (peer["stateId"], message_id,
                            _json({"hash": payload_hash, "receipt": receipt, "ack": "pending",
                                   "kind": kind, "room": room_id, "created": time.time()})))
                return receipt
            receipt = {"protocol": PROTOCOL, "messageId": message_id, "accepted": True,
                       "room": room_id, "sequence": None}
            db.execute("INSERT INTO runtime_federation_inbox(peer_id,message_id,record) VALUES(?,?,?)",
                       (peer["stateId"], message_id,
                        _json({"hash": payload_hash, "receipt": receipt, "ack": "pending", "created": time.time()})))
            return receipt

    @staticmethod
    def valid_participants(participants: list[JsonObject], *, share_names: bool=False, share_status: bool=False) -> list[JsonObject]:
        if not isinstance(participants, list) or not 1 <= len(participants) <= 50:
            return False
        seen = set()
        for person in participants:
            if (not isinstance(person, dict) or not isinstance(person.get("id"), str)
                    or len(person["id"]) > 128 or person["id"] in seen
                    or person.get("role") not in {"lead", "agent"}
                    or set(person) - {"id", "role", "name", "status"}
                    or ("name" in person and (not isinstance(person["name"], str) or len(person["name"]) > 80))
                    or ("name" in person and person.get("role") != "lead" and not share_names)
                    or ("status" in person and (not share_status or not isinstance(person["status"], str)
                                                  or len(person["status"]) > 40))):
                return False
            seen.add(person["id"])
        return True

    def _receive_chat_locked(self, db: sqlite3.Connection, peer: FederationPeerRecord, room_id: str, envelope: JsonObject, payload: JsonObject) -> None:
        text = payload.get("text")
        sender = payload.get("sender")
        if not isinstance(text, str) or not 1 <= len(text) <= MAX_MESSAGE or not isinstance(sender, str):
            raise ValueError("Invalid room message")
        room = _record(db.execute("SELECT record FROM runtime_federation_rooms WHERE id=? AND json_extract(record,'$.peerId')=?",
                                  (room_id, peer["stateId"])).fetchone())
        participant = next((person for person in room.get("remoteMembers", []) if person.get("id") == sender), None) if room else None
        if not room or room.get("status") != "approved" or not room.get("localApproved") or not room.get("remoteApproved") or not participant:
            raise PermissionError("Remote sender is not an approved room member")
        expected_name = participant.get("name")
        if payload.get("senderName") != expected_name:
            raise PermissionError("Sender name is not approved for this room")
        sequence = payload.get("sequence")
        if type(sequence) is not int or sequence < 1 or sequence <= room.get("remoteSequence", 0):
            raise ValueError("Remote room message ordering is invalid")
        rate = db.execute("SELECT count(*) FROM runtime_federation_inbox WHERE peer_id=? "
                          "AND json_extract(record,'$.room')=? "
                          "AND json_extract(record,'$.kind')='chat-message' "
                          "AND json_extract(record,'$.created')>=?",
                          (peer["stateId"], room_id, time.time() - 60)).fetchone()[0]
        if rate >= 30:
            raise PermissionError("Federation room rate limit exceeded")
        row_id = "federated:" + hashlib.sha256((peer["stateId"] + ":" + envelope["id"]).encode()).hexdigest()
        existing = db.execute("SELECT seq,text,sender FROM runtime_chat_messages WHERE id=?", (row_id,)).fetchone()
        if existing:
            if (existing["text"], existing["sender"]) != (text, sender):
                raise ValueError("Federation message ID has conflicting content")
            return {"protocol": PROTOCOL, "messageId": envelope["id"], "accepted": True,
                    "room": room_id, "sequence": existing["seq"]}
        created = payload.get("created")
        if not isinstance(created, (int, float)) or created <= 0:
            raise ValueError("Invalid message timestamp")
        display_sender = f"remote:{peer['stateId']}:{sender}"
        row = db.execute("INSERT INTO runtime_chat_messages(id,room,sender,text,created,deliveries) VALUES(?,?,?,?,?,?)",
                         (row_id, room_id, display_sender, text, created, "{}"))
        room.update(remoteSequence=sequence, updated=max(float(created), time.time()))
        db.execute("UPDATE runtime_federation_rooms SET record=? WHERE id=?", (_json(room), room_id))
        standard = {"id": room_id, "kind": "federated", "members": room["localMembers"],
                    "updated": room["updated"], "name": room["name"], "federation": True,
                    "peerId": peer["stateId"], "peerLabel": peer["label"]}
        self.runtime.put(db, "rooms", standard)
        # Remote text is explicitly labeled as untrusted data before entering a model turn.
        event_text = _json({"room": room_id, "message_id": envelope["id"],
                            "sender": display_sender, "sender_name": expected_name or peer["label"],
                            "text": text, "untrusted_remote": True,
                            "authority": "room-data-only"})
        for member in room["localMembers"]:
            agent = self.runtime.agent(member, db)
            if agent.get("deletedAt"):
                continue
            self.runtime.enqueue(db, agent, "agent_message", event_text,
                                 "federation:" + peer["stateId"] + ":" + envelope["id"] + ":" + member)
        return {"protocol": PROTOCOL, "messageId": envelope["id"], "accepted": True,
                "room": room_id, "sequence": row.lastrowid}

    def pull(self, peer: FederationPeerRecord, body: JsonObject) -> JsonObject:
        acks = body.get("acks", [])
        if not isinstance(acks, list) or len(acks) > MAX_BATCH:
            raise ValueError("Invalid federation receipt batch")
        with self.runtime.lock, self.runtime.db() as db:
            db.execute("BEGIN IMMEDIATE")
            acknowledged = []
            for ack in acks:
                if not isinstance(ack, dict) or not isinstance(ack.get("id"), str) or not isinstance(ack.get("hash"), str):
                    raise ValueError("Invalid federation receipt")
                row = db.execute("SELECT record FROM runtime_federation_outbox WHERE id=? AND peer_id=?",
                                 (ack["id"], peer["stateId"])).fetchone()
                item = _record(row)
                if item and item.get("hash") == ack["hash"]:
                    item.update(status="delivered", delivered=time.time(), error=None)
                    db.execute("UPDATE runtime_federation_outbox SET record=? WHERE id=?", (_json(item), ack["id"]))
                    acknowledged.append({"id": ack["id"], "hash": ack["hash"]})
            local = self.ensure_identity(db)
            rows = db.execute("SELECT id,record FROM runtime_federation_outbox WHERE peer_id=? "
                              "AND json_extract(record,'$.status')='queued' ORDER BY json_extract(record,'$.created'),id LIMIT ?",
                              (peer["stateId"], MAX_BATCH)).fetchall()
            messages: list[JsonObject] = []
            size = 0
            for row in rows:
                item = json.loads(row[1])
                envelope = self._outbound_envelope(item, local)
                encoded = len(_json(envelope).encode())
                if messages and size + encoded > MAX_BATCH_BYTES:
                    break
                messages.append(envelope); size += encoded
                item["hash"] = self._hash_envelope(envelope)
                db.execute("UPDATE runtime_federation_outbox SET record=? WHERE id=?",
                           (_json(item), row[0]))
            inbox = db.execute("SELECT message_id,record FROM runtime_federation_inbox WHERE peer_id=? "
                               "AND json_extract(record,'$.ack') IN ('pending','sent') ORDER BY json_extract(record,'$.created') LIMIT ?",
                               (peer["stateId"], MAX_BATCH)).fetchall()
            receipts = [{"id": row[0], "hash": json.loads(row[1])["hash"]} for row in inbox]
            for row in inbox:
                item = json.loads(row[1]); item["ack"] = "sent"
                db.execute("UPDATE runtime_federation_inbox SET record=? WHERE peer_id=? AND message_id=?",
                           (_json(item), peer["stateId"], row[0]))
        return {"protocol": PROTOCOL, "messages": messages, "acks": receipts,
                "acknowledged": acknowledged}

    def pump_once(self) -> None:
        with self.runtime.read_db() as db:
            peers = [_record(row) for row in db.execute("SELECT record FROM runtime_federation_peers")]
        for peer in peers:
            if self.stop_event.is_set() or not self.enabled():
                return
            if peer.get("status") == "revoked" or not peer.get("localApproved"):
                continue
            try:
                if peer.get("status") != "approved":
                    paired = self._send_pair(peer)
                    peer["remoteApproved"] = paired.get("remoteApproved") is True
                    peer["status"] = "approved" if (peer.get("localApproved") is True
                                                       and peer.get("remoteApproved") is True) else "pending"
                    peer["updated"] = time.time()
                    self._save_peer(peer)
                response = self._request(peer["origin"], "/api/federation/v1/status", {"protocol": PROTOCOL},
                                         self._local_identity(create=False), expected_key=peer["publicKey"],
                                         expected_state=peer["stateId"])
                if response.get("protocol") != PROTOCOL:
                    raise ValueError("Remote server uses an unsupported federation protocol")
                if response.get("revoked") is True:
                    self._mark_peer_revoked(peer["stateId"])
                    continue
                if response.get("approved") is True:
                    peer["remoteApproved"] = True
                    peer["status"] = "approved"
                    peer["updated"] = time.time()
                    self._save_peer(peer)
                if not _peer_allowed(peer):
                    continue
                self._deliver_peer(peer)
            except (OSError, ValueError, RuntimeError, urllib.error.URLError) as error:
                self._set_peer_error(peer["stateId"], str(error)[:240])

    def _deliver_peer(self, peer: FederationPeerRecord) -> None:
        local = self._local_identity(create=False)
        if not local:
            return
        with self.runtime.read_db() as db:
            queued = db.execute("SELECT id,record FROM runtime_federation_outbox WHERE peer_id=? "
                                 "AND json_extract(record,'$.status')='queued' "
                                 "AND COALESCE(json_extract(record,'$.nextAt'),0)<=? "
                                 "ORDER BY json_extract(record,'$.created'),id LIMIT ?",
                                 (peer["stateId"], time.time(), MAX_BATCH)).fetchall()
            inbox = db.execute("SELECT message_id,record FROM runtime_federation_inbox WHERE peer_id=? "
                               "AND json_extract(record,'$.ack') IN ('pending','sent') ORDER BY json_extract(record,'$.created') LIMIT ?",
                               (peer["stateId"], MAX_BATCH)).fetchall()
        for row in queued:
            record = json.loads(row[1])
            envelope = self._outbound_envelope(record, local)
            body = {"protocol": PROTOCOL, "envelope": envelope}
            try:
                result = self._request(peer["origin"], "/api/federation/v1/message", body, local,
                                       expected_key=peer["publicKey"], expected_state=peer["stateId"])
                self._save_delivery(peer, row[0], record, result, self._hash_envelope(envelope))
            except (OSError, ValueError, RuntimeError, urllib.error.URLError) as error:
                self._retry_outbox(row[0], record, str(error))
                return
        acks = [{"id": row[0], "hash": json.loads(row[1])["hash"]} for row in inbox]
        response = self._request(peer["origin"], "/api/federation/v1/pull",
                                 {"protocol": PROTOCOL, "acks": acks}, local,
                                 expected_key=peer["publicKey"], expected_state=peer["stateId"])
        if (response.get("protocol") != PROTOCOL or not isinstance(response.get("messages"), list)
                or not isinstance(response.get("acks"), list)
                or not isinstance(response.get("acknowledged"), list)):
            raise ValueError("Invalid federation pull response")
        for ack in response["acks"][:MAX_BATCH]:
            if isinstance(ack, dict) and isinstance(ack.get("id"), str) and isinstance(ack.get("hash"), str):
                self._confirm_outbox_ack(peer, ack["id"], ack["hash"])
        for ack in response["acknowledged"][:MAX_BATCH]:
            if isinstance(ack, dict) and isinstance(ack.get("id"), str) and isinstance(ack.get("hash"), str):
                self._confirm_inbox_ack(peer["stateId"], ack["id"], ack["hash"])
        for envelope in response["messages"][:MAX_BATCH]:
            receipt = self._receive_envelope(peer, envelope)
            self._record_ack(peer["stateId"], envelope["id"], self._hash_envelope(envelope), receipt)

    def _receive_envelope(self, peer: FederationPeerRecord, envelope: JsonObject) -> JsonObject:
        # Pulled envelopes carry the same Ed25519 signature and nonce contract
        # as directly delivered messages.
        return self.receive_envelope(peer, envelope)

    def _save_delivery(self, peer: FederationPeerRecord, row_id: str, record: FederationOutboxRecord, response: JsonObject, message_hash: str) -> None:
        if (response.get("protocol") != PROTOCOL or response.get("messageId") != record["id"]
                or response.get("accepted") is not True):
            raise ValueError("Remote server did not provide a durable message receipt")
        with self.runtime.lock, self.runtime.db() as db:
            db.execute("BEGIN IMMEDIATE")
            latest = _record(db.execute("SELECT record FROM runtime_federation_outbox WHERE id=? AND peer_id=?",
                                        (row_id, peer["stateId"])).fetchone())
            if latest and latest.get("status") == "queued":
                latest.update(status="delivered", delivered=time.time(), hash=message_hash, error=None,
                              receipt=response)
                db.execute("UPDATE runtime_federation_outbox SET record=? WHERE id=?", (_json(latest), row_id))
                self._confirm_room_accept_locked(db, latest)
            message = db.execute("SELECT deliveries FROM runtime_chat_messages WHERE id=?", (row_id,)).fetchone()
            if message:
                deliveries = json.loads(message[0]); deliveries["remote:" + peer["stateId"]] = "delivered"
                db.execute("UPDATE runtime_chat_messages SET deliveries=? WHERE id=?", (_json(deliveries), row_id))

    def _record_ack(self, peer_id: str, message_id: str, message_hash: str, receipt: JsonObject) -> None:
        with self.runtime.lock, self.runtime.db() as db:
            db.execute("BEGIN IMMEDIATE")
            existing = _record(db.execute("SELECT record FROM runtime_federation_inbox WHERE peer_id=? AND message_id=?",
                                          (peer_id, message_id)).fetchone())
            record = {"hash": message_hash, "receipt": receipt, "ack": "pending", "created": time.time()}
            if existing and existing.get("hash") != message_hash:
                raise ValueError("Remote message ID was replayed with different content")
            if not existing:
                db.execute("INSERT INTO runtime_federation_inbox(peer_id,message_id,record) VALUES(?,?,?)",
                           (peer_id, message_id, _json(record)))

    def _confirm_outbox_ack(self, peer: FederationPeerRecord, message_id: str, message_hash: str) -> None:
        with self.runtime.lock, self.runtime.db() as db:
            db.execute("BEGIN IMMEDIATE")
            record = _record(db.execute("SELECT record FROM runtime_federation_outbox WHERE id=? AND peer_id=?",
                                        (message_id, peer["stateId"])).fetchone())
            if record and record.get("hash") == message_hash:
                record.update(status="delivered", delivered=time.time(), error=None)
                db.execute("UPDATE runtime_federation_outbox SET record=? WHERE id=?", (_json(record), message_id))
                self._confirm_room_accept_locked(db, record)
                row = db.execute("SELECT deliveries FROM runtime_chat_messages WHERE id=?", (message_id,)).fetchone()
                if row:
                    deliveries = json.loads(row[0]); deliveries["remote:" + peer["stateId"]] = "delivered"
                    db.execute("UPDATE runtime_chat_messages SET deliveries=? WHERE id=?", (_json(deliveries), message_id))

    def _sync_room_entity(self, db: sqlite3.Connection, room_id: str) -> bool:
        """Refresh the durable room view after a federation source update."""
        return self.runtime.sync_room_entity(db, room_id, tombstone_unavailable=True)

    def _sync_peer_rooms(self, db: sqlite3.Connection, peer_id: str) -> None:
        ids = [row[0] for row in db.execute(
            "SELECT id FROM runtime_federation_rooms WHERE json_extract(record,'$.peerId')=?", (peer_id,))]
        for room_id in ids:
            self._sync_room_entity(db, room_id)

    def _confirm_room_accept_locked(self, db: sqlite3.Connection, outbox: FederationOutboxRecord) -> None:
        if outbox.get("kind") != "room-accept":
            return
        row = db.execute("SELECT record FROM runtime_federation_rooms WHERE id=?",
                         (outbox.get("room"),)).fetchone()
        room = _record(row)
        if room and room.get("localApproved"):
            room.update(remoteApproved=True, status="approved", updated=time.time())
            db.execute("UPDATE runtime_federation_rooms SET record=? WHERE id=?",
                       (_json(room), room["id"]))
            self._sync_room_entity(db, room["id"])

    def _confirm_inbox_ack(self, peer_id: str, message_id: str, message_hash: str) -> None:
        with self.runtime.lock, self.runtime.db() as db:
            db.execute("BEGIN IMMEDIATE")
            row = db.execute("SELECT record FROM runtime_federation_inbox WHERE peer_id=? AND message_id=?",
                             (peer_id, message_id)).fetchone()
            record = _record(row)
            if record and record.get("hash") == message_hash:
                record["ack"] = "confirmed"
                db.execute("UPDATE runtime_federation_inbox SET record=? WHERE peer_id=? AND message_id=?",
                           (_json(record), peer_id, message_id))

    def _retry_outbox(self, row_id: str, record: FederationOutboxRecord, error: str) -> None:
        attempts = min(int(record.get("attempts", 0)) + 1, 16)
        record.update(attempts=attempts, nextAt=time.time() + min(300, 2 ** min(attempts, 8)), error=error[:240])
        with self.runtime.lock, self.runtime.db() as db:
            db.execute("UPDATE runtime_federation_outbox SET record=? WHERE id=? AND json_extract(record,'$.status')='queued'",
                       (_json(record), row_id))

    def _set_peer_error(self, peer_id: str, error: str) -> None:
        with self.runtime.lock, self.runtime.db() as db:
            peer = _record(db.execute("SELECT record FROM runtime_federation_peers WHERE id=?", (peer_id,)).fetchone())
            if peer and peer.get("status") != "revoked":
                peer.update(lastError=error[:240], updated=time.time())
                db.execute("UPDATE runtime_federation_peers SET record=? WHERE id=?", (_json(peer), peer_id))

    def _mark_peer_revoked(self, peer_id: str) -> None:
        with self.runtime.lock, self.runtime.db() as db:
            peer = _record(db.execute("SELECT record FROM runtime_federation_peers WHERE id=?", (peer_id,)).fetchone())
            if peer:
                peer.update(status="revoked", localApproved=False, remoteApproved=False, revoked=time.time())
                peer.pop("inviteId", None)
                peer.pop("inviteToken", None)
                db.execute("UPDATE runtime_federation_peers SET record=? WHERE id=?", (_json(peer), peer_id))
                rows = db.execute("SELECT id FROM runtime_federation_outbox WHERE peer_id=?", (peer_id,)).fetchall()
                for row in rows:
                    msg = db.execute("SELECT deliveries FROM runtime_chat_messages WHERE id=?", (row[0],)).fetchone()
                    if msg:
                        deliveries = json.loads(msg[0]); deliveries["remote:" + peer_id] = "cancelled"
                        db.execute("UPDATE runtime_chat_messages SET deliveries=? WHERE id=?", (_json(deliveries), row[0]))
                db.execute("DELETE FROM runtime_federation_outbox WHERE peer_id=?", (peer_id,))
                db.execute("UPDATE runtime_federation_rooms SET record=json_set(record,'$.status','revoked') "
                           "WHERE json_extract(record,'$.peerId')=?", (peer_id,))
                self._sync_peer_rooms(db, peer_id)
