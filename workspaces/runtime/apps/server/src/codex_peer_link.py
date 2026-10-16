"""The QUIC link to paired servers (docs/peer-link.md).

The backend runs one `studio-peer-link` process. It listens on the Tailscale address of this
Mac, and the backend sends its requests to paired servers through its Unix socket. A signed
response advertises the link address in `X-Studio-Link`. A server without a link, or a link
that cannot connect before it sends the request, gets the request over HTTPS.
"""
from __future__ import annotations

import ipaddress
import json
import os
from pathlib import Path
import selectors
import shutil
import socket
import struct
import subprocess
import threading
import time
from collections.abc import Callable
from typing import TYPE_CHECKING, Any, cast

if TYPE_CHECKING:
    from codex_multi_server import MultiServerService

LINK_HEADER = "X-Studio-Link"
LINK_PORT = 4621
REFRESH_SECONDS = 30
START_SECONDS = 5
# A route that failed before the request left this server waits this long before the next try.
DOWN_SECONDS = 60
MAX_REPLY_LINE = 64 * 1024
SOCKET_NAME = "peer-link.sock"


class LinkFailure(Exception):
    """The link did not deliver the request (`sent` false) or lost its response (`sent` true)."""

    def __init__(self, reason: str, *, sent: bool) -> None:
        super().__init__(reason)
        self.reason = reason
        self.sent = sent


def binary() -> Path | None:
    """STUDIO_PEER_LINK_BIN, else the app's own (Resources/workspace/bin), else the cargo build."""
    configured = os.environ.get("STUDIO_PEER_LINK_BIN")
    candidates = [Path(configured)] if configured else []
    here = Path(__file__).resolve().parent
    candidates.append(here.parent / "bin" / "studio-peer-link")
    candidates.extend(parent / "target" / "release" / "studio-peer-link" for parent in here.parents)
    found = shutil.which("studio-peer-link")
    if found:
        candidates.append(Path(found))
    return next((candidate for candidate in candidates if candidate.is_file() and os.access(candidate, os.X_OK)), None)


def tailnet(address: str) -> bool:
    try:
        value = ipaddress.ip_address(address)
    except ValueError:
        return False
    return value in ipaddress.ip_network("100.64.0.0/10") or value in ipaddress.ip_network("fd7a:115c:a1e0::/48")


def parse_advert(value: object) -> tuple[str, int] | None:
    """`ADDRESS:PORT` on a Tailscale IPv4 address, else None."""
    if not isinstance(value, str) or len(value) > 64:
        return None
    host, _, port = value.rpartition(":")
    if not tailnet(host) or ":" in host or not port.isdigit() or not 0 < int(port) < 65536:
        return None
    return host, int(port)


class _Reader:
    """Buffered reads from the link socket: the reply line, then the body chunks."""

    def __init__(self, connection: socket.socket) -> None:
        self.connection = connection
        self.buffer = bytearray()

    def _fill(self) -> None:
        chunk = self.connection.recv(1 << 20)
        if not chunk:
            raise OSError("The link closed the response")
        self.buffer.extend(chunk)

    def line(self) -> bytes:
        while b"\n" not in self.buffer:
            if len(self.buffer) > MAX_REPLY_LINE:
                raise OSError("The link reply is invalid")
            self._fill()
        end = self.buffer.index(b"\n") + 1
        value = bytes(self.buffer[:end])
        del self.buffer[:end]
        return value

    def exact(self, count: int) -> bytes:
        while len(self.buffer) < count:
            self._fill()
        value = bytes(self.buffer[:count])
        del self.buffer[:count]
        return value


class PeerLink:
    def __init__(self, service: MultiServerService, backend_port: int) -> None:
        self.service = service
        self.backend_port = backend_port
        self.lock = threading.Lock()
        self.process: subprocess.Popen[bytes] | None = None
        self.listen: str | None = None
        self.peers: list[str] | None = None
        self.routes: dict[str, tuple[str, int]] = {}
        self.down: dict[str, float] = {}
        self.stop = threading.Event()
        self.thread: threading.Thread | None = None

    @property
    def socket_path(self) -> Path:
        return self.service.runtime.root / "multi-server" / SOCKET_NAME

    def start(self) -> None:
        self.thread = threading.Thread(target=self._loop, name="studio-peer-link", daemon=True)
        self.thread.start()

    def _loop(self) -> None:
        while not self.stop.is_set():
            try:
                self.ensure()
            except (OSError, RuntimeError, ValueError, subprocess.SubprocessError):
                self.close_process()
            self.stop.wait(REFRESH_SECONDS)

    def close(self) -> None:
        self.stop.set()
        self.close_process()

    def close_process(self) -> None:
        with self.lock:
            process, self.process, self.listen, self.peers = self.process, None, None, None
        if process is None:
            return
        if process.stdin is not None:
            try:
                process.stdin.close()
            except OSError:
                pass
        try:
            process.wait(timeout=3)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=3)
        if process.stdout is not None:
            process.stdout.close()

    def _paired_keys(self) -> list[str]:
        with self.service.runtime.read_db() as db:
            rows = db.execute("""SELECT json_extract(record,'$.publicKey') FROM runtime_access_clients
                WHERE json_extract(record,'$.kind')='server' AND json_extract(record,'$.status')='paired'""").fetchall()
        return sorted(str(row[0]) for row in rows if row[0])

    def _tailnet_ipv4(self) -> str | None:
        from codex_multi_server import _tailscale_json

        status = _tailscale_json("status", "--json")
        node = status.get("Self")
        addresses = node.get("TailscaleIPs") if isinstance(node, dict) else None
        if status.get("BackendState") != "Running" or not isinstance(addresses, list):
            return None
        return next((value for value in addresses if isinstance(value, str) and ":" not in value and tailnet(value)), None)

    def ensure(self) -> None:
        """Start the process when Serve is on; send the paired keys when they change."""
        origin = self.service.public_origin
        if not origin:
            self.close_process()
            return
        keys = self._paired_keys()
        with self.lock:
            process = self.process
            if process is not None and process.poll() is None:
                if keys != self.peers and process.stdin is not None:
                    process.stdin.write(json.dumps({"peers": keys}).encode() + b"\n")
                    process.stdin.flush()
                    self.peers = keys
                return
        self.close_process()
        executable = binary()
        address = self._tailnet_ipv4()
        if executable is None or address is None:
            return
        from urllib.parse import urlsplit

        identity = self.service._keys()
        for port in (LINK_PORT, 0):
            started = self._spawn(executable, {
                "listen": f"{address}:{port}", "privateKey": identity["privateKey"], "publicKey": identity["publicKey"],
                "peers": keys, "backendPort": self.backend_port, "forwardedHost": urlsplit(origin).netloc,
                "socket": str(self.socket_path)})
            if started:
                return

    def _spawn(self, executable: Path, config: dict[str, Any]) -> bool:
        # The private key goes through the pipe, never through the arguments or the environment.
        process = subprocess.Popen([str(executable)], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                   stderr=subprocess.DEVNULL)
        assert process.stdin is not None and process.stdout is not None
        try:
            process.stdin.write(json.dumps(config).encode() + b"\n")
            process.stdin.flush()
            with selectors.DefaultSelector() as selector:
                selector.register(process.stdout, selectors.EVENT_READ)
                ready = selector.select(START_SECONDS)
            line = process.stdout.readline(4096) if ready else b""
            value = json.loads(line) if line else {}
            port = value.get("port") if isinstance(value, dict) else None
            if value.get("ready") is not True or not isinstance(port, int):
                raise OSError("The link did not start")
        except (OSError, ValueError):
            process.kill()
            process.wait(timeout=3)
            process.stdin.close()
            process.stdout.close()
            return False
        host = str(config["listen"]).rpartition(":")[0]
        with self.lock:
            self.process, self.listen, self.peers = process, f"{host}:{port}", list(config["peers"])
        return True

    def advert(self) -> str | None:
        with self.lock:
            return self.listen if self.process is not None and self.process.poll() is None else None

    def learn(self, server_id: str, advert: object) -> None:
        route = parse_advert(advert)
        with self.lock:
            if route is None:
                self.routes.pop(server_id, None)
            elif self.routes.get(server_id) != route:
                self.routes[server_id] = route
                self.down.pop(server_id, None)

    def route(self, server_id: str) -> tuple[str, int] | None:
        with self.lock:
            if self.process is None or self.down.get(server_id, 0) > time.monotonic():
                return None
            return self.routes.get(server_id)

    def exchange(self, server_id: str, route: tuple[str, int], public_key: str, method: str, path: str,
                 headers: dict[str, str], raw: bytes, timeout: float, max_response: int,
                 sink: Callable[[bytes], None] | None = None) -> dict[str, Any]:
        """One request over the link: `{status, headers, raw}`, or LinkFailure. With `sink`, the
        body of a 200 response goes to it in parts, and `raw` stays empty."""
        head = {"address": f"{route[0]}:{route[1]}", "publicKey": public_key, "method": method, "path": path,
                "headers": [[name, value] for name, value in headers.items()], "bodyLength": len(raw),
                "timeout": timeout}
        connection = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        connection.settimeout(timeout + 5)
        try:
            try:
                connection.connect(str(self.socket_path))
                # The link forwards only a complete body, so a failed send did not reach the server.
                connection.sendall(json.dumps(head).encode() + b"\n" + raw)
            except OSError as error:
                raise LinkFailure(str(error), sent=False) from None
            reader = _Reader(connection)
            try:
                value = json.loads(reader.line())
                if not isinstance(value, dict):
                    raise ValueError
            except (OSError, ValueError):
                raise LinkFailure("The link reply is lost or invalid", sent=True) from None
            if "failure" in value:
                if value.get("sent") is False:
                    with self.lock:
                        self.down[server_id] = time.monotonic() + DOWN_SECONDS
                raise LinkFailure(str(value["failure"]), sent=value.get("sent") is not False)
            status, response_headers = value.get("status"), value.get("headers")
            if not isinstance(status, int) or not isinstance(response_headers, list):
                raise LinkFailure("The link reply is invalid", sent=True)
            if status != 200:
                sink = None
            body = bytearray()
            received = 0
            try:
                while True:
                    (length,) = struct.unpack(">I", reader.exact(4))
                    if length == 0:
                        break
                    received += length
                    if received > max_response:
                        raise LinkFailure("response_size", sent=True)
                    if sink is None:
                        body.extend(reader.exact(length))
                    else:
                        sink(reader.exact(length))
            except OSError:
                raise LinkFailure("The link lost the response", sent=True) from None
            return {"status": status, "headers": cast(list[list[str]], response_headers), "raw": bytes(body)}
        finally:
            connection.close()


def start(runtime: Any, backend_port: int) -> PeerLink:
    service = cast("MultiServerService", runtime.paired_access())
    link = PeerLink(service, backend_port)
    service.link = link
    link.start()
    return link
