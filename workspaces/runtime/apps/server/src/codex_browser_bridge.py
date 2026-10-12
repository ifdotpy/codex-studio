"""Open sign-in links of agents in the browser of the person (docs/vm-browser-bridge.md).

The guest service queues each link that a command in the VM asks to open
(`browser_bridge.py`). This module reads the queue, shows the person a `browser/open` request
card in the agent's chat, and opens nothing before the person approves. An approval binds
the loopback port of the link's redirect on this Mac, strictly, and the renderer opens the
link. The browser's sign-in redirect then reaches the tool in the VM through the guest
service, as one cleaned GET. Studio knows no tool: only URLs, ports and chats.

The same flow serves native chats of this server (`codex_browser_host.py`), and a window on
another Mac: the person's own server then holds the loopback port and relays the redirect to
this server over the paired channel, with the forward token of the approval.
"""
from __future__ import annotations

import base64
import binascii
import errno
import hashlib
import hmac
import html
import http
import json
import platform
import re
import secrets
import select
import socket
import sys
import threading
import time
from typing import Any, Callable, Protocol
import uuid

from codex_layout import VM_GUEST_ROOT
from codex_linux_vm import LinuxVMError
from codex_questions import answer_signature, record_answer

if str(VM_GUEST_ROOT) not in sys.path:
    # The guest's URL rules are the one rule set (browser_policy.py). Append, so no guest
    # module shadows a server module.
    sys.path.append(str(VM_GUEST_ROOT))

from browser_policy import PolicyError, provider  # noqa: E402

METHOD = "browser/open"
REMOTE_MESSAGE = ("Open this chat in the window of the Mac that runs its server to sign in, "
                  "or use the tool's device-code mode.")
REMOTE_ACTION = "browser_callback"
# Answers that end a relay without a code reaching the tool.
DEFINITE_CODES = frozenset({"stale", "limit", "invalid_params", "not_found"})
POLL_SECONDS = 20
FORWARD_SECONDS = 600
MAX_RELAYS = 5
MAX_CONNECTIONS = 8
HEAD_LIMIT = 16 * 1024
BODY_LIMIT = 256 * 1024
SENT_HEADERS = ("user-agent", "accept", "accept-language")
SANDBOX = "sandbox; default-src 'none'; style-src 'unsafe-inline'; img-src data:"
FINAL_STATES = frozenset({"declined", "stale", "expired", "closed", "replaced", "opened"})
CHARSET = re.compile(r"charset=[A-Za-z0-9._-]{1,40}")

Relay = Callable[[str, dict[str, str]], Any]


class Guest(Protocol):
    def call(self, method: str, params: dict[str, Any] | None = None, *,
             request_id: str | None = None, timeout: float = 60) -> Any: ...


class PortBusy(Exception):
    pass


def card_id(bridge_id: str) -> str:
    """One card for each guest request, also after a lost reply or a restart."""
    return str(uuid.uuid5(uuid.NAMESPACE_URL, "studio-browser-open:" + bridge_id))


def bind_addresses(host: str) -> list[tuple[socket.AddressFamily, str]]:
    """Every address on which the browser can reach the redirect host."""
    if host == "localhost":
        return [(socket.AF_INET, "127.0.0.1"), (socket.AF_INET6, "::1")]
    if host == "::1":
        return [(socket.AF_INET6, "::1")]
    return [(socket.AF_INET, "127.0.0.1")]


def page(text: str) -> bytes:
    return ("<!doctype html><meta charset=utf-8><title>LAYR Studio</title><p>"
            + html.escape(text) + "</p>").encode()


def respond(connection: socket.socket, status: int, body: bytes,
            content_type: str = "text/html; charset=utf-8") -> None:
    try:
        phrase = http.HTTPStatus(status).phrase
    except ValueError:
        phrase = "Status"
    # Studio writes every header. No Set-Cookie or Location of the tool reaches the browser.
    head = [f"HTTP/1.1 {status} {phrase}", "Content-Type: " + content_type,
            "Content-Length: " + str(len(body)), "Content-Security-Policy: " + SANDBOX,
            "Cache-Control: no-store", "X-Content-Type-Options: nosniff",
            "Referrer-Policy: no-referrer", "Connection: close"]
    connection.sendall(("\r\n".join(head) + "\r\n\r\n").encode("latin-1") + body)


def cleaned(result: Any) -> tuple[int, str, bytes]:
    """The status, content type and body of a relay result that the browser may see."""
    if not isinstance(result, dict):
        raise ValueError("The relay result is invalid")
    status = result.get("status")
    if type(status) is not int or not 100 <= status <= 599:
        raise ValueError("The relay status is invalid")
    if 300 <= status < 400:
        return 200, "text/html; charset=utf-8", page(
            "The sign-in command answered with a redirect. Studio does not follow it. Return to Studio.")
    try:
        body = base64.b64decode(str(result.get("body", "")), validate=True)
    except (binascii.Error, ValueError):
        raise ValueError("The relay body is invalid") from None
    if len(body) > BODY_LIMIT:
        raise ValueError("The relay body is too large")
    text = str(result.get("contentType") or "").lower()
    base = text.split(";", 1)[0].strip()
    if base not in {"text/html", "text/plain"}:
        base = "text/plain"
    charset = CHARSET.search(text)
    return status, base + "; " + (charset.group(0) if charset else "charset=utf-8"), body


def read_head(connection: socket.socket) -> bytes:
    data = b""
    while b"\r\n\r\n" not in data:
        block = connection.recv(4096)
        if not block:
            raise ValueError("The request ended early")
        data += block
        if len(data) > HEAD_LIMIT:
            raise ValueError("The request head is too large")
    return data.split(b"\r\n\r\n", 1)[0]


class Forward:
    """One approved loopback port on this Mac, for the sign-in redirect of one request."""

    def __init__(self, bridge_id: str, redirect: dict[str, Any], relay: Relay,
                 closed: Callable[["Forward", str], None], *, seconds: float = FORWARD_SECONDS,
                 listen_port: int | None = None) -> None:
        self.bridge_id = bridge_id
        self.host, self.port, self.path = str(redirect["host"]), int(redirect["port"]), str(redirect["path"])
        self.relay, self.closed_callback = relay, closed
        self.deadline = time.monotonic() + seconds
        # Tests run the tool and this forward on one host, so they bind another port.
        self.listen_port = self.port if listen_port is None else listen_port
        self.bound_port = self.listen_port
        self.sockets: list[socket.socket] = []
        self.lock = threading.Lock()
        self.relays = self.active = 0
        self.closed = False
        self.reason = ""

    def bind(self) -> None:
        port = self.listen_port
        for family, address in bind_addresses(self.host):
            listener = socket.socket(family, socket.SOCK_STREAM)
            try:
                if family == socket.AF_INET6:
                    listener.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_V6ONLY, 1)
                # Never SO_REUSEADDR: with it, macOS lets this bind take the connections of a
                # program that holds the wildcard address of the same port.
                listener.bind((address, port))
                listener.listen(16)
            except OSError as error:
                listener.close()
                if (family == socket.AF_INET6 and self.host == "localhost"
                        and error.errno in {errno.EADDRNOTAVAIL, errno.EAFNOSUPPORT}):
                    continue  # No IPv6 loopback on this Mac, so the browser cannot use ::1 either.
                for other in self.sockets:
                    other.close()
                self.sockets = []
                raise PortBusy(f"Port {self.port} is in use on this Mac. Free it and approve again, "
                               "or decline.") from error
            port = int(listener.getsockname()[1])
            self.sockets.append(listener)
        self.bound_port = port

    def start(self) -> None:
        threading.Thread(target=self.serve, name="studio-browser-forward", daemon=True).start()

    def close(self, reason: str) -> None:
        with self.lock:
            if self.closed:
                return
            self.closed, self.reason = True, reason
            sockets, self.sockets = self.sockets, []
        for listener in sockets:
            listener.close()
        self.closed_callback(self, reason)

    def serve(self) -> None:
        while True:
            with self.lock:
                if self.closed:
                    return
                sockets = list(self.sockets)
            if time.monotonic() > self.deadline:
                self.close("timeout")
                return
            try:
                ready, _, _ = select.select(sockets, [], [], 0.5)
            except (OSError, ValueError):
                continue  # A socket closed between the copy and the select.
            for listener in ready:
                try:
                    connection, _ = listener.accept()
                except OSError:
                    continue
                with self.lock:
                    admitted = not self.closed and self.active < MAX_CONNECTIONS
                    self.active += admitted
                if not admitted:
                    connection.close()
                    continue
                threading.Thread(target=self.connection, args=(connection,), daemon=True).start()

    def connection(self, connection: socket.socket) -> None:
        try:
            self.handle(connection)
        except (OSError, ValueError):
            pass
        finally:
            connection.close()
            with self.lock:
                self.active -= 1

    def handle(self, connection: socket.socket) -> None:
        connection.settimeout(10)
        lines = read_head(connection).decode("latin-1").split("\r\n")
        parts = lines[0].split(" ")
        if len(parts) != 3 or not parts[2].startswith("HTTP/1."):
            respond(connection, 400, page("Studio did not understand this request."))
            return
        method, target = parts[0], parts[1]
        if method != "GET":
            respond(connection, 405, page("Studio forwards only the sign-in redirect."))
            return
        path = target.split("?", 1)[0] or "/"
        if not target.startswith("/") or path != self.path:
            respond(connection, 404, page("Studio forwards only the sign-in redirect."))
            return
        headers: dict[str, str] = {}
        for line in lines[1:]:
            name, separator, value = line.partition(":")
            if separator and name.strip().lower() not in headers:
                headers[name.strip().lower()] = value.strip()
        # Cookie, Authorization, Referer and Origin never reach the tool.
        sent = {name: headers[name] for name in SENT_HEADERS
                if name in headers and len(headers[name]) <= 1024}
        with self.lock:
            refused = self.closed or self.relays >= MAX_RELAYS
            self.relays += not refused
        if refused:
            respond(connection, 410, page("This sign-in forward is closed. Return to Studio."))
            return
        try:
            result = self.relay(target, sent)
            status, content_type, body = cleaned(result)
        except Exception:  # noqa: BLE001 - the code may be used; never send it again
            respond(connection, 502, page("The sign-in result is unknown. Check the chat in Studio."))
            self.close("unknown")
            return
        respond(connection, status, body, content_type)
        if result.get("done") is True:
            self.close("completed")
        elif self.relays >= MAX_RELAYS:
            self.close("limit")


def token_hash(token: str) -> str:
    return hashlib.sha256(token.encode()).hexdigest()


class Bridge:
    def __init__(self, runtime: Any, guest: Callable[[], Guest], *,
                 http_providers: frozenset[str] = frozenset(),
                 listen_port: Callable[[int], int] | None = None,
                 forward_seconds: float = FORWARD_SECONDS, source: str = "vm",
                 server_id: Callable[[], str] | None = None) -> None:
        self.runtime, self.guest = runtime, guest
        # "vm": links of VM chats from the guest. "host": links of native chats of this server.
        self.source = source
        self.server_id = server_id or (lambda: str(runtime.multi_server().server_id))
        # The forward token of a remote approval, for a replay after a lost reply.
        self.tokens: dict[str, str] = {}
        self.http_providers, self.listen_port, self.forward_seconds = http_providers, listen_port, forward_seconds
        self.forwards: dict[str, Forward] = {}
        self.answering: set[str] = set()
        self.after = 0
        self.lock = threading.Lock()
        self.stopped = threading.Event()
        self.thread: threading.Thread | None = None

    def start(self) -> None:
        with self.lock:
            if self.thread is not None or self.stopped.is_set():
                return
            self.thread = threading.Thread(target=self.run, name="studio-browser-" + self.source, daemon=True)
        self.thread.start()

    def stop(self) -> None:
        self.stopped.set()
        with self.lock:
            forwards = list(self.forwards.values())
        for forward in forwards:
            forward.close("shutdown")

    def run(self) -> None:
        while not self.stopped.is_set() and not getattr(self.runtime, "closed", False):
            try:
                self.poll(POLL_SECONDS)
                continue
            except LinuxVMError as error:
                # An older guest refuses the unknown method with invalid_params. A stopped VM
                # has no guest socket.
                delay = 120 if error.code in {"invalid_params", "invalid_request"} else 15
            except (OSError, ValueError, KeyError, TypeError) as error:
                print("browser bridge poll: " + type(error).__name__ + ": " + str(error)[:200], file=sys.stderr)
                delay = 15
            self.stopped.wait(delay)

    def poll(self, seconds: float) -> None:
        result = self.guest().call("browser.wait", {"after": self.after, "waitSeconds": seconds},
                                   timeout=seconds + 15)
        for item in result["items"]:
            self.apply(item)
        self.after = int(result["next"])

    def refusal(self, db: Any, item: dict[str, Any]) -> str | None:
        """Why a queued link gets no card, or None."""
        try:
            agent = self.runtime.agent(str(item.get("agentId")), db)
        except ValueError:
            return "The chat does not exist"
        if agent.get("deletedAt"):
            return "The chat was deleted"
        if (agent.get("executionMode") == "vm") != (self.source == "vm"):
            return "The chat is not an active VM chat" if self.source == "vm" else "The chat is not a native chat"
        if agent.get("layrProjectId") and item.get("projectId") != agent["layrProjectId"]:
            return "The chat belongs to another project"
        try:
            checked = provider(item.get("url"), http_providers=self.http_providers)
        except PolicyError as error:
            return str(error)
        if (checked.redirect.record() if checked.redirect else None) != item.get("redirect") \
                or checked.host != item.get("host"):
            return "The link differs from its checked redirect"
        return None

    def apply(self, item: dict[str, Any]) -> None:
        bridge_id, state = str(item["id"]), item.get("state")
        key = card_id(bridge_id)
        refused = None
        with self.runtime.lock, self.runtime.db() as db:
            row = db.execute("SELECT record FROM runtime_requests WHERE id=?", (key,)).fetchone()
            if row is None:
                if state == "pending":
                    refused = self.refusal(db, item)
                    if refused is None:
                        self.runtime.put(db, "requests", {
                            "id": key, "method": METHOD, "agent": item["agentId"], "status": "pending",
                            "createdAt": time.time(), "params": {
                                "bridgeId": bridge_id, "url": item["url"], "host": item["host"],
                                "redirect": item.get("redirect"), "clientId": item.get("clientId"),
                                "scope": item.get("scope"), "linuxUser": item.get("linuxUser"),
                                "expiresAt": item.get("expiresAt"), "source": self.source}})
            elif state in FINAL_STATES:
                record = json.loads(row[0])
                if record.get("status") == "pending":
                    record.update(status="expired", closeReason=state)
                    self.runtime.put(db, "requests", record)
        if refused is not None:
            print("browser bridge refused link " + bridge_id + ": " + refused, file=sys.stderr)
            self.close_guest(bridge_id, "refused")
        if state in FINAL_STATES:
            with self.lock:
                forward = self.forwards.get(bridge_id)
            if forward is not None:
                forward.close("guest " + str(state))

    def close_guest(self, bridge_id: str, reason: str) -> None:
        try:
            self.guest().call("browser.close", {"id": bridge_id, "reason": reason},
                              request_id="browser-close:" + bridge_id, timeout=15)
        except (LinuxVMError, OSError):
            pass  # The guest request ends at its own deadline.

    def relay(self, bridge_id: str, target: str, headers: dict[str, str]) -> Any:
        # The guest keeps a receipt for this request ID; Studio never sends a second one.
        return self.guest().call("browser.callback", {"id": bridge_id, "target": target, "headers": headers},
                                 request_id="browser-callback:" + uuid.uuid4().hex, timeout=20)

    def forward_closed(self, forward: Forward, reason: str) -> None:
        with self.lock:
            if self.forwards.get(forward.bridge_id) is forward:
                del self.forwards[forward.bridge_id]
        if not reason.startswith("guest "):
            self.close_guest(forward.bridge_id, reason)

    def record(self, key: str) -> dict[str, Any]:
        with self.runtime.lock, self.runtime.db() as db:
            row = db.execute("SELECT record FROM runtime_requests WHERE id=?", (key,)).fetchone()
        if row is None:
            raise ValueError("Unknown request")
        value: dict[str, Any] = json.loads(row[0])
        return value

    def finish(self, key: str, decision: str, *, event: str | None = None,
               forward: dict[str, Any] | None = None) -> None:
        with self.runtime.lock, self.runtime.db() as db:
            row = db.execute("SELECT record FROM runtime_requests WHERE id=?", (key,)).fetchone()
            record = json.loads(row[0])
            if decision == "expired":
                if record["status"] == "pending":
                    record.update(status="expired", closeReason="stale")
                    self.runtime.put(db, "requests", record)
                return
            record["status"] = "answered"
            if forward is not None:
                record["params"]["forward"] = forward
            record_answer(record, {"decision": decision})
            self.runtime.put(db, "requests", record)
            if event and record.get("agent"):
                agent = self.runtime.agent(record["agent"], db)
                self.runtime.enqueue(db, agent, "browser_declined", event,
                                     "browser-declined:" + record["params"]["bridgeId"])

    def answer(self, key: str, data: dict[str, Any], *, local: bool) -> dict[str, Any]:
        """Answer a card. A window on another Mac sends `forward: true`: its own server then
        holds the loopback port and relays the redirect here (open_remote_forward)."""
        decision = data.get("decision")
        remote = decision == "accept" and not local and data.get("forward") is True
        if decision not in {"accept", "decline"}:
            raise ValueError("Choose accept or decline")
        record = self.record(key)
        params = record["params"]
        bridge_id, url = str(params["bridgeId"]), str(params["url"])
        if record.get("answerSignature"):
            if record["answerSignature"] != answer_signature({"decision": decision}):
                raise ValueError("This request already has a different answer")
            with self.lock:
                forwarding = bridge_id in self.forwards
            if decision == "accept" and local and (forwarding or not params.get("redirect")
                                                   or self.source == "host"):
                return {"status": "answered", "replayed": True, "openUrl": url}
            if remote and not params.get("redirect"):
                return {"status": "answered", "replayed": True, "openUrl": url}
            token = self.tokens.get(bridge_id)
            if remote and token is not None:
                return {"status": "answered", "replayed": True, "openUrl": url,
                        "forward": self.forward_spec(bridge_id, params["redirect"], params["forward"], token)}
            return {"status": "answered", "replayed": True}
        if record.get("status") != "pending":
            raise ValueError("Request is no longer pending")
        if decision == "accept" and not local and not remote:
            raise ValueError(REMOTE_MESSAGE)
        with self.lock:
            if bridge_id in self.answering:
                raise ValueError("Studio is already answering this request")
            self.answering.add(bridge_id)
        try:
            if decision == "decline":
                try:
                    self.guest().call("browser.answer", {"id": bridge_id, "decision": "decline"},
                                      request_id="browser-answer:" + bridge_id + ":decline", timeout=15)
                except (LinuxVMError, OSError):
                    pass  # The guest request ends at its own deadline.
                self.finish(key, "decline", event=(
                    f"The person declined to open {params['host']} in the browser. The sign-in command "
                    "keeps waiting until its own timeout. Stop it, or ask the person before you run it again."))
                return {"status": "answered"}
            return self.approve(key, params, bridge_id, url, remote=remote)
        finally:
            with self.lock:
                self.answering.discard(bridge_id)

    def forward_spec(self, bridge_id: str, redirect: dict[str, Any], forward: dict[str, Any],
                     token: str) -> dict[str, Any]:
        return {"server": self.server_id(), "bridgeId": bridge_id, "host": redirect["host"],
                "port": redirect["port"], "path": redirect["path"], "expiresAt": forward["expiresAt"],
                "token": token}

    def approve(self, key: str, params: dict[str, Any], bridge_id: str, url: str, *,
                remote: bool = False) -> dict[str, Any]:
        redirect = params.get("redirect")
        forward = None
        # The tool of a native chat listens on this Mac, so a window here reaches it directly.
        # A window on another Mac binds the port on that Mac (open_remote_forward).
        if isinstance(redirect, dict) and not remote and self.source == "vm":
            port = int(redirect["port"])
            with self.lock:
                busy = any(other.port == port for other in self.forwards.values())
            if busy:
                raise ValueError(f"Another sign-in uses port {port} on this Mac. Wait until it ends, "
                                 "then approve again.")
            forward = Forward(bridge_id, redirect, lambda target, headers: self.relay(bridge_id, target, headers),
                              self.forward_closed, seconds=self.forward_seconds,
                              listen_port=self.listen_port(port) if self.listen_port else None)
            try:
                forward.bind()
            except PortBusy as error:
                raise ValueError(str(error)) from None
        try:
            # A lost reply keeps the card pending. The same request ID replays the guest receipt.
            answered = self.guest().call("browser.answer", {"id": bridge_id, "decision": "approve"},
                                         request_id="browser-answer:" + bridge_id + ":approve", timeout=15)
        except LinuxVMError as error:
            if forward is not None:
                forward.closed_callback = lambda *_: None
                forward.close("not approved")
            if error.code == "stale":
                self.finish(key, "expired")
                raise ValueError("The sign-in command already stopped. Ask the agent to run it again.") from None
            raise ValueError("Studio could not reach the VM. Try again.") from None
        if forward is not None:
            with self.lock:
                self.forwards[bridge_id] = forward
            forward.start()
        if remote and isinstance(redirect, dict):
            lease = (answered or {}).get("lease") if isinstance(answered, dict) else None
            expires = lease.get("expiresAt") if isinstance(lease, dict) else None
            if type(expires) not in {int, float}:
                raise ValueError("Studio could not reach the VM. Try again.")
            token = secrets.token_urlsafe(32)
            remote_forward = {"tokenHash": token_hash(token), "expiresAt": expires}
            with self.lock:
                self.tokens[bridge_id] = token
            self.finish(key, "accept", forward=remote_forward)
            return {"status": "answered", "openUrl": url,
                    "forward": self.forward_spec(bridge_id, redirect, remote_forward, token)}
        self.finish(key, "accept")
        return {"status": "answered", "openUrl": url}

    def remote_callback(self, payload: dict[str, Any]) -> dict[str, Any]:
        """Relay one redirect that a window's server received for a remote approval."""
        bridge_id = str(payload.get("bridgeId") or "")
        token = payload.get("token")
        forward = (self.record(card_id(bridge_id)).get("params") or {}).get("forward") or {}
        expected = forward.get("tokenHash")
        if not isinstance(token, str) or not isinstance(expected, str) \
                or not hmac.compare_digest(token_hash(token), expected):
            raise PermissionError("The sign-in forward token is not valid")
        headers = payload.get("headers")
        try:
            result: dict[str, Any] = self.relay(bridge_id, str(payload.get("target") or ""),
                                                headers if isinstance(headers, dict) else {})
        except LinuxVMError as error:
            if error.code in DEFINITE_CODES:
                raise ValueError(str(error)) from None
            raise
        return result


def bridge(runtime: Any) -> Bridge:
    """The bridge of VM chats: the guest service of the Linux VM on this Mac."""
    with runtime.lock:
        value = runtime.__dict__.get("_browser_bridge")
        if not isinstance(value, Bridge):
            from codex_linux_workspaces import client

            def guest() -> Guest:
                result: Guest = client(runtime)
                return result
            value = runtime._browser_bridge = Bridge(runtime, guest)
        return value


def host_bridge(runtime: Any) -> Bridge:
    """The bridge of native chats: a BrowserBridge in this process (codex_browser_host.py)."""
    with runtime.lock:
        value = runtime.__dict__.get("_browser_host_bridge")
        if not isinstance(value, Bridge):
            from codex_browser_host import HostGuest
            guest = HostGuest(runtime)
            value = runtime._browser_host_bridge = Bridge(runtime, lambda: guest, source="host")
            value.host_guest = guest  # type: ignore[attr-defined]
        return value


def tick(runtime: Any) -> None:
    """Keep the polls running: native chats on every server, VM chats on a Mac with the VM."""
    if getattr(runtime, "closed", False):
        return
    try:
        host_bridge(runtime).start()
    except (OSError, RuntimeError, ValueError) as error:
        if not runtime.__dict__.get("_browser_host_failed"):
            runtime._browser_host_failed = True
            print("browser bridge for native chats: " + type(error).__name__ + ": " + str(error)[:200],
                  file=sys.stderr)
    if platform.system() == "Darwin":
        bridge(runtime).start()


def owner(runtime: Any, key: str) -> Bridge:
    """The bridge of a card, by the source that made it."""
    with runtime.lock, runtime.db() as db:
        row = db.execute("SELECT record FROM runtime_requests WHERE id=?", (key,)).fetchone()
    source = (json.loads(row[0]).get("params") or {}).get("source") if row else None
    return host_bridge(runtime) if source == "host" else bridge(runtime)


def answer(runtime: Any, key: str, data: dict[str, Any], *, local: bool) -> dict[str, Any]:
    return owner(runtime, key).answer(key, data, local=local)


def remote_callback(runtime: Any, payload: dict[str, Any]) -> dict[str, Any]:
    """The paired action browser_callback: one redirect from the server of a remote window."""
    bridge_id = payload.get("bridgeId")
    if not isinstance(bridge_id, str) or not re.fullmatch(r"[0-9a-f]{32}", bridge_id):
        raise ValueError("Supply a sign-in request")
    return owner(runtime, card_id(bridge_id)).remote_callback(payload)


def open_remote_forward(runtime: Any, data: dict[str, Any],
                        exchange: Callable[[str, str, dict[str, Any]], Any] | None = None, *,
                        listen_port: int | None = None) -> dict[str, Any]:
    """Hold the loopback port of a sign-in that a paired server approved for this window, and
    relay its redirect to that server. Only a window on this Mac asks; the port is bound
    strictly, as for a VM chat, and each relay goes once (Forward)."""
    server, bridge_id, token = data.get("server"), data.get("bridgeId"), data.get("token")
    host, port, path, expires = data.get("host"), data.get("port"), data.get("path"), data.get("expiresAt")
    if not isinstance(server, str) or not server or not isinstance(bridge_id, str) \
            or not re.fullmatch(r"[0-9a-f]{32}", bridge_id) or not isinstance(token, str) or not token:
        raise ValueError("The sign-in forward is invalid")
    if host not in {"127.0.0.1", "localhost", "::1"} or type(port) is not int or not 1024 <= port <= 65535 \
            or not isinstance(path, str) or not path.startswith("/") or len(path) > 2048 \
            or not isinstance(expires, (int, float)) or isinstance(expires, bool):
        raise ValueError("The sign-in forward is invalid")
    seconds = min(FORWARD_SECONDS, float(expires) - time.time())
    if seconds <= 0:
        raise ValueError("The sign-in request is no longer open. Ask the agent to run the command again.")
    if exchange is None:
        exchange = paired_exchange(runtime)
    with runtime.lock:
        forwards: dict[str, Forward] = runtime.__dict__.setdefault("_browser_remote_forwards", {})
        current = forwards.get(bridge_id)
        if current is not None and not current.closed:
            return {"status": "forwarding", "replayed": True}
        if any(other.port == port and not other.closed for other in forwards.values()):
            raise ValueError(f"Another sign-in uses port {port} on this Mac. Wait until it ends, then approve again.")

        def relay(target: str, headers: dict[str, str]) -> Any:
            return exchange(server, REMOTE_ACTION, {"bridgeId": bridge_id, "token": token,
                                                    "target": target, "headers": headers})

        def closed(forward: Forward, reason: str) -> None:
            with runtime.lock:
                if forwards.get(bridge_id) is forward:
                    del forwards[bridge_id]
        forward = Forward(bridge_id, {"host": host, "port": port, "path": path}, relay, closed, seconds=seconds,
                          listen_port=listen_port)
        try:
            forward.bind()
        except PortBusy as error:
            raise ValueError(str(error)) from None
        forwards[bridge_id] = forward
    forward.start()
    return {"status": "forwarding"}


def paired_exchange(runtime: Any) -> Callable[[str, str, dict[str, Any]], Any]:
    service = runtime.multi_server()

    def exchange(server: str, action: str, payload: dict[str, Any]) -> Any:
        if server not in {row["id"] for row in service.transport.servers()}:
            raise ValueError("The sign-in belongs to a server that is not paired")
        # A new identity for each redirect: a lost reply is never sent again.
        key = "browser-callback:" + uuid.uuid4().hex
        with runtime.db() as db:
            service.queue(db, server, action, payload, key)
        receipt = service.deliver(key)
        if receipt.get("outcome") == "applied":
            return receipt["value"]
        if receipt.get("outcome") == "not_applied":
            raise ValueError(str(receipt.get("error") or "The server refused the sign-in redirect"))
        raise RuntimeError("The sign-in redirect outcome is unknown")
    return exchange


def stop(runtime: Any) -> None:
    for name in ("_browser_bridge", "_browser_host_bridge"):
        value = runtime.__dict__.get(name)
        if isinstance(value, Bridge):
            value.stop()
            guest = getattr(value, "host_guest", None)
            if guest is not None:
                guest.close()
    for forward in list(runtime.__dict__.get("_browser_remote_forwards", {}).values()):
        forward.close("shutdown")
