"""Owner device pairing, signed HTTP access, and durable mutation receipts."""
from __future__ import annotations

import base64
from collections import deque
from collections.abc import Iterator
from contextlib import contextmanager
import hashlib
import hmac
import ipaddress
import json
import math
import os
from pathlib import Path
import re
import secrets
import shutil
import sqlite3
import stat
import subprocess
import tempfile
import threading
import time
from typing import Any, ContextManager, Protocol, cast
import uuid

from codex_multi_server_crypto import CryptoProcess
from codex_remote import RemoteAccess, validate_origin

PROTOCOL = 1
CLOCK_SKEW_SECONDS = 300
INVITE_SECONDS = 900
MAX_RESPONSE_BYTES = 1_048_576
MAX_NONCES = 10_000
IDENTITY_CACHE_SECONDS = 30
MAX_IDENTITY_CACHE = 256
MAX_PAIRING_ATTEMPTS = 128
MAX_UNPAIRED_NONCES = 1280
PAIRING_WINDOW_SECONDS = 60
RECEIPT_SECONDS = 7 * 86400
OUTBOUND_CACHE_SECONDS = 86400
PAIR_PATH = "/api/multi-server/v1/pair"
SIGNATURE_HEADERS = (
    "X-Studio-Client", "X-Studio-Server", "X-Studio-Timestamp",
    "X-Studio-Nonce", "X-Studio-Request-Id", "X-Studio-Signature",
)
IDENTIFIER = re.compile(r"[A-Za-z0-9][A-Za-z0-9_.:-]{0,127}\Z")


class Store(Protocol):
    root: Path

    def db(self) -> ContextManager[sqlite3.Connection]: ...
    def read_db(self) -> ContextManager[sqlite3.Connection]: ...


class Headers(Protocol):
    def get(self, name: str, default: str | None = None) -> str | None: ...
    def get_all(self, name: str, default: list[str] | None = None) -> list[str]: ...


class AccessError(Exception):
    """An API error with a stable code and HTTP status."""

    def __init__(self, status: int, code: str, message: str) -> None:
        super().__init__(message)
        self.status = status
        self.code = code


def _json(value: object) -> str:
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"), allow_nan=False)


def _record(row: sqlite3.Row | tuple[Any, ...] | None) -> dict[str, Any] | None:
    return cast(dict[str, Any], json.loads(row[0])) if row else None


def _id(value: object, name: str) -> str:
    if not isinstance(value, str) or not IDENTIFIER.fullmatch(value):
        raise AccessError(400, "invalid_id", f"Invalid {name}")
    return value


def _origin(value: object) -> str:
    try:
        return cast(str, validate_origin(value))
    except ValueError:
        raise AccessError(400, "invalid_origin", "Use an HTTPS Tailscale origin without a path") from None


def _check_response_identity(value: dict[str, Any], server_id: str, public_key: str | None) -> None:
    for identity in (value, value.get("identity")):
        if isinstance(identity, dict) and (
            ("serverId" in identity and identity["serverId"] != server_id)
            or ("publicKey" in identity and identity["publicKey"] != public_key)
        ):
            raise AccessError(403, "server_mismatch", "The server response identity does not match the paired server")


def _ed25519_key(value: object) -> bool:
    if not isinstance(value, str) or len(value) > 4096:
        return False
    if not value.startswith("-----BEGIN PUBLIC KEY-----") or not value.rstrip().endswith("-----END PUBLIC KEY-----"):
        return False
    try:
        content = value.removeprefix("-----BEGIN PUBLIC KEY-----").strip().removesuffix("-----END PUBLIC KEY-----").strip()
        raw = base64.b64decode("".join(content.split()), validate=True)
        # RFC 8410 SubjectPublicKeyInfo: id-Ed25519, absent parameters, 32-byte key.
        return len(raw) == 44 and raw[:12] == bytes.fromhex("302a300506032b6570032100")
    except ValueError:
        return False


def request_bytes(method: str, target: str, server_id: str, client_id: str,
                  timestamp: str, nonce: str, request_id: str, raw: bytes) -> bytes:
    return "\n".join(("studio-multi-server-v1", method.upper(), target, server_id,
                      client_id, timestamp, nonce, request_id,
                      hashlib.sha256(raw).hexdigest())).encode("utf-8")


def _tailscale_json(*arguments: str) -> dict[str, Any]:
    executable = shutil.which("tailscale")
    bundled = Path("/Applications/Tailscale.app/Contents/MacOS/Tailscale")
    if not executable and bundled.is_file():
        executable = str(bundled)
    if not executable:
        raise AccessError(503, "identity_unavailable", "The Tailscale identity is unavailable")
    try:
        result = subprocess.run([executable, *arguments], capture_output=True, timeout=5, check=True)
        if len(result.stdout) > MAX_RESPONSE_BYTES:
            raise ValueError
        value = json.loads(result.stdout)
        if not isinstance(value, dict):
            raise ValueError
        return value
    except (OSError, subprocess.SubprocessError, ValueError):
        raise AccessError(503, "identity_unavailable", "The Tailscale identity is unavailable") from None


def _owner_login() -> str:
    # The CLI JSON contract exposes Self.UserID and the corresponding User entry.
    status = _tailscale_json("status", "--json")
    node, users = status.get("Self"), status.get("User")
    if not isinstance(node, dict) or not isinstance(users, dict):
        raise AccessError(503, "identity_unavailable", "The Tailscale owner identity is unavailable")
    profile = users.get(str(node.get("UserID")))
    if not isinstance(profile, dict):
        raise AccessError(503, "identity_unavailable", "The Tailscale owner identity is unavailable")
    login = profile.get("LoginName")
    if status.get("BackendState") != "Running" or node.get("Tags") or not isinstance(login, str) or not login.strip() or len(login) > 254:
        raise AccessError(503, "identity_unavailable", "The Tailscale owner identity is unavailable")
    return login.strip().lower()


def _peer_login(headers: Headers) -> str:
    try:
        address = ipaddress.ip_address(headers.get("X-Forwarded-For") or "")
    except ValueError:
        raise AccessError(403, "identity_unavailable", "The Tailscale client identity is unavailable") from None
    if not (address in ipaddress.ip_network("100.64.0.0/10") or address in ipaddress.ip_network("fd7a:115c:a1e0::/48")):
        raise AccessError(403, "identity_unavailable", "The Tailscale client identity is unavailable")
    # Do not substitute a caller's identity header for the whois result.
    whois = _tailscale_json("whois", "--json", str(address))
    node, profile = whois.get("Node"), whois.get("UserProfile")
    if not isinstance(node, dict) or not isinstance(profile, dict):
        raise AccessError(403, "identity_unavailable", "The Tailscale client identity is unavailable")
    login = profile.get("LoginName")
    if node.get("Tags") or not isinstance(login, str) or not login.strip() or len(login) > 254:
        raise AccessError(403, "identity_unavailable", "The Tailscale client identity is unavailable")
    supplied = headers.get_all("Tailscale-User-Login", [])
    if len(supplied) > 1 or (supplied and supplied[0].strip().lower() != login.strip().lower()):
        raise AccessError(403, "owner_mismatch", "The Tailscale identity does not match")
    return login.strip().lower()


def _exchange(url: str, method: str, headers: dict[str, str], raw: bytes, timeout: float) -> dict[str, Any]:
    node = os.environ.get("CODEX_NODE") or shutil.which("node")
    if not node:
        raise AccessError(503, "transport_unavailable", "The Studio Node runtime is unavailable")
    body = _json({"url": url, "method": method, "headers": headers,
                  "body": base64.b64encode(raw).decode(), "timeout": timeout}).encode()
    helper = Path(__file__).with_name("codex_multi_server_http.mjs")
    try:
        process = subprocess.run([node, str(helper)], input=body,
                                 env={**os.environ, "ELECTRON_RUN_AS_NODE": "1"},
                                 capture_output=True, timeout=timeout, check=True)
        if len(process.stdout) > MAX_RESPONSE_BYTES * 2:
            raise ValueError
        result = json.loads(process.stdout)
        if isinstance(result, dict) and result.get("failure") == "response_size":
            raise AccessError(502, "response_size", "The server response exceeds 1 MiB. Keep the request ID")
        if not isinstance(result, dict) or not isinstance(result.get("status"), int):
            raise ValueError
        return result
    except subprocess.TimeoutExpired:
        raise AccessError(504, "remote_timeout", "The server request timed out. Keep the request ID") from None
    except (OSError, subprocess.SubprocessError, ValueError):
        # stderr can contain a private URL or request data. Never return it.
        raise AccessError(503, "remote_unavailable", "The server is unavailable or its response is invalid. Keep the request ID") from None


class MultiServerService:
    """One credential store per Studio state directory, independent of federation."""

    def __init__(self, runtime: Store) -> None:
        self.runtime = runtime
        self.lock = threading.RLock()
        self.public_origin: str | None = RemoteAccess(runtime.root).origin()
        self.session_id = uuid.uuid4().hex
        self.active: set[tuple[str, str]] = set()
        self._identity: dict[str, Any] | None = None
        self.crypto = CryptoProcess()
        self.identity_cache: dict[str, tuple[float, str]] = {}
        self.pairing_attempts: deque[float] = deque()
        with self.runtime.db() as db:
            self.ensure_tables(db)
            self.prune(db)

    def close(self) -> None:
        self.crypto.shutdown()

    def _login(self, headers: Headers | None = None) -> str:
        address = (headers.get("X-Forwarded-For") or "") if headers is not None else "owner"
        with self.lock:
            now = time.monotonic()
            saved = self.identity_cache.get(address)
            try:
                if saved and now - saved[0] < IDENTITY_CACHE_SECONDS:
                    login = saved[1]
                else:
                    login = _peer_login(headers) if headers is not None else _owner_login()
                    if len(self.identity_cache) >= MAX_IDENTITY_CACHE:
                        self.identity_cache.clear()
                    self.identity_cache[address] = (now, login)
                if headers is not None:
                    supplied = headers.get_all("Tailscale-User-Login", [])
                    if len(supplied) > 1 or (supplied and supplied[0].strip().lower() != login):
                        raise AccessError(403, "owner_mismatch", "The Tailscale identity does not match")
                return login
            except AccessError:
                self.identity_cache.clear()
                raise

    def _pairing_limit(self) -> None:
        with self.lock:
            now = time.monotonic()
            while self.pairing_attempts and self.pairing_attempts[0] <= now - PAIRING_WINDOW_SECONDS:
                self.pairing_attempts.popleft()
            if len(self.pairing_attempts) >= MAX_PAIRING_ATTEMPTS:
                raise AccessError(429, "pairing_limit", "The server pairing attempt limit is reached")
            self.pairing_attempts.append(now)

    @staticmethod
    def prune(db: sqlite3.Connection) -> None:
        now = time.time()
        db.execute("DELETE FROM runtime_access_nonces WHERE created<?", (int(now) - CLOCK_SKEW_SECONDS * 2,))
        db.execute("""UPDATE runtime_access_receipts SET status='tombstone',record=json_object(
                     'outcome','complete','completed',json_extract(record,'$.completed'),
                     'httpStatus',json_extract(record,'$.status'))
                     WHERE status='complete' AND json_extract(record,'$.completed')<?""", (now - RECEIPT_SECONDS,))
        db.execute("""UPDATE runtime_access_outbound SET record=json_object(
                     'outcome','expired','completed',json_extract(record,'$.completed'))
                     WHERE json_extract(record,'$.outcome')='complete'
                     AND json_extract(record,'$.completed')<?""", (now - OUTBOUND_CACHE_SECONDS,))

    @staticmethod
    def ensure_tables(db: sqlite3.Connection) -> None:
        db.executescript("""
        CREATE TABLE IF NOT EXISTS runtime_access_clients (id TEXT PRIMARY KEY, record TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS runtime_access_invites (
          id TEXT PRIMARY KEY, request_id TEXT NOT NULL UNIQUE, hash TEXT NOT NULL, record TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS runtime_access_nonces (
          client TEXT NOT NULL, nonce TEXT NOT NULL, created INTEGER NOT NULL, PRIMARY KEY(client,nonce));
        CREATE INDEX IF NOT EXISTS runtime_access_nonce_age ON runtime_access_nonces(created);
        CREATE TABLE IF NOT EXISTS runtime_access_receipts (
          client TEXT NOT NULL, request_id TEXT NOT NULL, hash TEXT NOT NULL, session TEXT NOT NULL,
          status TEXT NOT NULL, record TEXT NOT NULL, PRIMARY KEY(client,request_id));
        CREATE TABLE IF NOT EXISTS runtime_access_outbound (
          server TEXT NOT NULL, request_id TEXT NOT NULL, hash TEXT NOT NULL,
          record TEXT NOT NULL, PRIMARY KEY(server,request_id));
        CREATE INDEX IF NOT EXISTS runtime_access_receipt_age
          ON runtime_access_receipts(json_extract(record,'$.completed')) WHERE status='complete';
        CREATE INDEX IF NOT EXISTS runtime_access_outbound_age
          ON runtime_access_outbound(json_extract(record,'$.completed'))
          WHERE json_extract(record,'$.outcome')='complete';
        CREATE TABLE IF NOT EXISTS runtime_access_audit (
          seq INTEGER PRIMARY KEY AUTOINCREMENT, client TEXT NOT NULL, actor TEXT NOT NULL,
          action TEXT NOT NULL, created REAL NOT NULL);
        """)
        if "actor" not in {row[1] for row in db.execute("PRAGMA table_info(runtime_access_audit)")}:
            db.execute("ALTER TABLE runtime_access_audit ADD COLUMN actor TEXT NOT NULL DEFAULT 'local'")

    @contextmanager
    def _write(self) -> Iterator[sqlite3.Connection]:
        with self.lock, self.runtime.db() as db:
            db.execute("BEGIN IMMEDIATE")
            self.prune(db)
            yield db

    def _keys(self) -> dict[str, Any]:
        with self.lock:
            if self._identity is not None:
                return self._identity
            directory = self.runtime.root / "multi-server"
            directory.mkdir(mode=0o700, exist_ok=True)
            if directory.is_symlink() or directory.stat().st_mode & 0o077:
                raise AccessError(503, "credential_permissions", "The credential directory requires owner-only access")
            path = directory / "identity.json"
            if path.is_symlink():
                raise AccessError(503, "credential_permissions", "The credential file must not be a symbolic link")
            if not path.exists():
                pair = self.crypto.call("generate")
                value = {"serverId": uuid.uuid4().hex, "publicKey": pair["publicKey"],
                         "privateKey": pair["privateKey"], "label": "Studio server"}
                fd, temporary = tempfile.mkstemp(prefix="identity-", dir=directory)
                try:
                    with os.fdopen(fd, "w") as stream:
                        stream.write(_json(value))
                        stream.flush()
                        os.fsync(stream.fileno())
                    os.replace(temporary, path)
                    directory_fd = os.open(directory, os.O_RDONLY)
                    try:
                        os.fsync(directory_fd)
                    finally:
                        os.close(directory_fd)
                finally:
                    if os.path.exists(temporary):
                        os.unlink(temporary)
            fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
            with os.fdopen(fd) as stream:
                metadata = os.fstat(stream.fileno())
                if not stat.S_ISREG(metadata.st_mode) or metadata.st_mode & 0o077 or metadata.st_size > 16_384:
                    raise AccessError(503, "credential_permissions", "The credential file requires owner-only access")
                value = json.load(stream)
                private_key = value.get("privateKey") if isinstance(value, dict) else None
                label = value.get("label") if isinstance(value, dict) else None
                if (not isinstance(value, dict) or not _ed25519_key(value.get("publicKey"))
                        or not isinstance(private_key, str)
                        or not private_key.startswith("-----BEGIN PRIVATE KEY-----")
                        or not isinstance(label, str) or not 1 <= len(label) <= 80):
                    raise AccessError(503, "credential_invalid", "The server credential file is invalid")
                _id(value.get("serverId"), "server credential ID")
                self._identity = value
            return self._identity

    @property
    def local_server_id(self) -> str:
        return str(self._keys()["serverId"])

    def identity(self, *, owner: bool = False) -> dict[str, Any]:
        keys = self._keys()
        result = {name: keys[name] for name in ("serverId", "publicKey", "label")}
        result["origin"] = self.public_origin
        result["tailscaleUser"] = self._login() if owner else None
        return result

    def servers(self) -> list[dict[str, Any]]:
        with self.runtime.read_db() as db:
            rows = db.execute("SELECT record FROM runtime_access_clients WHERE json_extract(record,'$.kind')='server'").fetchall()
        return [self._public(cast(dict[str, Any], _record(row))) for row in rows]

    def paired_servers(self) -> list[dict[str, Any]]:
        return self.servers()

    def paired_server(self, server_id: str) -> dict[str, Any]:
        with self.runtime.read_db() as db:
            peer = _record(db.execute("SELECT record FROM runtime_access_clients WHERE id=?", (server_id,)).fetchone())
        if not peer or peer.get("status") != "paired" or peer.get("kind") != "server":
            raise AccessError(403, "server_not_paired", "The server is not paired or is revoked")
        return self._public(peer)

    @staticmethod
    def _public(client: dict[str, Any]) -> dict[str, Any]:
        return {key: client.get(key) for key in ("id", "clientId", "serverId", "label", "kind", "origin", "publicKey",
                                                "tailscaleUser", "status", "created", "lastAccess", "revoked")}

    def snapshot(self) -> dict[str, Any]:
        with self.runtime.read_db() as db:
            clients = [self._public(cast(dict[str, Any], _record(row))) for row in db.execute("SELECT record FROM runtime_access_clients ORDER BY id")]
            invites = []
            for row in db.execute("SELECT record FROM runtime_access_invites ORDER BY rowid DESC LIMIT 100"):
                record = cast(dict[str, Any], _record(row))
                invites.append({key: record.get(key) for key in ("inviteId", "expires", "created", "status")})
        return {"protocol": PROTOCOL, "identity": self.identity(), "clients": clients,
                "servers": [client for client in clients if client["kind"] == "server"], "invites": invites}

    def create_invite(self, body: dict[str, Any], actor: str = "local") -> dict[str, Any]:
        request_id = _id(body.get("requestId"), "request ID")
        if not self.public_origin:
            raise AccessError(400, "serve_required", "Configure Tailscale Serve HTTPS before pairing")
        owner = self._login()
        keys = self._keys()
        fingerprint = hashlib.sha256(_json(body).encode()).hexdigest()
        with self._write() as db:
            row = db.execute("SELECT hash,record FROM runtime_access_invites WHERE request_id=?", (request_id,)).fetchone()
            if row:
                if row[0] != fingerprint:
                    raise AccessError(409, "request_conflict", "The request ID has different content")
                invite = cast(dict[str, Any], json.loads(row[1]))
            else:
                now = int(time.time())
                invite = {"protocol": PROTOCOL, "inviteId": uuid.uuid4().hex,
                          "serverId": keys["serverId"], "label": body.get("label") or keys["label"],
                          "origin": validate_origin(self.public_origin), "publicKey": keys["publicKey"],
                          "tailscaleUser": owner, "expires": now + INVITE_SECONDS,
                          "created": now, "status": "open"}
                token = self._invite_token(invite["inviteId"], request_id)
                invite["tokenHash"] = hashlib.sha256(token.encode()).hexdigest()
                db.execute("INSERT INTO runtime_access_invites VALUES(?,?,?,?)",
                           (invite["inviteId"], request_id, fingerprint, _json(invite)))
                self._audit(db, "local", "create_invite", actor)
        public = {key: invite[key] for key in ("protocol", "inviteId", "serverId", "label", "origin", "publicKey", "tailscaleUser", "expires")}
        public["token"] = self._invite_token(invite["inviteId"], request_id)
        return {"invitation": public, "expires": invite["expires"]}

    def _invite_token(self, invite_id: str, request_id: str) -> str:
        digest = hmac.digest(self._keys()["privateKey"].encode(), f"invite-v1\n{invite_id}\n{request_id}".encode(), "sha256")
        return base64.urlsafe_b64encode(digest).decode().rstrip("=")

    @staticmethod
    def _audit(db: sqlite3.Connection, client: str, action: str, actor: str = "local") -> None:
        db.execute("INSERT INTO runtime_access_audit(client,actor,action,created) VALUES(?,?,?,?)", (client, actor, action, time.time()))
        db.execute("DELETE FROM runtime_access_audit WHERE seq <= (SELECT max(seq)-10000 FROM runtime_access_audit)")

    def audit(self) -> dict[str, Any]:
        with self.runtime.read_db() as db:
            rows = db.execute("SELECT seq,client,actor,action,created FROM runtime_access_audit ORDER BY seq DESC LIMIT 100").fetchall()
        return {"records": [{"sequence": row[0], "clientId": row[1], "actorId": row[2], "action": row[3], "created": row[4]} for row in rows]}

    def authenticate(self, headers: Headers, method: str, target: str, raw: bytes) -> dict[str, Any]:
        values: dict[str, str] = {}
        for name in SIGNATURE_HEADERS:
            supplied = headers.get_all(name, [])
            if len(supplied) != 1 or not supplied[0] or len(supplied[0]) > 512:
                raise AccessError(401, "invalid_credentials", "A single signed credential header is required")
            values[name] = supplied[0]
        client_id = _id(values["X-Studio-Client"], "client ID")
        request_id = _id(values["X-Studio-Request-Id"], "request ID")
        server_id = values["X-Studio-Server"]
        if server_id != self.local_server_id:
            raise AccessError(401, "server_mismatch", "The target server identity does not match")
        timestamp, nonce = values["X-Studio-Timestamp"], values["X-Studio-Nonce"]
        if not re.fullmatch(r"[0-9]{1,12}", timestamp) or abs(int(time.time()) - int(timestamp)) > CLOCK_SKEW_SECONDS:
            raise AccessError(401, "clock_difference", "The request timestamp is outside the allowed clock difference")
        if not re.fullmatch(r"[A-Za-z0-9_-]{32}", nonce):
            raise AccessError(401, "invalid_nonce", "The request nonce is invalid")
        pairing = method == "POST" and target == PAIR_PATH
        body: dict[str, Any] = {}
        if pairing:
            self._pairing_limit()
            try:
                body = json.loads(raw)
                if not isinstance(body, dict):
                    raise ValueError
            except ValueError:
                raise AccessError(400, "invalid_pairing", "The pairing request is invalid") from None
            key = body.get("publicKey")
            if body.get("clientId") != client_id or body.get("requestId") != request_id or body.get("protocol") != PROTOCOL:
                raise AccessError(400, "invalid_pairing", "The pairing identity or protocol does not match")
            if body.get("kind") == "server":
                _origin(body.get("origin"))
            principal = {"clientId": client_id, "kind": body.get("kind"), "publicKey": key}
            with self.runtime.read_db() as db:
                existing = _record(db.execute("SELECT record FROM runtime_access_clients WHERE id=?", (client_id,)).fetchone())
            if existing and existing.get("status") == "revoked":
                raise AccessError(403, "client_revoked", "The client is revoked")
            fingerprint = hashlib.sha256(method.encode() + b"\n" + target.encode() + b"\n" + raw).hexdigest()
            recovery = existing and existing.get("pairingRequestId") == request_id and existing.get("pairingHash") == fingerprint
            if not recovery:
                with self.runtime.read_db() as db:
                    invite = _record(db.execute("SELECT record FROM runtime_access_invites WHERE id=?", (body.get("inviteId"),)).fetchone())
                token = body.get("token")
                if not invite or invite.get("status") != "open" or invite["expires"] < time.time():
                    raise AccessError(403, "invite_unavailable", "The invitation is expired or was already used")
                if not isinstance(token, str) or len(token) > 128 or not hmac.compare_digest(invite["tokenHash"], hashlib.sha256(token.encode()).hexdigest()):
                    raise AccessError(403, "invalid_invite", "The invitation secret is invalid")
        else:
            with self.runtime.read_db() as db:
                principal = _record(db.execute("SELECT record FROM runtime_access_clients WHERE id=?", (client_id,)).fetchone()) or {}
            if principal.get("status") != "paired":
                raise AccessError(403, "client_revoked", "The client is not paired or is revoked")
            key = principal.get("publicKey")
        signature = values["X-Studio-Signature"]
        try:
            valid_encoding = len(base64.b64decode(signature, validate=True)) == 64
        except ValueError:
            valid_encoding = False
        if not _ed25519_key(key) or not valid_encoding:
            raise AccessError(401, "invalid_signature", "The request signature is invalid")
        try:
            valid = self.crypto.call("verify", publicKey=key, signature=signature,
                            data=base64.b64encode(request_bytes(method, target, server_id, client_id,
                                                              timestamp, nonce, request_id, raw)).decode()).get("valid")
        except RuntimeError:
            raise AccessError(503, "crypto_unavailable", "The Ed25519 verification service is unavailable") from None
        if valid is not True:
            raise AccessError(401, "invalid_signature", "The request signature is invalid")
        owner, observed = self._login(), self._login(headers)
        if owner != observed or (principal.get("tailscaleUser") not in (None, observed)):
            self.identity_cache.clear()
            raise AccessError(403, "owner_mismatch", "The Tailscale client must be the server owner")
        now = int(time.time())
        with self._write() as db:
            db.execute("DELETE FROM runtime_access_nonces WHERE created<?", (now - CLOCK_SKEW_SECONDS * 2,))
            if pairing and not recovery:
                unpaired = db.execute("""SELECT count(*) FROM runtime_access_nonces n
                    LEFT JOIN runtime_access_clients c ON c.id=n.client WHERE c.id IS NULL""").fetchone()[0]
                if unpaired >= MAX_UNPAIRED_NONCES:
                    raise AccessError(429, "pairing_limit", "The server pairing attempt limit is reached")
            count = db.execute("SELECT count(*) FROM runtime_access_nonces WHERE client=?", (client_id,)).fetchone()[0]
            if count >= MAX_NONCES:
                raise AccessError(429, "nonce_limit", "The client request limit is reached")
            try:
                db.execute("INSERT INTO runtime_access_nonces VALUES(?,?,?)", (client_id, nonce, now))
            except sqlite3.IntegrityError:
                raise AccessError(401, "nonce_replay", "The request nonce was already used") from None
            if not pairing:
                current = _record(db.execute("SELECT record FROM runtime_access_clients WHERE id=?", (client_id,)).fetchone())
                if not current or current.get("status") != "paired":
                    raise AccessError(403, "client_revoked", "The client is revoked")
                current["lastAccess"] = time.time()
                db.execute("UPDATE runtime_access_clients SET record=? WHERE id=?", (_json(current), client_id))
        return {**principal, "clientId": client_id, "serverId": client_id if principal.get("kind") == "server" else None,
                "targetServerId": server_id, "requestId": request_id, "tailscaleUser": observed,
                "pairing": pairing}

    def pair(self, principal: dict[str, Any], body: dict[str, Any]) -> dict[str, Any]:
        if not principal.get("pairing") or body.get("kind") not in {"ui", "server"}:
            raise AccessError(403, "invalid_pairing", "A signed pairing request is required")
        label = body.get("label")
        if not isinstance(label, str) or not 1 <= len(label) <= 80:
            raise AccessError(400, "invalid_label", "Use a client label with 1 to 80 characters")
        invite_id = _id(body.get("inviteId"), "invitation ID")
        client_id = principal["clientId"]
        origin = _origin(body.get("origin")) if body["kind"] == "server" else None
        result = {"protocol": PROTOCOL, **self.identity(), "tailscaleUser": principal["tailscaleUser"],
                  "clientId": client_id, "paired": True}
        with self._write() as db:
            invite = _record(db.execute("SELECT record FROM runtime_access_invites WHERE id=?", (invite_id,)).fetchone())
            if not invite or invite["status"] != "open" or invite["expires"] < time.time():
                raise AccessError(403, "invite_unavailable", "The invitation is expired or was already used")
            token = body.get("token")
            if not isinstance(token, str) or len(token) > 128 or not hmac.compare_digest(invite["tokenHash"], hashlib.sha256(token.encode()).hexdigest()):
                raise AccessError(403, "invalid_invite", "The invitation secret is invalid")
            if invite["tailscaleUser"] != principal["tailscaleUser"]:
                raise AccessError(403, "owner_mismatch", "The invitation owner does not match")
            if db.execute("SELECT 1 FROM runtime_access_clients WHERE id=?", (client_id,)).fetchone():
                raise AccessError(409, "client_exists", "Use a new client identity for a new pairing")
            now = time.time()
            client = {"id": client_id, "clientId": client_id, "serverId": client_id if body["kind"] == "server" else None,
                      "label": label, "kind": body["kind"], "origin": origin, "publicKey": body["publicKey"],
                      "tailscaleUser": principal["tailscaleUser"], "status": "paired", "created": now,
                      "lastAccess": now, "revoked": None}
            pending = db.execute("SELECT hash FROM runtime_access_receipts WHERE client=? AND request_id=?",
                                 (client_id, principal["requestId"])).fetchone()
            if pending is None:
                raise AccessError(409, "receipt_required", "Reserve a pairing receipt before the operation")
            client["pairingRequestId"] = principal["requestId"]
            client["pairingHash"] = pending[0]
            client["pairingReceipt"] = result
            invite["status"] = "used"
            db.execute("UPDATE runtime_access_invites SET record=? WHERE id=?", (_json(invite), invite_id))
            db.execute("INSERT INTO runtime_access_clients VALUES(?,?)", (client_id, _json(client)))
            self._audit(db, client_id, "pair", client_id)
        return result

    def revoke(self, client_id: str, actor: str = "local") -> dict[str, Any]:
        with self._write() as db:
            client = _record(db.execute("SELECT record FROM runtime_access_clients WHERE id=?", (client_id,)).fetchone())
            if not client:
                raise AccessError(404, "client_missing", "The client does not exist")
            if client["status"] != "revoked":
                client.update(status="revoked", revoked=time.time())
                db.execute("UPDATE runtime_access_clients SET record=? WHERE id=?", (_json(client), client_id))
                self._audit(db, client_id, "revoke", actor)
        return self.snapshot()

    def allowed(self, client_id: str) -> bool:
        with self.runtime.read_db() as db:
            row = db.execute("SELECT json_extract(record,'$.status') FROM runtime_access_clients WHERE id=?", (client_id,)).fetchone()
        return bool(row and row[0] == "paired")

    def reserve(self, client_id: str, request_id: str, method: str, target: str, raw: bytes) -> dict[str, Any] | None:
        fingerprint = hashlib.sha256(method.encode() + b"\n" + target.encode() + b"\n" + raw).hexdigest()
        with self._write() as db:
            row = db.execute("SELECT hash,session,status,record FROM runtime_access_receipts WHERE client=? AND request_id=?",
                             (client_id, request_id)).fetchone()
            if row:
                if row[0] != fingerprint:
                    raise AccessError(409, "request_conflict", "The request ID has different content")
                if row[2] == "tombstone":
                    raise AccessError(410, "receipt_expired", "The saved response expired. The operation must not run again")
                if row[2] == "retry":
                    db.execute("UPDATE runtime_access_receipts SET session=?,status='pending' WHERE client=? AND request_id=?",
                               (self.session_id, client_id, request_id))
                    self.active.add((client_id, request_id))
                    return None
                if row[2] == "complete":
                    receipt = cast(dict[str, Any], json.loads(row[3]))
                    if "inviteTokenOffset" in receipt:
                        template = base64.b64decode(receipt["body"])
                        offset = receipt["inviteTokenOffset"]
                        token = self._invite_token(receipt["inviteId"], request_id).encode()
                        receipt["body"] = base64.b64encode(template[:offset] + token + template[offset:]).decode()
                    return receipt
                if method == "POST" and target == PAIR_PATH:
                    client = _record(db.execute("SELECT record FROM runtime_access_clients WHERE id=?", (client_id,)).fetchone())
                    if client and client.get("pairingRequestId") == request_id and client.get("pairingHash") == fingerprint:
                        response = _json(client["pairingReceipt"]).encode()
                        receipt = {"status": 200, "headers": [["content-type", "application/json"],
                                   ["content-length", str(len(response))], ["cache-control", "no-store"]],
                                   "body": base64.b64encode(response).decode(), "completed": time.time()}
                        db.execute("UPDATE runtime_access_receipts SET status='complete',record=? WHERE client=? AND request_id=?",
                                   (_json(receipt), client_id, request_id))
                        return receipt
                if row[1] == self.session_id and (client_id, request_id) in self.active:
                    raise AccessError(409, "request_pending", "The operation is in progress. Keep the request ID")
                raise AccessError(409, "outcome_unknown", "The operation outcome is unknown. Recover its existing receipt")
            db.execute("INSERT INTO runtime_access_receipts VALUES(?,?,?,?,'pending',?)",
                       (client_id, request_id, fingerprint, self.session_id, _json({"created": time.time()})))
            self.active.add((client_id, request_id))
        return None

    def complete(self, client_id: str, request_id: str, status: int,
                 headers: list[tuple[bytes, bytes]], raw: bytes, *, redact_invite: bool = False) -> None:
        record = {"status": status, "headers": [[name.decode("latin-1"), value.decode("latin-1")] for name, value in headers],
                  "body": base64.b64encode(raw).decode(), "completed": time.time()}
        if redact_invite and 200 <= status < 300:
            invitation = json.loads(raw)["invitation"]
            token = invitation["token"].encode()
            if (token != self._invite_token(invitation["inviteId"], request_id).encode()
                    or raw.count(token) != 1):
                raise AccessError(502, "outcome_unknown", "The invitation receipt is invalid. Keep the request ID")
            offset = raw.index(token)
            record["body"] = base64.b64encode(raw[:offset] + raw[offset + len(token):]).decode()
            record["inviteId"] = invitation["inviteId"]
            record["inviteTokenOffset"] = offset
        with self._write() as db:
            db.execute("UPDATE runtime_access_receipts SET status='complete',record=? WHERE client=? AND request_id=? AND session=?",
                       (_json(record), client_id, request_id, self.session_id))
            self.active.discard((client_id, request_id))

    def abandon(self, client_id: str, request_id: str) -> None:
        with self.lock:
            self.active.discard((client_id, request_id))

    def retry(self, client_id: str, request_id: str, *, discard: bool = False) -> None:
        with self._write() as db:
            if discard:
                db.execute("DELETE FROM runtime_access_receipts WHERE client=? AND request_id=? AND session=? AND status='pending'",
                           (client_id, request_id, self.session_id))
            else:
                db.execute("UPDATE runtime_access_receipts SET status='retry' WHERE client=? AND request_id=? AND session=? AND status='pending'",
                           (client_id, request_id, self.session_id))
            self.active.discard((client_id, request_id))

    def signed_headers(self, server_id: str, method: str, target: str, raw: bytes, request_id: str) -> dict[str, str]:
        timestamp, nonce = str(int(time.time())), secrets.token_urlsafe(24)
        keys = self._keys()
        headers = dict(zip(SIGNATURE_HEADERS[:5], (self.local_server_id, server_id, timestamp, nonce, request_id), strict=True))
        headers["X-Studio-Signature"] = str(self.crypto.call("sign", privateKey=keys["privateKey"],
            data=base64.b64encode(request_bytes(method, target, server_id, self.local_server_id,
                                              timestamp, nonce, request_id, raw)).decode())["signature"])
        return headers

    def accept_invite(self, invitation: dict[str, Any], request_id: str, actor: str = "local") -> dict[str, Any]:
        server_id = _id(invitation.get("serverId"), "server ID")
        origin = _origin(invitation.get("origin"))
        if invitation.get("protocol") != PROTOCOL or not _ed25519_key(invitation.get("publicKey")):
            raise AccessError(400, "invalid_invite", "The invitation protocol or key is invalid")
        if not self.public_origin:
            raise AccessError(400, "serve_required", "Configure Tailscale Serve HTTPS before pairing")
        keys = self._keys()
        body = {"protocol": PROTOCOL, "inviteId": invitation.get("inviteId"), "token": invitation.get("token"),
                "clientId": self.local_server_id, "label": keys["label"], "kind": "server",
                "publicKey": keys["publicKey"], "origin": self.public_origin, "requestId": request_id}
        result = self._request(origin, server_id, "POST", PAIR_PATH, body, request_id, 15, public_key=invitation["publicKey"])
        if (result.get("paired") is not True or result.get("serverId") != server_id
                or result.get("publicKey") != invitation["publicKey"] or result.get("origin") != origin
                or result.get("clientId") != self.local_server_id or result.get("tailscaleUser") != self._login()):
            raise AccessError(403, "server_mismatch", "The paired server identity does not match the invitation")
        with self._write() as db:
            peer = _record(db.execute("SELECT record FROM runtime_access_clients WHERE id=?", (server_id,)).fetchone())
            if peer and (peer.get("status") != "paired" or peer["publicKey"] != invitation["publicKey"]):
                raise AccessError(409, "server_conflict", "The server identity is already registered or revoked")
            if not peer:
                peer = {"id": server_id, "clientId": server_id, "serverId": server_id, "kind": "server", "origin": origin,
                        "publicKey": invitation["publicKey"], "label": invitation.get("label") or "Studio server",
                        "tailscaleUser": result["tailscaleUser"], "status": "paired", "created": time.time(),
                        "lastAccess": None, "revoked": None}
                db.execute("INSERT INTO runtime_access_clients VALUES(?,?)", (server_id, _json(peer)))
                self._audit(db, server_id, "accept_invite", actor)
        return self.snapshot()

    def request(self, server_id: str, method: str, path: str, body: dict[str, Any] | None,
                request_id: str, timeout: float = 15) -> dict[str, Any]:
        peer = self.paired_server(server_id)
        return self._request(peer["origin"], server_id, method, path, body, request_id, timeout, public_key=peer["publicKey"])

    def _request(self, origin: str, server_id: str, method: str, path: str, body: dict[str, Any] | None,
                 request_id: str, timeout: float, *, public_key: str | None = None) -> dict[str, Any]:
        origin = _origin(origin)
        _id(request_id, "request ID")
        if not isinstance(timeout, (int, float)) or not math.isfinite(timeout) or not 0 < timeout <= 120:
            raise AccessError(400, "invalid_timeout", "Use a timeout from 0 to 120 seconds")
        if not path.startswith("/api/") or "#" in path or any(ord(character) < 32 for character in path):
            raise AccessError(400, "invalid_target", "Use an API path on the paired server")
        method = method.upper()
        journal = not (method == "POST" and path == "/api/servers/orchestration")
        raw = _json(body).encode() if body is not None else b""
        if body is not None:
            for name in ("requestId", "request_id"):
                if name in body and body[name] != request_id:
                    raise AccessError(409, "request_conflict", "The operation ID does not match its signature")
        if len(raw) > 262_144:
            raise AccessError(413, "request_size", "The server request exceeds 256 KiB")
        fingerprint = hashlib.sha256(method.encode() + b"\n" + path.encode() + b"\n" + raw).hexdigest()
        with self._write() as db:
            row = db.execute("SELECT hash,record FROM runtime_access_outbound WHERE server=? AND request_id=?", (server_id, request_id)).fetchone() if journal else None
            if row:
                if row[0] != fingerprint:
                    raise AccessError(409, "request_conflict", "The request ID has different content")
                saved = cast(dict[str, Any], json.loads(row[1]))
                if saved.get("outcome") == "complete":
                    value = cast(dict[str, Any], saved["value"])
                    _check_response_identity(value, server_id, public_key)
                    return value
            elif journal:
                db.execute("INSERT INTO runtime_access_outbound VALUES(?,?,?,?)",
                           (server_id, request_id, fingerprint, _json({"outcome": "unknown", "created": time.time()})))
        headers = self.signed_headers(server_id, method, path, raw, request_id)
        if raw:
            headers["Content-Type"] = "application/json"
        headers["Accept-Encoding"] = "identity"
        response = _exchange(origin + path, method, headers, raw, timeout)
        if response.get("url") != origin + path or 300 <= response["status"] < 400:
            raise AccessError(403, "redirect_refused", "The server redirect was refused")
        try:
            result_bytes = base64.b64decode(response["body"], validate=True)
            if len(result_bytes) > MAX_RESPONSE_BYTES:
                raise AccessError(502, "response_size", "The server response exceeds 1 MiB")
            value = json.loads(result_bytes)
            if not isinstance(value, dict):
                raise ValueError
        except (ValueError, KeyError, TypeError):
            raise AccessError(502, "remote_response", "The server response is invalid. Keep the request ID") from None
        if not 200 <= response["status"] < 300:
            raise AccessError(response["status"], str(value.get("code") or "remote_error"), str(value.get("error") or "The server refused the request"))
        _check_response_identity(value, server_id, public_key)
        if value.get("outcome") in {"pending", "unknown"}:
            return value
        if journal:
            with self._write() as db:
                db.execute("UPDATE runtime_access_outbound SET record=? WHERE server=? AND request_id=?",
                           (_json({"outcome": "complete", "value": value, "completed": time.time()}), server_id, request_id))
        return value
