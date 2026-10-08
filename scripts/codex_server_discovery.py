"""Owner-only server discovery. No Serve configuration or invitation secrets."""
from __future__ import annotations

import base64
from concurrent.futures import ThreadPoolExecutor, as_completed, TimeoutError as FutureTimeout
import json
import sqlite3
import threading
import time
from typing import Any, TYPE_CHECKING
from urllib.parse import urlsplit
import uuid

import codex_multi_server as access
from codex_remote import RemoteAccess

if TYPE_CHECKING:
    from codex_multi_server import Headers, MultiServerService

IDENTITY_PATH = "/api/multi-server/v1/identity"
AUTO_PAIR_PATH = "/api/multi-server/v1/auto-pair"
INTERVAL = 300
PROBE_TIMEOUT = 5
PASS_TIMEOUT = 90
CONCURRENCY = 4
MAX_CANDIDATES = 64


class ServerDiscovery:
    def __init__(self, service: MultiServerService) -> None:
        self.service = service
        self.run_lock = threading.Lock()
        self.stop = threading.Event()
        self.thread: threading.Thread | None = None
        with service.runtime.db() as db:
            db.executescript('''
                CREATE TABLE IF NOT EXISTS runtime_access_settings(id INTEGER PRIMARY KEY CHECK(id=1), auto_pair INTEGER NOT NULL);
                INSERT OR IGNORE INTO runtime_access_settings VALUES(1,1);
                CREATE TABLE IF NOT EXISTS runtime_access_discovered(id TEXT PRIMARY KEY, record TEXT NOT NULL);
                CREATE TABLE IF NOT EXISTS runtime_access_auto_attempts(id TEXT PRIMARY KEY, record TEXT NOT NULL);
                CREATE TABLE IF NOT EXISTS runtime_access_unrevokes(id TEXT PRIMARY KEY, client TEXT NOT NULL);
            ''')

    def start(self) -> None:
        if self.thread is not None:
            return
        self.thread = threading.Thread(target=self._loop, name="studio-server-discovery", daemon=True)
        self.thread.start()

    def close(self) -> None:
        self.stop.set()
        if self.thread is not None:
            self.thread.join(timeout=PASS_TIMEOUT + 10)

    def _loop(self) -> None:
        while not self.stop.is_set():
            try:
                self.discover()
            except (access.AccessError, OSError, RuntimeError, sqlite3.Error):
                pass
            self.stop.wait(INTERVAL)

    def enabled(self) -> bool:
        with self.service.runtime.read_db() as db:
            return bool(db.execute("SELECT auto_pair FROM runtime_access_settings WHERE id=1").fetchone()[0])

    def settings(self, enabled: bool, actor: str) -> dict[str, Any]:
        with self.service._write() as db:
            db.execute("UPDATE runtime_access_settings SET auto_pair=? WHERE id=1", (int(enabled),))
            self.service._audit(db, "local", "settings", actor)
        return self.service.snapshot()

    def identity(self) -> dict[str, Any]:
        return {"protocol": 1, **self.service.identity(owner=True), "autoPair": self.enabled()}

    def owner_proof(self, headers: Headers, origin: str | None = None) -> str:
        # Auto-pair never uses the short peer cache: a DNS/node change matters.
        login, node = access._peer_details(headers)
        owner = access._owner_login()
        if login != owner:
            raise access.AccessError(403, "owner_mismatch", "The Tailscale client must be the server owner")
        if origin is not None:
            dns = node.get("Name") or node.get("DNSName")
            if not isinstance(dns, str) or dns.rstrip(".").lower() != urlsplit(origin).hostname:
                raise access.AccessError(403, "origin_mismatch", "The caller origin does not match its Tailscale node")
        return login

    def probe(self, origin: str, *, timeout: float = PROBE_TIMEOUT) -> dict[str, Any]:
        origin = access._origin(origin)
        reply = access._exchange(origin + IDENTITY_PATH, "GET", {"Accept-Encoding": "identity"}, b"", timeout)
        if reply.get("url") != origin + IDENTITY_PATH or reply.get("status") != 200:
            raise access.AccessError(503, "identity_unavailable", "The peer identity is unavailable")
        try:
            raw = base64.b64decode(reply["body"], validate=True)
            if len(raw) > 16 * 1024:
                raise ValueError
            value = json.loads(raw)
            if (not isinstance(value, dict) or value.get("protocol") != 1 or value.get("origin") != origin
                    or not access._ed25519_key(value.get("publicKey")) or type(value.get("autoPair")) is not bool
                    or not isinstance(value.get("label"), str) or not 1 <= len(value["label"]) <= 80
                    or not isinstance(value.get("tailscaleUser"), str)):
                raise ValueError
            access._id(value.get("serverId"), "server ID")
        except (ValueError, KeyError, TypeError):
            raise access.AccessError(403, "identity_mismatch", "The peer identity is invalid") from None
        return value

    def verify_auto(self, headers: Headers, body: dict[str, Any]) -> str:
        if not self.enabled():
            raise access.AccessError(403, "auto_pair_disabled", "Automatic server pairing is disabled")
        origin = access._origin(body.get("origin"))
        owner = self.owner_proof(headers, origin)
        peer = self.probe(origin)
        if (peer["serverId"] != body["serverId"] or peer["publicKey"] != body["publicKey"]
                or peer["tailscaleUser"].strip().lower() != owner):
            raise access.AccessError(403, "identity_mismatch", "The caller identity does not match its proof")
        if not peer["autoPair"]:
            raise access.AccessError(403, "auto_pair_disabled", "Automatic server pairing is disabled on the caller")
        return owner

    def register(self, peer: dict[str, Any], *, request_id: str | None = None,
                 receipt: dict[str, Any] | None = None) -> None:
        server_id = peer["serverId"]
        with self.service._write() as db:
            if self.stop.is_set() or not db.execute("SELECT auto_pair FROM runtime_access_settings WHERE id=1").fetchone()[0]:
                raise access.AccessError(403, "auto_pair_disabled", "Automatic server pairing is disabled")
            existing = access._record(db.execute("SELECT record FROM runtime_access_clients WHERE id=?", (server_id,)).fetchone())
            if existing and existing.get("status") == "revoked":
                raise access.AccessError(403, "client_revoked", "The server is revoked")
            if existing and (existing.get("publicKey") != peer["publicKey"] or existing.get("kind") != "server"
                             or existing.get("origin") != peer["origin"]):
                raise access.AccessError(403, "identity_mismatch", "The registered server identity differs")
            if existing is None:
                existing = {"id": server_id, "clientId": server_id, "serverId": server_id, "kind": "server",
                            "label": peer["label"], "origin": peer["origin"], "publicKey": peer["publicKey"],
                            "tailscaleUser": peer["tailscaleUser"], "status": "paired", "created": time.time(),
                            "lastAccess": None, "revoked": None}
                self.service._audit(db, server_id, "auto_pair", server_id)
            if request_id is not None:
                pending = db.execute("SELECT hash FROM runtime_access_receipts WHERE client=? AND request_id=?", (server_id, request_id)).fetchone()
                if pending is None:
                    raise access.AccessError(409, "receipt_required", "Reserve an automatic pairing receipt first")
                existing.update(autoPairingRequestId=request_id, autoPairingHash=pending[0], autoPairingReceipt=receipt)
            db.execute("INSERT INTO runtime_access_clients VALUES(?,?) ON CONFLICT(id) DO UPDATE SET record=excluded.record",
                       (server_id, access._json(existing)))

    def accept(self, principal: dict[str, Any], body: dict[str, Any]) -> dict[str, Any]:
        if not principal.get("autoPairing"):
            raise access.AccessError(403, "invalid_pairing", "A signed server identity proof is required")
        result = {"protocol": 1, **self.service.identity(), "tailscaleUser": principal["tailscaleUser"],
                  "clientId": body["serverId"], "paired": True}
        self.register({**body, "tailscaleUser": principal["tailscaleUser"]}, request_id=principal["requestId"], receipt=result)
        return result

    def unrevoke(self, client_id: str, request_id: str, actor: str) -> dict[str, Any]:
        access._id(client_id, "client ID")
        access._id(request_id, "request ID")
        with self.service._write() as db:
            saved = db.execute("SELECT client FROM runtime_access_unrevokes WHERE id=?", (request_id,)).fetchone()
            if saved and saved[0] != client_id:
                raise access.AccessError(409, "request_conflict", "This request ID selected another server")
            if not saved:
                peer = access._record(db.execute("SELECT record FROM runtime_access_clients WHERE id=?", (client_id,)).fetchone())
                if not peer or peer.get("status") != "revoked" or peer.get("kind") != "server":
                    raise access.AccessError(409, "server_not_revoked", "Select a revoked server")
                db.execute("DELETE FROM runtime_access_clients WHERE id=?", (client_id,))
                db.execute("DELETE FROM runtime_access_auto_attempts WHERE id=?", (client_id,))
                db.execute("INSERT INTO runtime_access_unrevokes VALUES(?,?)", (request_id, client_id))
                self.service._audit(db, client_id, "unrevoke", actor)
        return self.service.snapshot()

    def servers(self, clients: list[dict[str, Any]]) -> list[dict[str, Any]]:
        with self.service.runtime.read_db() as db:
            known = {row[0]: json.loads(row[1]) for row in db.execute("SELECT id,record FROM runtime_access_discovered")}
        for client in clients:
            discovered = known.get(client["id"], {})
            known[client["id"]] = {**client, "lastSeen": discovered.get("lastSeen"), "autoPair": discovered.get("autoPair")}
        return sorted(known.values(), key=lambda peer: peer["id"])

    def _candidate(self, origin: str, owner: str, deadline: float) -> None:
        if self.stop.is_set() or time.monotonic() >= deadline:
            return
        identified = False
        try:
            peer = self.probe(origin, timeout=min(PROBE_TIMEOUT, max(0.1, deadline - time.monotonic())))
            identified = True
            if peer["serverId"] == self.service.local_server_id or peer["tailscaleUser"].strip().lower() != owner:
                return
            with self.service._write() as db:
                if self.stop.is_set():
                    return
                previous = access._record(db.execute("SELECT record FROM runtime_access_discovered WHERE id=?", (peer["serverId"],)).fetchone())
                client = access._record(db.execute("SELECT record FROM runtime_access_clients WHERE id=?", (peer["serverId"],)).fetchone())
                if client and (client["publicKey"] != peer["publicKey"] or client["origin"] != origin):
                    return
                record = {"id": peer["serverId"], "clientId": peer["serverId"], "serverId": peer["serverId"], "kind": "server",
                          "label": peer["label"], "origin": origin, "publicKey": peer["publicKey"], "tailscaleUser": owner,
                          "status": "discovered", "created": previous["created"] if previous else time.time(),
                          "lastAccess": None, "revoked": None, "lastSeen": time.time(), "autoPair": peer["autoPair"]}
                db.execute("INSERT INTO runtime_access_discovered VALUES(?,?) ON CONFLICT(id) DO UPDATE SET record=excluded.record",
                           (peer["serverId"], access._json(record)))
                # Bound stale discovery rows. Active credentials remain in their own table.
                db.execute("DELETE FROM runtime_access_discovered WHERE id IN (SELECT id FROM runtime_access_discovered ORDER BY json_extract(record,'$.lastSeen') DESC LIMIT -1 OFFSET 256)")
            if client or not peer["autoPair"] or not self.enabled() or time.monotonic() >= deadline:
                return
            keys = self.service._keys()
            body = {"protocol": 1, "serverId": keys["serverId"], "label": keys["label"],
                    "origin": self.service.public_origin, "publicKey": keys["publicKey"]}
            with self.service._write() as db:
                prior = access._record(db.execute("SELECT record FROM runtime_access_auto_attempts WHERE id=?", (peer["serverId"],)).fetchone())
                if prior and {k: v for k, v in prior.items() if k != "requestId"} == body:
                    body = prior
                else:
                    body["requestId"] = str(uuid.uuid4())
                    db.execute("INSERT INTO runtime_access_auto_attempts VALUES(?,?) ON CONFLICT(id) DO UPDATE SET record=excluded.record",
                               (peer["serverId"], access._json(body)))
            response = self.service._request(origin, peer["serverId"], "POST", AUTO_PAIR_PATH, body,
                                            body["requestId"], min(15, max(0.1, deadline - time.monotonic())), public_key=peer["publicKey"])
            if (response.get("paired") is not True or response.get("clientId") != self.service.local_server_id
                    or response.get("origin") != origin or response.get("tailscaleUser") != owner):
                raise access.AccessError(403, "identity_mismatch", "The automatic pairing response differs")
            self.register(peer)
        except (access.AccessError, OSError, RuntimeError, sqlite3.Error):
            # A refusal cannot restore a credential. Known identities remain visible.
            if not identified:
                self._unreachable(origin)

    def _unreachable(self, origin: str) -> None:
        if self.stop.is_set():
            return
        with self.service._write() as db:
            rows = db.execute("SELECT id,record FROM runtime_access_discovered WHERE json_extract(record,'$.origin')=?", (origin,)).fetchall()
            for row in rows:
                peer = json.loads(row[1])
                peer["status"] = "unreachable"
                db.execute("UPDATE runtime_access_discovered SET record=? WHERE id=?", (access._json(peer), row[0]))

    def discover(self) -> dict[str, Any]:
        if not self.run_lock.acquire(blocking=False):
            raise access.AccessError(409, "discovery_busy", "Server discovery is already active")
        try:
            self.service.public_origin = RemoteAccess(self.service.runtime.root).origin()
            if not self.service.public_origin or self.stop.is_set():
                return self.service.snapshot()
            deadline = time.monotonic() + PASS_TIMEOUT
            status = access._tailscale_json("status", "--json")
            node, peers = status.get("Self"), status.get("Peer")
            if status.get("BackendState") != "Running" or not isinstance(node, dict) or node.get("Tags") or not isinstance(peers, dict) or type(node.get("UserID")) is not int or node["UserID"] <= 0:
                raise access.AccessError(503, "identity_unavailable", "The Tailscale peers are unavailable")
            owner = access._owner_login()
            candidates = set()
            for peer in peers.values():
                if (not isinstance(peer, dict) or peer.get("Online") is not True or peer.get("Tags")
                        or peer.get("UserID") != node.get("UserID") or not isinstance(peer.get("DNSName"), str)):
                    continue
                try:
                    origin = access._origin("https://" + peer["DNSName"].rstrip(".").lower())
                except access.AccessError:
                    continue
                if origin != self.service.public_origin:
                    candidates.add(origin)
            # Formerly known peers that disappeared or went offline retain their identity.
            with self.service.runtime.read_db() as db:
                origins = [json.loads(row[0])["origin"] for row in db.execute("SELECT record FROM runtime_access_discovered")]
            for origin in origins:
                if origin not in candidates:
                    self._unreachable(origin)
            with ThreadPoolExecutor(max_workers=CONCURRENCY, thread_name_prefix="studio-discovery-probe") as pool:
                futures = [pool.submit(self._candidate, origin, owner, deadline) for origin in sorted(candidates)[:MAX_CANDIDATES]]
                try:
                    for future in as_completed(futures, timeout=max(0.1, deadline - time.monotonic())):
                        future.result()
                except FutureTimeout:
                    for future in futures:
                        future.cancel()
            return self.service.snapshot()
        finally:
            self.run_lock.release()
