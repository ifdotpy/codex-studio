#!/usr/bin/env python3
"""Automatic discovery with real runtimes, signed routes and a PATH identity stub."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import base64
import importlib.util
import json
import os
from pathlib import Path
import secrets
import sys
import threading
import time
import unittest
from unittest.mock import patch
from urllib.parse import urlsplit
import uuid

spec = importlib.util.spec_from_file_location("discovery_fixture", Path(__file__).with_name("multi-server-signed-integration.py"))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
from codex_federation import _crypto, _sign
from codex_multi_server import request_bytes
from codex_server_discovery import AUTO_PAIR_PATH, IDENTITY_PATH


class DiscoveryContract(unittest.TestCase):
    def setUp(self):
        fixture.SignedIntegration.setUp(self)
        self.status_file = self.folder / "status.json"
        self.whois_file = self.folder / "whois.json"
        self.status = {"BackendState": "Running", "Self": {"UserID": 1},
                       "User": {"1": {"LoginName": fixture.OWNER}}, "Peer": {
                           name: {"UserID": 1, "Online": True, "DNSName": f"{name}.example.ts.net."} for name in ("a", "b")}}
        self.whois = {endpoint.address: {"Node": {"Name": endpoint.name + ".example.ts.net."},
                      "UserProfile": {"LoginName": fixture.OWNER}} for endpoint in (self.a, self.b)}
        self.save_identity()
        self.identity_overrides = {}
        self.probes = []
        self.drop_once = None
        executable = self.folder / "bin" / "tailscale"
        executable.write_text(f'''#!{sys.executable}
import json, os, sys
args = sys.argv[1:]
with open(os.environ["SIGNED_TEST_TAILSCALE_LOG"], "a") as log:
    log.write(json.dumps(args) + "\\n")
if args == ["status", "--json"]:
    value = json.load(open({str(self.status_file)!r}))
elif len(args) == 3 and args[:2] == ["whois", "--json"]:
    value = json.load(open({str(self.whois_file)!r}))[args[2]]
else:
    sys.exit("The isolated stub refuses this command")
print(json.dumps(value))
''')

    def save_identity(self):
        self.status_file.write_text(json.dumps(self.status))
        self.whois_file.write_text(json.dumps(self.whois))

    def exchange(self, url, method, headers, raw, timeout):
        parsed = urlsplit(url)
        origin = parsed.scheme + "://" + parsed.netloc
        endpoint = self.endpoints.get(origin)
        if endpoint is None:
            self.probes.append(origin)
            return {"status": 404, "url": url, "body": base64.b64encode(b'{}').decode()}
        sender = next(ep for ep in self.endpoints.values() if ep.server_id == headers.get("X-Studio-Client")) if "X-Studio-Client" in headers else next(ep for ep in self.endpoints.values() if ep is not endpoint)
        target = parsed.path + ("?" + parsed.query if parsed.query else "")
        response = endpoint.client.request(method, target, content=raw, headers={**headers,
            "X-Forwarded-Host": parsed.netloc, "X-Forwarded-Proto": "https",
            "X-Forwarded-For": sender.address, "Tailscale-User-Login": fixture.OWNER})
        value = response.json()
        if target == IDENTITY_PATH:
            self.probes.append(origin)
            value.update(self.identity_overrides.get(endpoint.name, {}))
        self.wire.append({"recipient": endpoint.name, "target": target, "headers": headers,
                          "raw": raw, "status": response.status_code})
        if self.drop_once == target and response.status_code == 200:
            self.drop_once = None
            raise TimeoutError("The route completed before its reply was lost")
        return {"status": response.status_code, "url": url, "body": base64.b64encode(json.dumps(value).encode()).decode()}

    def discover(self, endpoint=None, request_id="discover-one"):
        endpoint = endpoint or self.a
        return endpoint.local({"action": "discover", "requestId": request_id})

    def auto_request(self, sender=None, target=None, request_id="auto-one", **changes):
        sender, target = sender or self.a, target or self.b
        key = sender.runtime.paired_access()._keys()
        body = {"protocol": 1, "serverId": sender.server_id, "origin": sender.origin,
                "publicKey": key["publicKey"], "label": key["label"], "requestId": request_id, **changes}
        raw = json.dumps(body).encode()
        headers = sender.runtime.paired_access().signed_headers(target.server_id, "POST", AUTO_PAIR_PATH, raw, request_id)
        return target.client.post(AUTO_PAIR_PATH, content=raw, headers={**headers,
            "X-Forwarded-Host": urlsplit(target.origin).netloc, "X-Forwarded-Proto": "https",
            "X-Forwarded-For": sender.address, "Tailscale-User-Login": fixture.OWNER, "Content-Type": "application/json"})

    def paired(self, endpoint):
        return [peer for peer in endpoint.runtime.paired_access().servers() if peer["status"] == "paired"]

    def test_discovery_filters_and_real_mutual_pairing(self):
        self.status["Peer"].update({
            "other": {"UserID": 2, "Online": True, "DNSName": "other.example.ts.net."},
            "tagged": {"UserID": 1, "Online": True, "Tags": ["tag:server"], "DNSName": "tagged.example.ts.net."},
            "offline": {"UserID": 1, "Online": False, "DNSName": "offline.example.ts.net."},
            "missing": {"UserID": 1, "Online": True},
            "non-studio": {"UserID": 1, "Online": True, "DNSName": "unknown.example.ts.net."},
        })
        self.save_identity()
        result = self.discover()
        self.assertTrue(result["settings"]["autoPair"])
        self.assertEqual(self.paired(self.a)[0]["serverId"], self.b.server_id)
        self.assertEqual(self.paired(self.b)[0]["serverId"], self.a.server_id)
        self.assertIsInstance(result["servers"][0]["lastSeen"], float)
        self.assertIn("https://unknown.example.ts.net", self.probes)
        for name in ("other", "tagged", "offline"):
            self.assertNotIn(f"https://{name}.example.ts.net", self.probes)
        for endpoint in (self.a, self.b):
            with endpoint.runtime.read_db() as db:
                self.assertEqual(db.execute("SELECT count(*) FROM runtime_access_audit WHERE action='auto_pair'").fetchone()[0], 1)
        self.assertEqual(self.auto_request().status_code, 200)
        self.assertEqual(self.auto_request().status_code, 200)
        self.discover(request_id="discover-two")
        for endpoint in (self.a, self.b):
            with endpoint.runtime.read_db() as db:
                self.assertEqual(db.execute("SELECT count(*) FROM runtime_access_clients").fetchone()[0], 1)
                self.assertEqual(db.execute("SELECT count(*) FROM runtime_access_audit WHERE action='auto_pair'").fetchone()[0], 1)
        self.a.restart()
        self.discover(request_id="after-restart")
        self.assertEqual(len(self.paired(self.a)), 1)

    def test_lost_auto_pair_reply_recovers_same_proof_after_restart(self):
        self.drop_once = AUTO_PAIR_PATH
        first = self.discover()
        self.assertEqual(self.paired(self.a), [])
        self.assertEqual(self.paired(self.b)[0]["serverId"], self.a.server_id)
        self.assertEqual(first["servers"][0]["status"], "discovered")
        self.a.restart()
        self.discover(request_id="recover-auto")
        self.assertEqual(self.paired(self.a)[0]["serverId"], self.b.server_id)
        attempts = [row for row in self.wire if row["target"] == AUTO_PAIR_PATH]
        self.assertEqual(len(attempts), 2)
        self.assertEqual(attempts[0]["raw"], attempts[1]["raw"])
        self.assertEqual(attempts[0]["headers"]["X-Studio-Request-Id"], attempts[1]["headers"]["X-Studio-Request-Id"])
        self.assertNotEqual(attempts[0]["headers"]["X-Studio-Nonce"], attempts[1]["headers"]["X-Studio-Nonce"])
        for endpoint in (self.a, self.b):
            with endpoint.runtime.read_db() as db:
                self.assertEqual(db.execute("SELECT count(*) FROM runtime_access_audit WHERE action='auto_pair'").fetchone()[0], 1)

    def test_background_discovery_starts_once_and_repeats_until_close(self):
        service = self.a.runtime.paired_access().discovery()
        seen = threading.Event()
        count = 0
        def discover():
            nonlocal count
            count += 1
            if count >= 2:
                seen.set()
            return {}
        with patch.object(service, "discover", side_effect=discover), patch("codex_server_discovery.INTERVAL", 0.01):
            service.start()
            first = service.thread
            service.start()
            self.assertIs(first, service.thread)
            self.assertTrue(seen.wait(2))
            service.close()
            count_at_close = count
            time.sleep(0.03)
            self.assertEqual(count, count_at_close)
            self.assertFalse(service.thread.is_alive())

    def test_owner_origin_key_and_disabled_proofs_are_refused(self):
        cases = ("owner", "tagged", "origin", "key", "server-id", "caller-disabled", "target-disabled")
        for case in cases:
            with self.subTest(case=case):
                original = json.loads(json.dumps(self.whois))
                self.identity_overrides.clear()
                if case == "owner":
                    self.whois[self.a.address]["UserProfile"]["LoginName"] = "other@example.test"
                if case == "tagged":
                    self.whois[self.a.address]["Node"]["Tags"] = ["tag:service"]
                if case == "server-id":
                    self.identity_overrides["a"] = {"serverId": str(uuid.uuid4())}
                if case == "origin":
                    self.whois[self.a.address]["Node"]["Name"] = "wrong.example.ts.net."
                if case == "key":
                    self.identity_overrides["a"] = {"publicKey": _crypto("generate")["publicKey"]}
                if case == "caller-disabled":
                    self.a.local({"action": "settings", "autoPair": False})
                if case == "target-disabled":
                    self.b.local({"action": "settings", "autoPair": False})
                self.save_identity()
                response = self.auto_request(request_id="refuse-" + case)
                self.assertEqual(response.status_code, 403, response.text)
                self.assertEqual(self.paired(self.b), [])
                with self.b.runtime.read_db() as db:
                    self.assertEqual(db.execute("SELECT count(*) FROM runtime_access_receipts").fetchone()[0], 0)
                self.whois = original
                self.save_identity()
                self.a.local({"action": "settings", "autoPair": True})
                self.b.local({"action": "settings", "autoPair": True})

    def test_revocation_wins_and_explicit_unrevoke_requires_new_proof(self):
        self.discover()
        created = self.paired(self.b)[0]["created"]
        self.b.local({"action": "revoke", "clientId": self.a.server_id, "requestId": "revoke-a"})
        self.assertEqual(self.auto_request().status_code, 403)
        self.discover(self.b)
        self.assertEqual(self.b.runtime.paired_access().servers()[0]["status"], "revoked")
        self.b.local({"action": "unrevoke", "clientId": self.a.server_id, "requestId": "unrevoke-a"})
        self.assertEqual(self.paired(self.b), [])
        self.b.local({"action": "unrevoke", "clientId": self.a.server_id, "requestId": "unrevoke-a"})
        self.discover(self.b, "after-unrevoke")
        self.assertEqual(self.paired(self.b)[0]["serverId"], self.a.server_id)
        self.assertGreater(self.paired(self.b)[0]["created"], created)

    def test_ui_invitation_retry_has_no_secret_in_receipts_and_pairs_a_browser(self):
        self.discover()
        request = {"action": "ui_invite", "serverId": self.b.server_id, "requestId": "ui-b"}
        self.drop_once = "/api/multi-server"
        failed = self.a.client.post("/api/multi-server", json=request, headers={"X-Canvas-Token": "test-token"})
        self.assertEqual(failed.status_code, 503)
        first = self.a.local(request)
        self.assertEqual(first, self.a.local(request))
        invitation = first["invitation"]
        self.assertEqual(invitation["serverId"], self.b.server_id)
        for endpoint in (self.a, self.b):
            with endpoint.runtime.read_db() as db:
                for table in ("runtime_access_receipts", "runtime_access_outbound", "runtime_access_audit"):
                    records = db.execute("SELECT * FROM " + table).fetchall()
                    self.assertFalse(any(invitation["token"] in str(tuple(row)) for row in records), table)
                    if table == "runtime_access_receipts":
                        for row in records:
                            saved = json.loads(row["record"])
                            if "body" in saved:
                                self.assertNotIn(invitation["token"].encode(), base64.b64decode(saved["body"]))
        keys = _crypto("generate")
        body = {"protocol": 1, "inviteId": invitation["inviteId"], "token": invitation["token"],
                "clientId": "browser-1", "label": "Browser", "kind": "ui", "publicKey": keys["publicKey"], "requestId": "browser-pair"}
        raw = json.dumps(body).encode()
        timestamp, nonce = str(int(time.time())), secrets.token_urlsafe(24)
        signature = _sign(keys["privateKey"], request_bytes("POST", "/api/multi-server/v1/pair", self.b.server_id,
                          "browser-1", timestamp, nonce, "browser-pair", raw))
        response = self.b.client.post("/api/multi-server/v1/pair", content=raw, headers={
            "X-Studio-Client": "browser-1", "X-Studio-Server": self.b.server_id, "X-Studio-Timestamp": timestamp,
            "X-Studio-Nonce": nonce, "X-Studio-Request-Id": "browser-pair", "X-Studio-Signature": signature,
            "X-Forwarded-Host": "b.example.ts.net", "X-Forwarded-Proto": "https", "X-Forwarded-For": self.a.address,
            "Tailscale-User-Login": fixture.OWNER, "Content-Type": "application/json"})
        self.assertEqual(response.status_code, 200, response.text)
        self.assertEqual(response.json()["clientId"], "browser-1")

    def test_discover_and_ui_invite_are_local_only_and_identity_is_serve_only(self):
        self.assertEqual(self.a.client.get(IDENTITY_PATH).status_code, 403)
        self.discover()
        for action in ({"action": "discover", "requestId": "remote-discover"},
                       {"action": "ui_invite", "serverId": self.b.server_id, "requestId": "remote-invite"}):
            for signed in (False, True):
                raw = json.dumps(action).encode()
                headers = self.b.runtime.paired_access().signed_headers(self.a.server_id, "POST", "/api/multi-server", raw, action["requestId"]) if signed else {"X-Canvas-Token": "test-token", "Origin": self.a.origin}
                response = self.a.client.post("/api/multi-server", content=raw, headers={**headers,
                    "X-Forwarded-Host": "a.example.ts.net", "X-Forwarded-Proto": "https", "X-Forwarded-For": self.b.address,
                    "Tailscale-User-Login": fixture.OWNER, "Content-Type": "application/json"})
                self.assertEqual(response.status_code, 403, response.text)

    def test_known_unpaired_peer_becomes_unreachable_and_disabled_setting_persists(self):
        self.a.local({"action": "settings", "autoPair": False})
        result = self.discover()
        self.assertEqual(result["servers"][0]["status"], "discovered")
        last_seen = result["servers"][0]["lastSeen"]
        self.status["Peer"]["b"]["Online"] = False
        self.save_identity()
        result = self.discover(request_id="offline")
        self.assertEqual(result["servers"][0]["status"], "unreachable")
        self.assertEqual(result["servers"][0]["lastSeen"], last_seen)
        self.a.restart()
        self.assertFalse(self.a.runtime.paired_access().snapshot()["settings"]["autoPair"])

    def test_discovery_limits_probe_concurrency_and_global_candidates(self):
        self.a.local({"action": "settings", "autoPair": False})
        self.status["Peer"] = {str(index): {"UserID": 1, "Online": True, "DNSName": f"peer-{index:03}.example.ts.net."} for index in range(80)}
        self.save_identity()
        active, peak, total = 0, 0, 0
        lock = threading.Lock()
        def probe(origin, *, timeout):
            nonlocal active, peak, total
            with lock:
                active += 1
                total += 1
                peak = max(peak, active)
            time.sleep(0.02)
            with lock:
                active -= 1
            return {"protocol": 1, "serverId": str(uuid.uuid5(uuid.NAMESPACE_URL, origin)), "label": "Fixture",
                    "origin": origin, "publicKey": self.b.runtime.paired_access()._keys()["publicKey"], "tailscaleUser": fixture.OWNER, "autoPair": False}
        with patch.object(self.a.runtime.paired_access().discovery(), "probe", side_effect=probe):
            self.discover()
        self.assertEqual(total, 64)
        self.assertGreater(peak, 1)
        self.assertLessEqual(peak, 4)


if __name__ == "__main__":
    unittest.main()
