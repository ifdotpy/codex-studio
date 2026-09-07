#!/usr/bin/env python3
"""Exercise the Tailscale proxy boundary without modifying a tailnet."""
import json
from pathlib import Path
import sys
import tempfile
import threading
import unittest
import urllib.error
import urllib.request

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
from codex_canvas import Canvas, make_server
from codex_remote import validate_origin


class MobileOriginContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="studio-mobile-origin-")
        self.canvas = Canvas(Path(self.temp.name))
        self.server = make_server(self.canvas)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.url = f"http://127.0.0.1:{self.server.server_port}"
        self.origin = "https://studio.example.ts.net"
        self.headers = {"X-Forwarded-Host": "studio.example.ts.net", "X-Forwarded-Proto": "https",
                        "X-Forwarded-For": "100.100.12.34", "Origin": self.origin}

    def tearDown(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join()
        self.temp.cleanup()

    def request(self, headers=None, body=None, path="/api/state"):
        req = urllib.request.Request(self.url + path,
            data=json.dumps(body).encode() if body is not None else None,
            headers={"Content-Type": "application/json", **(headers or {})})
        try:
            with urllib.request.urlopen(req, timeout=3) as result:
                return result.status, json.load(result)
        except urllib.error.HTTPError as result:
            with result:
                return result.code, json.load(result)

    def enable(self):
        (self.canvas.root / "remote-access.json").write_text(json.dumps({"enabled": True, "origin": self.origin}))

    def test_local_stays_available_and_remote_requires_explicit_origin(self):
        self.assertEqual(self.request()[0], 200)
        self.assertEqual(self.request(self.headers)[0], 403)
        self.enable()
        self.assertEqual(self.request(self.headers)[0], 200)
        self.assertEqual(self.request()[0], 200)

    def test_reject_wrong_host_origin_scheme_and_proxy_source(self):
        self.enable()
        for header, value in [("Host", "attacker.example"), ("Origin", "https://evil.example"),
                              ("X-Forwarded-Host", "evil.ts.net"), ("X-Forwarded-Proto", "http"),
                              ("X-Forwarded-For", "192.168.1.10"), ("X-Forwarded-For", "100.100.12.34, 1.2.3.4"),
                              ("Sec-Fetch-Site", "cross-site")]:
            with self.subTest(header=header, value=value):
                self.assertEqual(self.request({**self.headers, header: value})[0], 403)
        for missing in ("X-Forwarded-For", "X-Forwarded-Proto", "X-Forwarded-Host"):
            headers = {k: v for k, v in self.headers.items() if k != missing}
            self.assertEqual(self.request(headers)[0], 403)

    def test_remote_writes_keep_csrf_token(self):
        self.enable()
        code, state = self.request(self.headers)
        self.assertEqual(code, 200)
        body = {"id": "12345678-1234-1234-1234-123456789abc", "name": "Mobile", "members": []}
        self.assertEqual(self.request(self.headers, body, "/api/chats")[0], 403)
        self.assertEqual(self.request({**self.headers, "X-Canvas-Token": state["token"]}, body, "/api/chats")[0], 200)
        self.assertEqual(len(self.canvas.chats()), 1)

    def test_public_panel_scripts_accept_exact_proxy_host(self):
        self.enable()
        headers = {**self.headers, "Host": "studio.example.ts.net", "Origin": "null", "Sec-Fetch-Site": "cross-site"}
        req = urllib.request.Request(self.url + "/assets/panel-bridge.js", headers=headers)
        with urllib.request.urlopen(req, timeout=3) as result:
            self.assertEqual(result.status, 200)
        self.assertEqual(self.request(headers)[0], 403)

    def test_workspace_identity_guards_mutations(self):
        _, state = self.request()
        _, identity = self.request(path="/api/sync/identity")
        headers = {"X-Canvas-Token": state["token"], "X-Canvas-Workspace": "wrong"}
        self.assertEqual(self.request(headers, {"rows": []}, "/api/sync/drafts")[0], 409)
        headers["X-Canvas-Workspace"] = identity["workspaceId"]
        self.assertEqual(self.request(headers, {"rows": []}, "/api/sync/drafts"), (200, []))
        _, pull = self.request(path="/api/sync/pull?scope=state")
        self.assertEqual(pull["workspaceId"], identity["workspaceId"])
        self.assertNotIn("token", json.loads(pull["documents"][0]["payload"]))

    def test_disable_takes_effect_without_restart(self):
        self.enable()
        self.assertEqual(self.request(self.headers)[0], 200)
        (self.canvas.root / "remote-access.json").write_text('{"enabled": false}')
        self.assertEqual(self.request(self.headers)[0], 403)

    def test_config_is_exact_https_origin(self):
        for value in ("https://evil.test", "http://studio.example.ts.net", "https://studio.example.ts.net/path", "https://user@studio.example.ts.net", "https://studio.example.ts.net?x=1"):
            with self.subTest(value=value), self.assertRaises(ValueError):
                validate_origin(value)
        self.assertEqual(validate_origin(self.origin + "/"), self.origin)


if __name__ == "__main__":
    unittest.main()
