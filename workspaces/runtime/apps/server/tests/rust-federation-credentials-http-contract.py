#!/usr/bin/env python3
"""Local credential lifecycle through both HTTP backends, using disposable keys."""
from __future__ import annotations

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import hashlib
import json
import os
from pathlib import Path
import sqlite3
import sys
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from codex_runtime import Runtime
from http_backend_harness import TimedHTTPContractTest, start_contract_backend
from runtime_test_environment import make_runtime


class CredentialsHTTPContract(TimedHTTPContractTest, unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.backend = start_contract_backend(cls)
        cls.backend.stop()
        runtime = make_runtime(Runtime, cls.backend)
        try:
            access = runtime.paired_access()
            cls.expected = access.identity()
            # Use the real Python writer, interrupted after its durable reservation.
            with patch.object(access, "_recover_name", return_value=None):
                access.rename("Recovered Python name", "python-pending", "local")
        finally:
            runtime.close()
        cls.backend.start()
        cls.backend.ready_samples.append(cls.backend.ready_seconds or 0)

    def headers(self):
        return {"Origin": self.backend.origin or "",
                "X-Canvas-Token": self.backend.session["token"],
                "X-Studio-API-Schema": self.backend.schema_hash}

    def snapshot(self):
        result = self.backend.request("GET", "/api/multi-server", headers=self.headers())
        self.assertEqual(result.status, 200, result.body[:500])
        self.assertNotIn(b"privateKey", result.body)
        self.assertNotIn(b"PRIVATE KEY", result.body)
        return result.json()

    def name(self, request_id, label, target="local"):
        return self.backend.request("POST", "/api/multi-server", headers=self.headers(), body={
            "action": "name", "serverId": target, "requestId": request_id, "label": label})

    def rows(self):
        with sqlite3.connect(self.backend.state / "canvas.sqlite3") as db:
            return {table: db.execute(f"SELECT * FROM {table} ORDER BY rowid").fetchall()
                    for table in ("runtime_access_name_requests", "runtime_access_name_targets", "runtime_access_audit")}

    def file_digest(self):
        return hashlib.sha256((self.backend.state / "multi-server/identity.json").read_bytes()).hexdigest()

    def test_a_python_identity_and_pending_name_are_recovered_once(self):
        identity = self.snapshot()["identity"]
        self.assertEqual(identity["serverId"], self.expected["serverId"])
        self.assertEqual(identity["publicKey"], self.expected["publicKey"])
        self.assertEqual(identity["label"], "Recovered Python name")
        rows = self.rows()
        self.assertEqual(rows["runtime_access_name_requests"], [("python-pending", "Recovered Python name", "local", 1)])
        self.snapshot()
        self.assertEqual(self.rows(), rows)

    def test_b_label_schema_matches_python_name_policy(self):
        from codex_multi_server import AccessError, MultiServerService
        cases = json.loads((Path(__file__).parent / "fixtures/rust-federation-credentials/names.json").read_text())
        for case in cases:
            with self.subTest(case=case["name"]):
                try:
                    MultiServerService.validate_name(case["label"])
                    accepted = True
                except AccessError:
                    accepted = False
                self.assertEqual(accepted, case["valid"])
                before, digest = self.rows(), self.file_digest()
                response = self.name("corpus-" + case["name"], case["label"])
                self.assertEqual(response.status, 200 if accepted else 400, response.body[:500])
                if not accepted:
                    self.assertEqual(self.rows(), before)
                    self.assertEqual(self.file_digest(), digest)

    def test_b_rename_replay_conflict_and_refusal_do_not_write(self):
        keys_before = json.loads((self.backend.state / "multi-server/identity.json").read_text())
        result = self.name("local-name", "Local café")
        self.assertEqual(result.status, 200, result.body[:500])
        self.assertEqual(result.json()["identity"]["label"], "Local café")
        keys_after = json.loads((self.backend.state / "multi-server/identity.json").read_text())
        for key in ("serverId", "publicKey", "privateKey"):
            self.assertEqual(hashlib.sha256(keys_before[key].encode()).hexdigest(),
                             hashlib.sha256(keys_after[key].encode()).hexdigest())
        rows, digest = self.rows(), self.file_digest()
        replay = self.name("local-name", "Local café", target=keys_after["serverId"])
        self.assertEqual(replay.status, 200, replay.body[:500])
        self.assertEqual(self.name("local-name", "Changed").status, 409)
        for label in (" outer", "trailing ", "line\nbreak", "hidden\u200b", "tab\tname"):
            with self.subTest(label=repr(label)):
                self.assertEqual(self.name("invalid-label", label).status, 400)
        self.assertEqual(self.rows(), rows)
        self.assertEqual(self.file_digest(), digest)

    def test_c_cached_identity_has_python_read_timing(self):
        expected = self.snapshot()["identity"]
        path = self.backend.state / "multi-server/identity.json"
        original = path.read_bytes()
        try:
            path.write_text("invalid credential sentinel")
            self.assertEqual(self.snapshot()["identity"], expected)
        finally:
            path.write_bytes(original)

    def test_d_restart_preserves_renamed_keys_and_receipts(self):
        before, digest, rows = self.snapshot()["identity"], self.file_digest(), self.rows()
        self.backend.stop()
        self.backend.start()
        self.assertEqual(self.snapshot()["identity"], before)
        self.assertEqual(self.file_digest(), digest)
        self.assertEqual(self.rows(), rows)

    def test_e_fresh_identity_is_private_stable_and_signable(self):
        self.backend.stop()
        path = self.backend.state / "multi-server/identity.json"
        path.unlink()
        self.backend.start()
        first = self.snapshot()["identity"]
        self.assertEqual(first["label"], "Studio server")
        self.assertNotEqual(first["serverId"], self.expected["serverId"])
        self.assertTrue(first["publicKey"].startswith("-----BEGIN PUBLIC KEY-----"))
        self.assertEqual(path.stat().st_mode & 0o777, 0o600)
        self.assertEqual(path.parent.stat().st_mode & 0o777, 0o700)
        digest = self.file_digest()
        self.assertEqual(self.snapshot()["identity"], first)
        self.assertEqual(self.file_digest(), digest)

    def test_f_refuses_unsafe_identity_without_replacing_it(self):
        if not os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"):
            self.skipTest("Rust fail-closed credential loading; Python loads during backend startup")
        self.backend.stop()
        path = self.backend.state / "multi-server/identity.json"
        digest = self.file_digest()
        path.chmod(0o644)
        try:
            self.backend.start()
            result = self.backend.request("GET", "/api/multi-server", headers=self.headers())
            self.assertEqual(result.status, 503, result.body[:500])
            self.assertNotIn(b"PRIVATE KEY", result.body)
            self.assertEqual(self.file_digest(), digest)
        finally:
            path.chmod(0o600)


if __name__ == "__main__":
    unittest.main()
