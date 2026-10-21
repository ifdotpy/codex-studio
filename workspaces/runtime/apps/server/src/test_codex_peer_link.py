"""Two studio-peer-link processes on the loopback address, each with a fake backend."""
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import shutil
import tempfile
import threading
import time
from types import SimpleNamespace
from typing import Any
import unittest

import codex_peer_link
from codex_multi_server_crypto import CryptoProcess

BIG = bytes(range(256)) * (12 * 1024)  # 3 MiB


class Backend(BaseHTTPRequestHandler):
    seen: list[dict[str, Any]] = []

    def log_message(self, *_: object) -> None:
        pass

    def _answer(self) -> None:
        body = self.rfile.read(int(self.headers.get("Content-Length") or 0))
        Backend.seen.append({"method": self.command, "path": self.path, "version": self.request_version,
                             "headers": {name.lower(): value for name, value in self.headers.items()}, "body": body})
        data = BIG if self.path == "/api/big" else json.dumps({"ok": True, "length": len(body)}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/octet-stream" if self.path == "/api/big" else "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    do_GET = do_POST = _answer


class PeerLinkFixture(unittest.TestCase):
    """Three peer link processes and a backend; it holds no tests."""

    def setUp(self) -> None:
        executable = codex_peer_link.binary()
        if executable is None:
            self.skipTest("Build studio-peer-link first: cargo build --release -p studio-peer-link")
        self.executable = executable
        # A short folder: a Unix socket path has at most 104 bytes on macOS.
        self.folder = Path(tempfile.mkdtemp(prefix="pl", dir="/tmp"))
        self.addCleanup(shutil.rmtree, self.folder, True)
        crypto = CryptoProcess()
        self.addCleanup(crypto.shutdown)
        self.keys = {name: crypto.call("generate") for name in ("a", "b", "c")}
        Backend.seen = []
        self.backend = ThreadingHTTPServer(("127.0.0.1", 0), Backend)
        threading.Thread(target=self.backend.serve_forever, daemon=True).start()
        self.addCleanup(self.backend.server_close)
        self.addCleanup(self.backend.shutdown)
        self.links = {name: self._link(name, peers) for name, peers in (("a", ["b"]), ("b", ["a"]), ("c", ["b"]))}

    def _link(self, name: str, peers: list[str]) -> codex_peer_link.PeerLink:
        root = self.folder / name
        (root / "multi-server").mkdir(parents=True)
        link = codex_peer_link.PeerLink(SimpleNamespace(runtime=SimpleNamespace(root=root)), self.backend.server_port)  # type: ignore[arg-type]
        started = link._spawn(self.executable, {
            "listen": "127.0.0.1:0", "privateKey": self.keys[name]["privateKey"], "publicKey": self.keys[name]["publicKey"],
            "peers": [self.keys[peer]["publicKey"] for peer in peers], "backendPort": self.backend.server_port,
            "forwardedHost": f"{name}.example.ts.net", "socket": str(link.socket_path)})
        self.assertTrue(started)
        self.addCleanup(link.close)
        return link

    def _route(self, name: str) -> tuple[str, int]:
        advert = self.links[name].advert()
        assert advert is not None
        host, _, port = advert.rpartition(":")
        return host, int(port)

    def _send(self, source: str, target: str, path: str = "/api/servers/orchestration", *, key: str | None = None,
              method: str = "POST", headers: dict[str, str] | None = None, raw: bytes = b'{"a":1}') -> dict[str, Any]:
        return self.links[source].exchange(target, self._route(target), self.keys[key or target]["publicKey"], method, path,
                                           headers or {}, raw, 10, 8 * 1024 * 1024)


class PeerLinkProcesses(PeerLinkFixture):
    def test_a_request_reaches_the_backend_as_a_serve_request(self) -> None:
        response = self._send("a", "b", headers={"X-Studio-Signature": "sig", "X-Forwarded-For": "100.64.0.9",
                                                 "Host": "evil", "Tailscale-User-Login": "someone"})
        self.assertEqual(response["status"], 200)
        self.assertEqual(json.loads(response["raw"]), {"ok": True, "length": 7})
        seen = Backend.seen[-1]
        self.assertEqual((seen["method"], seen["path"], seen["body"]), ("POST", "/api/servers/orchestration", b'{"a":1}'))
        # The link sets the Serve headers itself; the peer cannot supply them.
        self.assertEqual(seen["headers"]["x-forwarded-for"], "127.0.0.1")
        self.assertEqual(seen["headers"]["x-forwarded-host"], "b.example.ts.net")
        self.assertEqual(seen["headers"]["x-forwarded-proto"], "https")
        self.assertEqual(seen["headers"]["host"], f"127.0.0.1:{self.backend.server_port}")
        self.assertNotIn("tailscale-user-login", seen["headers"])
        self.assertEqual(seen["headers"]["x-studio-signature"], "sig")

    def test_a_large_response_arrives_whole(self) -> None:
        # The exchange keeps its own 10 s deadline, so a stalled stream still fails here.
        # The wall-clock transfer bound is in test_codex_peer_link_realtime.py (heavy tier).
        response = self._send("a", "b", "/api/big", method="GET", raw=b"")
        self.assertEqual(response["raw"], BIG)

    def test_an_export_offset_query_reaches_the_backend(self) -> None:
        # The bundle fetch and the chat move continue an export from an offset (docs/peer-link.md).
        path = '/api/servers/exports/0e3c1a5e-7b1d-4f43-9a6e-2c1f0b9d8e77.bundle?offset=131072'
        self.assertEqual(self._send('a', 'b', path, method='GET', raw=b'')['status'], 200)
        self.assertEqual((Backend.seen[-1]['method'], Backend.seen[-1]['path']), ('GET', path))

    def test_the_response_limit_is_kept(self) -> None:
        with self.assertRaises(codex_peer_link.LinkFailure) as caught:
            self.links["a"].exchange("b", self._route("b"), self.keys["b"]["publicKey"], "GET", "/api/big", {}, b"", 10, 1024)
        self.assertEqual((caught.exception.reason, caught.exception.sent), ("response_size", True))

    def test_an_unpaired_client_is_refused_before_its_request(self) -> None:
        with self.assertRaises(codex_peer_link.LinkFailure) as caught:
            self._send("c", "b")
        self.assertFalse(caught.exception.sent)
        self.assertEqual(Backend.seen, [])

    def test_a_server_with_another_key_is_refused(self) -> None:
        # Link a dials b's address but expects c's key: the TLS handshake fails.
        self.links["a"].process.stdin.write(json.dumps({"peers": [self.keys["c"]["publicKey"]]}).encode() + b"\n")  # type: ignore[union-attr]
        self.links["a"].process.stdin.flush()  # type: ignore[union-attr]
        time.sleep(0.2)
        with self.assertRaises(codex_peer_link.LinkFailure) as caught:
            self._send("a", "b", key="c")
        self.assertFalse(caught.exception.sent)
        self.assertEqual(Backend.seen, [])

    def test_a_revoked_key_closes_the_connection(self) -> None:
        self.assertEqual(self._send("a", "b")["status"], 200)
        process = self.links["b"].process
        assert process is not None and process.stdin is not None
        process.stdin.write(b'{"peers":[]}\n')
        process.stdin.flush()
        time.sleep(0.3)
        with self.assertRaises(codex_peer_link.LinkFailure) as caught:
            self._send("a", "b")
        self.assertFalse(caught.exception.sent)
        self.assertEqual(len(Backend.seen), 1)

    def test_the_advert_is_a_tailnet_address(self) -> None:
        self.assertEqual(codex_peer_link.parse_advert("100.75.160.63:4621"), ("100.75.160.63", 4621))
        for value in ("127.0.0.1:4621", "192.168.1.5:4621", "100.75.160.63:0", "100.75.160.63", None, "x" * 80):
            self.assertIsNone(codex_peer_link.parse_advert(value))


if __name__ == "__main__":
    unittest.main()
