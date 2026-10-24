"""The guest side of the VM browser bridge (docs/vm-browser-bridge.md).

Agents run `studio-browser URL` (it is also `BROWSER` and `xdg-open`). The shim sends the
link on a Unix socket, and the kernel gives this service the peer UID. The service checks the
link, finds the chat, and queues a request. The Studio server reads the queue with
`browser.wait`, shows the person a card, and after an approval relays the one sign-in
redirect to the tool's loopback listener with `browser.callback`. Every connection to a
listener first checks in /proc/net/tcp that the listener belongs to the requester's UID.
"""
from __future__ import annotations

import asyncio
import base64
from dataclasses import dataclass
import http.client
import json
import os
from pathlib import Path
import socket
import struct
import tempfile
import time
from typing import Any, Awaitable, Callable
from urllib.parse import parse_qs, urljoin, urlsplit
import uuid

from browser_policy import PolicyError, Provider, StartPage, classify, provider
from shared_values_generated import (SIGN_IN_JOINED_SIGNED_IN_REASON, SIGN_IN_JOINED_STATE, SIGN_IN_NOT_SENT_CODE,
                                     SIGN_IN_OPEN_LIMIT_SECONDS)

SOCKET = Path("/run/codex-studio-browser/open.sock")
TTL_SECONDS = 600
# A pending request whose command still listens gets a new deadline when its current one is at
# most this many seconds away: one request time from now, at most SIGN_IN_OPEN_LIMIT_SECONDS
# from its creation (docs/vm-browser-bridge.md, "Card lifetime").
EXTEND_BEFORE_SECONDS = TTL_SECONDS / 2
KEEP_SECONDS = 3600
LIMIT_WINDOW_SECONDS = 600
LIMIT_COUNT = 6
CONNECTIONS_PER_UID = 2
LINE_LIMIT = 16 * 1024
READ_SECONDS = 5
CHECK_SECONDS = 5
MAX_RELAYS = 5
BODY_LIMIT = 256 * 1024
HEAD_LIMIT = 64 * 1024
SENT_HEADERS = ("user-agent", "accept", "accept-language")
# A joined request waits on the open request of another chat with the same Linux user, sign-in
# host and client (its `joinedTo`) instead of a card of its own (docs/vm-browser-bridge.md,
# "One card for the same sign-in").
OPEN_STATES = frozenset({"pending", "approved", SIGN_IN_JOINED_STATE})
CARD_STATES = frozenset({"pending", "approved"})
# States after which the same link of a chat can queue again.
REOPEN_STATES = frozenset({"stale", "expired", "replaced"})
# The listener addresses that a connection to 127.0.0.1 or ::1 can reach.
IPV4_REACH = frozenset({"127.0.0.1", "0.0.0.0", "::ffff:127.0.0.1", "::"})
IPV6_REACH = frozenset({"::1", "::"})

Context = Callable[[str], Awaitable[dict[str, Any]]]


class BridgeError(Exception):
    def __init__(self, code: str, message: str):
        super().__init__(message)
        self.code = code


@dataclass(frozen=True)
class Listener:
    address: str
    port: int
    uid: int


@dataclass(frozen=True)
class Reply:
    status: int
    location: str | None
    content_type: str
    body: bytes


def _address(text: str) -> str:
    data = bytes.fromhex(text)
    if len(data) == 4:
        return socket.inet_ntop(socket.AF_INET, data[::-1])
    # /proc writes IPv6 as four 32-bit words in host byte order.
    words = b"".join(data[index:index + 4][::-1] for index in range(0, 16, 4))
    return socket.inet_ntop(socket.AF_INET6, words)


def parse_listeners(text: str) -> list[Listener]:
    """The listening sockets of /proc/net/tcp or tcp6 text, with their owner UIDs."""
    rows = []
    for line in text.splitlines()[1:]:
        fields = line.split()
        if len(fields) < 8 or fields[3] != "0A":
            continue
        address, _, port = fields[1].partition(":")
        try:
            rows.append(Listener(_address(address), int(port, 16), int(fields[7])))
        except (ValueError, OSError):
            continue
    return rows


def proc_listeners(root: Path = Path("/proc/net")) -> list[Listener] | None:
    """The listeners of the guest, or None when /proc could not be read: the listeners are
    unknown, which is not the same as no listener."""
    rows: list[Listener] = []
    for name in ("tcp", "tcp6"):
        try:
            rows += parse_listeners((root / name).read_text())
        except FileNotFoundError:
            if name == "tcp":
                return None
            # A guest without IPv6 has no tcp6 table.
        except OSError:
            return None
    return rows


def known(rows: list[Listener] | None) -> list[Listener]:
    """The listeners for a step that must not guess: unknown listeners refuse with a
    retryable error, and the request stays open."""
    if rows is None:
        raise BridgeError(SIGN_IN_NOT_SENT_CODE,
                          "Studio could not read the listening ports of this computer. Try again")
    return rows


def connect_address(uid: int, port: int, listeners: list[Listener]) -> str:
    """The guest address of the listener of `uid` on `port`. Every listener that a loopback
    connection to that port can reach must belong to `uid`."""
    reachable = [row for row in listeners if row.port == port and row.address in IPV4_REACH | IPV6_REACH]
    if not reachable:
        raise BridgeError("stale", f"The sign-in command no longer listens on port {port}")
    if any(row.uid != uid for row in reachable):
        raise BridgeError("forbidden", f"Another Linux user listens on port {port}")
    addresses = {row.address for row in reachable}
    return "127.0.0.1" if addresses & (IPV4_REACH - {"::"}) else "::1"


def fetch(address: str, port: int, target: str, headers: dict[str, str], *, timeout: float, limit: int) -> Reply:
    """One GET to a guest loopback listener. No redirect is followed."""
    connection = http.client.HTTPConnection(address, port, timeout=timeout)
    try:
        connection.request("GET", target, headers=headers)
        response = connection.getresponse()
        body = response.read(limit + 1)
        if len(body) > limit:
            raise BridgeError("output_limit", f"The page on port {port} is larger than {limit} bytes")
        return Reply(response.status, response.getheader("Location"), response.getheader("Content-Type") or "", body)
    except (OSError, http.client.HTTPException) as error:
        raise BridgeError("unavailable", f"The sign-in command did not answer on port {port}") from error
    finally:
        connection.close()


def peer_uid(connection: Any) -> int:
    option = getattr(socket, "SO_PEERCRED", None)
    if option is not None:
        data = connection.getsockopt(socket.SOL_SOCKET, option, struct.calcsize("3i"))
        return int(struct.unpack("3i", data)[1])
    # macOS, for tests: LOCAL_PEERCRED gives struct xucred, where cr_uid follows cr_version.
    data = connection.getsockopt(0, 0x001, 76)
    return int(struct.unpack_from("2I", data)[1])


def _write_json(path: Path, value: Any) -> None:
    descriptor, temporary = tempfile.mkstemp(prefix=".browser-", dir=path.parent)
    try:
        with os.fdopen(descriptor, "w") as output:
            json.dump(value, output, separators=(",", ":"))
            output.flush()
            os.fsync(output.fileno())
        os.replace(temporary, path)
    finally:
        Path(temporary).unlink(missing_ok=True)


def _session(value: object) -> str:
    try:
        if isinstance(value, str) and str(uuid.UUID(value)) == value:
            return value
    except ValueError:
        pass
    raise BridgeError("unknown_agent", "Run the command in a Studio chat: LAYR_SESSION is missing or invalid")


def _clean_headers(headers: object) -> dict[str, str]:
    result: dict[str, str] = {}
    if not isinstance(headers, dict):
        return result
    for name in SENT_HEADERS:
        value = headers.get(name)
        if isinstance(value, str) and len(value) <= 1024 and not any(ord(c) < 32 for c in value):
            result[name] = value
    return result


def listening_deadline(item: dict[str, Any], now: float) -> float | None:
    """The new deadline of a pending request whose command still listens, or None to keep it:
    one request time from now, at most SIGN_IN_OPEN_LIMIT_SECONDS from its creation."""
    limit = item["createdAt"] + SIGN_IN_OPEN_LIMIT_SECONDS
    rolled = min(now + TTL_SECONDS, limit)
    if rolled > item["expiresAt"] and (rolled - item["expiresAt"] >= EXTEND_BEFORE_SECONDS or rolled >= limit):
        return rolled
    return None


def _signed_in(item: dict[str, Any]) -> bool:
    """Whether the sign-in of a request that ended finished: the command took a relayed result,
    a device page opened, or the person approved the card and its command then stopped
    listening."""
    if item["state"] == "closed":
        return item.get("closeReason") == "completed"
    if item["state"] == "opened":
        return True
    return item["state"] == "stale" and isinstance(item.get("lease"), dict)


class BrowserBridge:
    def __init__(self, state: Path, context: Context, *,
                 listeners: Callable[[], list[Listener] | None] = proc_listeners,
                 http_providers: frozenset[str] = frozenset(),
                 clock: Callable[[], float] = time.time):
        self.context, self.listeners, self.http_providers, self.clock = context, listeners, http_providers, clock
        directory = state / "browser"
        directory.mkdir(mode=0o700, parents=True, exist_ok=True)
        directory.chmod(0o700)
        self.path = directory / "requests.json"
        saved: dict[str, Any] = json.loads(self.path.read_text()) if self.path.exists() else {}
        self.seq = int(saved.get("seq", 0))
        self.items: dict[str, dict[str, Any]] = dict(saved.get("items", {}))
        self.changed = asyncio.Condition()
        self.connections: dict[int, int] = {}
        self.server: asyncio.AbstractServer | None = None
        self.checker: asyncio.Task[None] | None = None

    # The agent side.

    async def listen(self, path: Path = SOCKET) -> None:
        path.unlink(missing_ok=True)
        self.server = await asyncio.start_unix_server(self.client, path=str(path), limit=LINE_LIMIT + 1)
        # Every agent user connects. The kernel peer UID is the access control.
        path.chmod(0o666)
        self.checker = asyncio.create_task(self.check_loop())

    async def close(self) -> None:
        if self.checker is not None:
            self.checker.cancel()
            await asyncio.gather(self.checker, return_exceptions=True)
        if self.server is not None:
            self.server.close()
            await self.server.wait_closed()

    async def client(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        try:
            uid = peer_uid(writer.get_extra_info("socket"))
        except OSError:
            writer.close()
            return
        if self.connections.get(uid, 0) >= CONNECTIONS_PER_UID:
            writer.close()
            return
        self.connections[uid] = self.connections.get(uid, 0) + 1
        try:
            try:
                line = await asyncio.wait_for(reader.readline(), READ_SECONDS)
                reply = await self.handle(uid, line)
            except (asyncio.TimeoutError, ValueError, OSError):
                reply = {"ok": False, "code": "invalid_request", "message": "Send one JSON line within 5 seconds"}
            writer.write(json.dumps(reply).encode() + b"\n")
            await asyncio.wait_for(writer.drain(), READ_SECONDS)
        except (asyncio.TimeoutError, OSError):
            pass
        finally:
            self.connections[uid] -= 1
            if not self.connections[uid]:
                del self.connections[uid]
            writer.close()

    async def handle(self, uid: int, line: bytes) -> dict[str, Any]:
        try:
            if len(line) > LINE_LIMIT or not line.endswith(b"\n"):
                raise BridgeError("invalid_request", "Send one JSON line of at most 16 KB")
            value = json.loads(line)
            if not isinstance(value, dict) or value.get("v") != 1:
                raise BridgeError("invalid_request", "Use protocol version 1")
            item = await self.open(uid, value.get("url"), value.get("session"))
            return {"ok": True, "id": item["id"], "host": item["host"],
                    "joined": item["state"] == SIGN_IN_JOINED_STATE}
        except BridgeError as error:
            return {"ok": False, "code": error.code, "message": str(error)}
        except PolicyError as error:
            return {"ok": False, "code": "invalid_url", "message": str(error)}
        except ValueError:
            return {"ok": False, "code": "invalid_request", "message": "The request is not valid JSON"}
        except Exception:  # noqa: BLE001 - the agent gets an answer, not a closed socket
            return {"ok": False, "code": "internal", "message": "The guest could not queue the link"}

    async def open(self, uid: int, url: object, session: object) -> dict[str, Any]:
        agent = _session(session)
        try:
            context = await self.context(agent)
        except Exception:
            raise BridgeError("unknown_agent", "Studio does not know the chat in LAYR_SESSION") from None
        if context.get("uid") != uid:
            raise BridgeError("unknown_agent", "LAYR_SESSION names a chat of another Linux user")
        return await self.queue(uid, url, agent, context)

    async def submit(self, params: dict[str, Any]) -> dict[str, Any]:
        """`browser.submit`: the Studio server queues a link for a chat, from the agent's
        `orchestration_sign_in` call or from a sign-in link in its message
        (docs/vm-browser-bridge.md, section 7). The chat's own Linux user owns the request, so the
        listener checks stay those of a link that the chat's command opened itself."""
        agent = _session(params.get("agentId"))
        try:
            context = await self.context(agent)
        except Exception:
            raise BridgeError("unknown_agent", "Studio does not know this chat") from None
        uid = context.get("uid")
        if type(uid) is not int:
            raise BridgeError("unknown_agent", "Studio does not know the Linux user of this chat")
        try:
            return dict(await self.queue(uid, params.get("url"), agent, context))
        except PolicyError as error:
            raise BridgeError("invalid_url", str(error)) from None

    def _existing(self, agent: str, url: object, now: float) -> dict[str, Any] | None:
        """The request of this chat for the same link, while it can still answer. A link that a
        command opens and that the agent also gives in a message or a tool call gets one card."""
        if not isinstance(url, str):
            return None
        for item in self.items.values():
            if item["agentId"] == agent and url in (item["url"], item.get("startUrl")) \
                    and item["state"] not in REOPEN_STATES and item["createdAt"] > now - TTL_SECONDS:
                return item
        return None

    def _leader(self, uid: int, agent: str, final: Provider, now: float) -> str | None:
        """The oldest open request of another chat with the same Linux user, sign-in host and
        client, while it can still answer. A device code page has no client and never joins."""
        if final.client_id is None:
            return None
        open_requests = [item for item in self.items.values()
                         if item["state"] in CARD_STATES and item["agentId"] != agent and item["uid"] == uid
                         and item["host"] == final.host and item.get("clientId") == final.client_id
                         and now <= item["expiresAt"]]
        return min(open_requests, key=lambda item: item["createdAt"])["id"] if open_requests else None

    async def queue(self, uid: int, url: object, agent: str, context: dict[str, Any]) -> dict[str, Any]:
        # A native chat names itself by its session; the context gives its Studio chat.
        agent = str(context.get("agentId") or agent)
        now = self.clock()
        existing = self._existing(agent, url, now)
        if existing is not None:
            return existing
        recent = [item for item in self.items.values()
                  if item["uid"] == uid and item["createdAt"] > now - LIMIT_WINDOW_SECONDS]
        if len(recent) >= LIMIT_COUNT:
            raise BridgeError("limit", f"This Linux user asked {LIMIT_COUNT} times in 10 minutes. Wait, then try again")
        checked = classify(url, http_providers=self.http_providers)
        start_url = None
        final: Provider
        if isinstance(checked, StartPage):
            start_url = checked.url
            parts = urlsplit(checked.url)
            address = connect_address(uid, checked.port, known(self.listeners()))
            target = (parts.path or "/") + ("?" + parts.query if parts.query else "")
            reply = await asyncio.to_thread(fetch, address, checked.port, target,
                                            {"Host": parts.netloc, "Accept": "text/html"},
                                            timeout=READ_SECONDS, limit=HEAD_LIMIT)
            if not (300 <= reply.status < 400 and reply.location):
                raise BridgeError("invalid_url", f"The page on port {checked.port} does not redirect to a sign-in "
                                  "page. Studio opens only pages that redirect to https")
            final = provider(urljoin(checked.url, reply.location), http_providers=self.http_providers)
        else:
            final = checked
        if final.redirect is not None:
            connect_address(uid, final.redirect.port, known(self.listeners()))
        async with self.changed:
            existing = self._existing(agent, url, now)
            if existing is not None:
                return existing
            leader = self._leader(uid, agent, final, now)
            # The latest link of a chat replaces its earlier pending or joined link.
            for item in self.items.values():
                if item["agentId"] == agent and item["state"] in {"pending", SIGN_IN_JOINED_STATE}:
                    self._update(item, state="replaced")
            item = {"id": uuid.uuid4().hex, "agentId": agent, "projectId": context.get("projectId"),
                    "uid": uid, "linuxUser": context.get("owner"), "url": final.url, "startUrl": start_url,
                    "host": final.host, "redirect": final.redirect.record() if final.redirect else None,
                    "clientId": final.client_id, "scope": final.scope, "state": "pending",
                    "createdAt": now, "expiresAt": now + TTL_SECONDS, "relays": 0, "lease": None}
            if leader is not None:
                item.update(state=SIGN_IN_JOINED_STATE, joinedTo=leader)
            self.items[item["id"]] = item
            self._update(item)
            self._save()
        return item

    # The Studio server side (guest JSON-RPC methods).

    def _update(self, item: dict[str, Any], **fields: Any) -> None:
        item.update(fields)
        self.seq += 1
        item["seq"] = self.seq
        self.changed.notify_all()

    def _save(self) -> None:
        now = self.clock()
        for key in [key for key, item in self.items.items()
                    if item["state"] not in OPEN_STATES and item["createdAt"] < now - KEEP_SECONDS]:
            del self.items[key]
        _write_json(self.path, {"seq": self.seq, "items": self.items})

    def _item(self, params: dict[str, Any]) -> dict[str, Any]:
        key = params.get("id")
        item = self.items.get(key) if isinstance(key, str) else None
        if item is None:
            raise BridgeError("not_found", "The sign-in request does not exist")
        return item

    async def call(self, method: str, params: dict[str, Any]) -> dict[str, Any]:
        if method == "browser.wait":
            return await self.wait(params)
        if method == "browser.submit":
            return await self.submit(params)
        async with self.changed:
            if method == "browser.answer":
                return self.answer(params)
            if method == "browser.close":
                item = self._item(params)
                if item["state"] in OPEN_STATES:
                    reason = params.get("reason")
                    self._update(item, state="closed", closeReason=reason if isinstance(reason, str) else "closed")
                    self._save()
                return {"state": item["state"]}
        if method == "browser.callback":
            return await self.callback(params)
        raise BridgeError("invalid_request", "The browser method is not supported")

    async def wait(self, params: dict[str, Any]) -> dict[str, Any]:
        after, seconds = params.get("after", 0), params.get("waitSeconds", 0)
        if type(after) is not int or after < 0 or type(seconds) not in {int, float} or not 0 <= seconds <= 20:
            raise BridgeError("invalid_params", "after must be a cursor and waitSeconds at most 20")
        async with self.changed:
            if after > self.seq:
                after = 0  # A guest state reset. Send everything again.
            try:
                await asyncio.wait_for(self.changed.wait_for(lambda: self.seq > after), seconds)
            except asyncio.TimeoutError:
                pass
            items = [dict(item) for item in self.items.values() if item["seq"] > after]
            return {"items": sorted(items, key=lambda item: int(item["seq"])), "next": self.seq}

    def answer(self, params: dict[str, Any]) -> dict[str, Any]:
        item = self._item(params)
        decision = params.get("decision")
        if decision == "decline":
            if item["state"] == "pending":
                self._update(item, state="declined")
                self._save()
            return {"state": item["state"], "lease": item.get("lease")}
        if decision != "approve":
            raise BridgeError("invalid_params", "decision must be approve or decline")
        if item["state"] in {"approved", "opened"}:
            return {"state": item["state"], "lease": item["lease"]}
        if item["state"] != "pending" or self.clock() > item["expiresAt"]:
            raise BridgeError("stale", "The sign-in request is no longer open. Ask the agent to run the command again")
        redirect = item.get("redirect")
        if redirect is None:
            # A link without a loopback redirect (a device code page) needs no forward.
            self._update(item, state="opened")
            self._save()
            return {"state": "opened", "lease": None}
        rows = known(self.listeners())
        try:
            connect_address(item["uid"], redirect["port"], rows)
        except BridgeError as error:
            self._update(item, state="stale")
            self._save()
            raise BridgeError("stale", str(error)) from None
        lease = {**redirect, "expiresAt": self.clock() + TTL_SECONDS}
        self._update(item, state="approved", lease=lease)
        self._save()
        return {"state": "approved", "lease": lease}

    async def callback(self, params: dict[str, Any]) -> dict[str, Any]:
        item = self._item(params)
        lease = item.get("lease")
        if item["state"] != "approved" or not lease or self.clock() > lease["expiresAt"]:
            raise BridgeError("stale", "The sign-in forward is closed")
        if item["relays"] >= MAX_RELAYS:
            raise BridgeError("limit", "The sign-in forward relayed its maximum number of requests")
        target = params.get("target")
        parts = urlsplit(target if isinstance(target, str) else "")
        if not isinstance(target, str) or not target.startswith("/") or parts.scheme or parts.netloc \
                or (parts.path or "/") != lease["path"] or len(target) > 8192:
            raise BridgeError("invalid_params", "Only the sign-in redirect path can pass")
        port = int(lease["port"])
        address = connect_address(item["uid"], port, known(self.listeners()))
        host = ("[::1]" if lease["host"] == "::1" else lease["host"]) + ":" + str(port)
        headers = {"Host": host, **_clean_headers(params.get("headers")), "Connection": "close"}
        async with self.changed:
            item["relays"] += 1
            self._save()
        reply = await asyncio.to_thread(fetch, address, port, target, headers, timeout=10, limit=BODY_LIMIT)
        query = parse_qs(parts.query)
        done = "code" in query or "error" in query
        async with self.changed:
            if done and item["state"] == "approved":
                self._update(item, state="closed", closeReason="completed")
            self._save()
        return {"status": reply.status, "contentType": reply.content_type[:200],
                "body": base64.b64encode(reply.body).decode(), "done": done}

    async def check(self) -> None:
        """Mark requests whose time ran out or whose tool stopped listening. A pending request
        whose tool still listens stays open up to SIGN_IN_OPEN_LIMIT_SECONDS."""
        async with self.changed:
            now, rows, probed, changed = self.clock(), None, False, False
            for item in self.items.values():
                if item["state"] not in OPEN_STATES:
                    continue
                expires = (item.get("lease") or {}).get("expiresAt", item["expiresAt"])
                # None: no loopback redirect, or unknown listeners (the probe failed). Unknown
                # listeners neither make a request stale nor move its deadline.
                redirect, listening = item.get("redirect"), None
                if redirect is not None:
                    if not probed:
                        rows, probed = self.listeners(), True
                    if rows is not None:
                        try:
                            connect_address(item["uid"], redirect["port"], rows)
                            listening = True
                        except BridgeError:
                            listening = False
                if item["state"] in {"pending", SIGN_IN_JOINED_STATE} and listening:
                    deadline = listening_deadline(item, now)
                    if deadline is not None:
                        self._update(item, expiresAt=deadline)
                        changed, expires = True, deadline
                if now > expires:
                    self._update(item, state="expired")
                    changed = True
                elif listening is False:
                    self._update(item, state="stale")
                    changed = True
            changed |= self._settle_joined()
            if changed:
                self._save()

    def _settle_joined(self) -> bool:
        """End the joined requests whose card ended: closed as signed in when its sign-in
        finished, else the oldest joined request becomes the card and the others wait on it."""
        waiting: dict[str, list[dict[str, Any]]] = {}
        for item in self.items.values():
            if item["state"] == SIGN_IN_JOINED_STATE:
                waiting.setdefault(item.get("joinedTo") or "", []).append(item)
        changed = False
        for leader_id, joined in waiting.items():
            leader = self.items.get(leader_id)
            if leader is not None and leader["state"] in OPEN_STATES:
                continue
            changed = True
            if leader is not None and _signed_in(leader):
                for item in joined:
                    self._update(item, state="closed", closeReason=SIGN_IN_JOINED_SIGNED_IN_REASON)
                continue
            joined.sort(key=lambda item: item["createdAt"])
            card, rest = joined[0], joined[1:]
            self._update(card, state="pending", joinedTo=None)
            for item in rest:
                self._update(item, joinedTo=card["id"])
        return changed

    async def check_loop(self) -> None:
        while True:
            await asyncio.sleep(CHECK_SECONDS)
            try:
                await self.check()
            except OSError:
                continue
