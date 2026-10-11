"""Fakes for the browser bridge tests: a sign-in provider and two sign-in tools.

They copy what docs/vm-browser-bridge.md measured. A kubelogin-style tool listens on a fixed
port, opens its own page `http://localhost:PORT/` with `xdg-open`, and that page redirects to
the provider with `redirect_uri=http://localhost:PORT`. An AWS-style tool listens on a random
port, opens the provider page directly with `$BROWSER`, and uses
`redirect_uri=http://127.0.0.1:PORT/oauth/callback`. Each tool posts the code to the provider
after its callback, as a real tool does. No real provider and no real browser take part.
"""
from __future__ import annotations

from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import os
from pathlib import Path
import secrets
import socket
import stat
import subprocess
import sys
import threading
from typing import Any
from urllib.parse import parse_qs, urlencode, urlsplit
import urllib.request

SHIM = Path(__file__).with_name("studio_browser.py")


class _Server(ThreadingHTTPServer):
    daemon_threads = True
    owner: Any


class _Handler(BaseHTTPRequestHandler):
    server: _Server

    def log_message(self, format: str, *args: Any) -> None:
        pass

    def send(self, status: int, body: bytes = b"", headers: tuple[tuple[str, str], ...] = ()) -> None:
        self.send_response(status)
        for name, value in headers:
            self.send_header(name, value)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self) -> None:
        self.server.owner.get(self)

    def do_POST(self) -> None:
        self.server.owner.post(self)


class FakeProvider:
    """`/authorize` sends the browser back to `redirect_uri` with a code; `/token` records it."""

    def __init__(self) -> None:
        self.server = _Server(("127.0.0.1", 0), _Handler)
        self.server.owner = self
        self.port = int(self.server.server_address[1])
        self.origin = f"127.0.0.1:{self.port}"
        self.tokens: list[dict[str, list[str]]] = []
        self.codes: list[str] = []
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def authorize_url(self, redirect_uri: str, state: str) -> str:
        return f"http://{self.origin}/authorize?" + urlencode(
            {"response_type": "code", "client_id": "fake-client", "redirect_uri": redirect_uri,
             "state": state, "scope": "openid"})

    def get(self, handler: _Handler) -> None:
        parts = urlsplit(handler.path)
        query = parse_qs(parts.query)
        if parts.path != "/authorize":
            handler.send(404)
            return
        code = "code-" + secrets.token_hex(4)
        self.codes.append(code)
        target = query["redirect_uri"][0]
        handler.send(302, headers=(("Location", target + "?" + urlencode({"code": code, "state": query["state"][0]})),))

    def post(self, handler: _Handler) -> None:
        body = handler.rfile.read(int(handler.headers.get("Content-Length") or 0)).decode()
        self.tokens.append(parse_qs(body))
        handler.send(200, b'{"access_token":"fake"}', (("Content-Type", "application/json"),))

    def close(self) -> None:
        self.server.shutdown()
        self.server.server_close()


def free_port(preferred: int | None = None) -> int:
    for port in ([preferred] if preferred else []) + [0]:
        with socket.socket() as probe:
            try:
                probe.bind(("127.0.0.1", port))
            except OSError:
                continue
            return int(probe.getsockname()[1])
    raise OSError("No free port")


class FakeTool:
    """A sign-in command: its loopback listener, its launcher call, and its token request."""

    def __init__(self, provider: FakeProvider, style: str, *, port: int | None = None) -> None:
        self.provider, self.style = provider, style
        self.state = secrets.token_urlsafe(8)
        self.server = _Server(("127.0.0.1", port or 0), _Handler)
        self.server.owner = self
        self.port = int(self.server.server_address[1])
        self.received: list[tuple[str, dict[str, str]]] = []
        self.codes: list[str] = []
        if style == "kubelogin":
            self.redirect_uri = f"http://localhost:{self.port}"
            self.callback_path = "/"
            self.open_url = f"http://localhost:{self.port}/"
        else:
            self.redirect_uri = f"http://127.0.0.1:{self.port}/oauth/callback"
            self.callback_path = "/oauth/callback"
            self.open_url = provider.authorize_url(self.redirect_uri, self.state)
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def get(self, handler: _Handler) -> None:
        parts = urlsplit(handler.path)
        query = parse_qs(parts.query)
        self.received.append((handler.path, {name.lower(): value for name, value in handler.headers.items()}))
        if self.style == "kubelogin" and parts.path == "/" and "code" not in query:
            location = self.provider.authorize_url(self.redirect_uri, self.state)
            handler.send(302, headers=(("Location", location),))
            return
        if parts.path != self.callback_path or query.get("state") != [self.state] or "code" not in query:
            handler.send(400, b"bad callback", (("Content-Type", "text/plain"),))
            return
        code = query["code"][0]
        self.codes.append(code)
        data = urlencode({"grant_type": "authorization_code", "code": code,
                          "redirect_uri": self.redirect_uri}).encode()
        urllib.request.urlopen(f"http://{self.provider.origin}/token", data=data, timeout=5).read()
        handler.send(200, b"<html><script>window.close()</script>Signed in</html>",
                     (("Content-Type", "text/html; charset=utf-8"), ("Set-Cookie", "tool=1"),
                      ("Location", "https://example.invalid/")))

    def launch(self, bin_dir: Path, environment: dict[str, str]) -> subprocess.CompletedProcess[str]:
        """Run the launcher as the tool does: `xdg-open URL` from PATH, or `$BROWSER URL`."""
        env = {**environment, "PATH": str(bin_dir) + os.pathsep + "/usr/bin:/bin"}
        if self.style == "kubelogin":
            command = ["xdg-open", self.open_url]
        else:
            env["BROWSER"] = str(bin_dir / "studio-browser")
            command = [env["BROWSER"], self.open_url]
        return subprocess.run(command, env=env, capture_output=True, text=True, timeout=30)

    def close(self) -> None:
        self.server.shutdown()
        self.server.server_close()


def install_shim(bin_dir: Path) -> None:
    """The VM's /usr/local/bin/studio-browser and its xdg-open link, in a test folder."""
    bin_dir.mkdir(parents=True, exist_ok=True)
    wrapper = bin_dir / "studio-browser"
    wrapper.write_text(f"#!/bin/sh\nexec {sys.executable} -I {SHIM} \"$@\"\n")
    wrapper.chmod(wrapper.stat().st_mode | stat.S_IXUSR)
    (bin_dir / "xdg-open").symlink_to(wrapper)
