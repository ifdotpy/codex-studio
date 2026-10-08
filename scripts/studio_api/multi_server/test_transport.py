"""Real native TLS transport checks on disposable loopback ports."""
from __future__ import annotations

import base64
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import shutil
import ssl
import subprocess
import tempfile
import threading
import time
from typing import Any
import unittest
from unittest.mock import patch

from codex_multi_server import AccessError, _exchange, _owner_login, _peer_login
from studio_api.middleware import HeaderView
from studio_api.multi_server.boundary import _body


class IdentityTests(unittest.TestCase):
    def test_status_and_whois_require_the_same_real_owner(self) -> None:
        status = {"BackendState": "Running", "Self": {"UserID": 42}, "User": {"42": {"LoginName": "Owner@Example.com"}}}
        whois = {"Node": {"User": 42}, "UserProfile": {"LoginName": "owner@example.com"}}
        headers = HeaderView([(b"x-forwarded-for", b"100.64.0.12"), (b"tailscale-user-login", b"owner@example.com")])
        with patch("codex_multi_server._tailscale_json", side_effect=[status, whois]) as command:
            self.assertEqual(_owner_login(), "owner@example.com")
            self.assertEqual(_peer_login(headers), "owner@example.com")
            self.assertEqual(command.call_args_list[0].args, ("status", "--json"))
            self.assertEqual(command.call_args_list[1].args, ("whois", "--json", "100.64.0.12"))
        with patch("codex_multi_server._tailscale_json", return_value={"Node": {"Tags": ["tag:server"]}, "UserProfile": {"LoginName": "owner@example.com"}}):
            with self.assertRaises(AccessError):
                _peer_login(headers)
        with patch("codex_multi_server._tailscale_json", return_value=whois):
            with self.assertRaises(AccessError):
                _peer_login(HeaderView([(b"x-forwarded-for", b"100.64.0.12"), (b"tailscale-user-login", b"other@example.com")]))

    def test_missing_identity_and_untrusted_addresses_fail(self) -> None:
        with patch("codex_multi_server._tailscale_json", return_value={}):
            with self.assertRaises(AccessError):
                _owner_login()
            with self.assertRaises(AccessError):
                _peer_login(HeaderView([(b"x-forwarded-for", b"100.64.0.12")]))
        with patch("codex_multi_server._tailscale_json") as command:
            with self.assertRaises(AccessError):
                _peer_login(HeaderView([(b"x-forwarded-for", b"127.0.0.1")]))
            command.assert_not_called()

    def test_malformed_identity_fails_with_a_clear_error(self) -> None:
        malformed_values: tuple[dict[str, Any], ...] = ({"Self": ["wrong"], "User": {}}, {"Self": {}, "User": []})
        for malformed in malformed_values:
            with patch("codex_multi_server._tailscale_json", return_value=malformed):
                with self.assertRaises(AccessError) as unavailable:
                    _owner_login()
                self.assertEqual(unavailable.exception.code, "identity_unavailable")


class BodyLimitTests(unittest.IsolatedAsyncioTestCase):
    async def test_body_deadline_size_and_duplicate_length(self) -> None:
        import asyncio
        from starlette.types import Message, Scope

        scope: Scope = {"type": "http", "method": "POST", "path": "/api/probe"}

        async def blocked() -> Message:
            await asyncio.Event().wait()
            raise AssertionError("The body deadline must stop this read")

        with patch("studio_api.multi_server.boundary.REQUEST_READ_TIMEOUT_SECONDS", 0.01):
            with self.assertRaises(AccessError) as expired:
                await _body(scope, HeaderView([(b"content-length", b"1")]), blocked)
        self.assertEqual(expired.exception.status, 408)
        for raw_headers in ([(b"content-length", b"1"), (b"content-length", b"1")], [(b"content-length", b"262145")]):
            with self.assertRaises(AccessError):
                await _body(scope, HeaderView(raw_headers), blocked)


class NativeTransportTests(unittest.TestCase):
    def test_tls_exact_bytes_redirect_refusal_and_process_deadline(self) -> None:
        executable = shutil.which("openssl")
        self.assertIsNotNone(executable, "The TLS fixture requires OpenSSL")
        with tempfile.TemporaryDirectory(prefix="studio-access-tls-") as temporary:
            directory = Path(temporary)
            key, cert, config = directory / "key.pem", directory / "cert.pem", directory / "openssl.cnf"
            config.write_text("[req]\ndistinguished_name=dn\nx509_extensions=v3\nprompt=no\n[dn]\nCN=localhost\n[v3]\nsubjectAltName=DNS:localhost,IP:127.0.0.1\nbasicConstraints=CA:TRUE\n")
            subprocess.run([str(executable), "req", "-x509", "-nodes", "-newkey", "rsa:2048", "-days", "1",
                            "-keyout", str(key), "-out", str(cert), "-config", str(config)],
                           check=True, capture_output=True, timeout=20)
            seen: list[bytes] = []
            release = threading.Event()

            class Handler(BaseHTTPRequestHandler):
                def log_message(self, format: str, *args: Any) -> None:
                    pass

                def do_POST(self) -> None:
                    body = self.rfile.read(int(self.headers.get("Content-Length", "0")))
                    seen.append(body)
                    output = json.dumps({"body": base64.b64encode(body).decode()}).encode()
                    self.send_response(200)
                    self.send_header("Content-Length", str(len(output)))
                    self.end_headers()
                    self.wfile.write(output)

                def do_GET(self) -> None:
                    if self.path == "/redirect":
                        self.send_response(302)
                        self.send_header("Location", "/echo")
                        self.send_header("Content-Length", "0")
                        self.end_headers()
                    elif self.path == "/large":
                        output = b"x" * (2 * 1024 * 1024)
                        self.send_response(200)
                        self.send_header("Content-Length", str(len(output)))
                        self.end_headers()
                        try:
                            self.wfile.write(output)
                        except (OSError, ssl.SSLError):
                            pass
                    else:
                        release.wait(5)

            server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
            server.daemon_threads = True
            context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
            context.load_cert_chain(cert, key)
            server.socket = context.wrap_socket(server.socket, server_side=True)
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            try:
                origin = f"https://127.0.0.1:{server.server_port}"
                raw = '{"text":"Zażółć","space": 1}'.encode()
                with patch.dict(os.environ, {"NODE_EXTRA_CA_CERTS": str(cert)}):
                    result = _exchange(origin + "/echo", "POST", {"Content-Type": "application/json"}, raw, 5)
                    self.assertEqual(result["status"], 200)
                    self.assertEqual(seen, [raw])
                    self.assertEqual(json.loads(base64.b64decode(result["body"]))["body"], base64.b64encode(raw).decode())
                    redirect = _exchange(origin + "/redirect", "GET", {}, b"", 5)
                    self.assertEqual(redirect["status"], 302)
                    self.assertEqual(redirect["url"], origin + "/redirect")
                    with self.assertRaises(AccessError) as large:
                        _exchange(origin + "/large", "GET", {}, b"", 5)
                    self.assertEqual(large.exception.code, "response_size")
                    started = time.monotonic()
                    with self.assertRaises(AccessError) as expired:
                        _exchange(origin + "/slow", "GET", {}, b"", 0.3)
                    self.assertEqual(expired.exception.code, "remote_timeout")
                    self.assertLess(time.monotonic() - started, 2)
            finally:
                release.set()
                server.shutdown()
                server.server_close()
                thread.join(3)


if __name__ == "__main__":
    unittest.main()
