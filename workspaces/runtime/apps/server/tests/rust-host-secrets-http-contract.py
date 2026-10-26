#!/usr/bin/env python3
"""HTTP contracts for Linux guest project secrets and Mac-only vault actions."""

from __future__ import annotations

import json
import math
import os
from pathlib import Path
import shlex
import socket
import socketserver
import sys
import tempfile
import threading
import unittest

from test_isolation import isolate_supervisor_environment

isolate_supervisor_environment()
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))

from http_backend_harness import TimedHTTPContractTest, start_contract_backend
from server_test_waits import HANG_GUARD_SECONDS

FAKE_PULL_GATE_TIMEOUT_MARGIN_SECONDS = 5.0
FAKE_PULL_GATE_POLL_INTERVAL_SECONDS = 0.01
FAKE_PULL_GATE_TIMEOUT_SECONDS = HANG_GUARD_SECONDS + FAKE_PULL_GATE_TIMEOUT_MARGIN_SECONDS
FAKE_PULL_GATE_MAX_ATTEMPTS = math.ceil(
    FAKE_PULL_GATE_TIMEOUT_SECONDS / FAKE_PULL_GATE_POLL_INTERVAL_SECONDS
)


class GuestHandler(socketserver.StreamRequestHandler):
    def handle(self) -> None:
        line = self.rfile.readline()
        if not line:
            return
        request = json.loads(line)
        self.server.requests.append(request)
        response = {"id": request["id"], "result": {"names": ["TOKEN"]}}
        self.wfile.write(json.dumps(response).encode() + b"\n")


class GuestServer(socketserver.ThreadingUnixStreamServer):
    daemon_threads = True
    allow_reuse_address = True

    def __init__(self, path: Path):
        self.requests: list[dict] = []
        super().__init__(str(path), GuestHandler)


class HostSecretsHTTPContract(TimedHTTPContractTest, unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.rust_backend = bool(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"))
        cls.fake_tools = tempfile.TemporaryDirectory(dir=os.environ.get("TMPDIR"))
        cls.fake_desk = Path(cls.fake_tools.name) / "layr-desk"
        cls.fake_desk_calls = Path(cls.fake_tools.name) / "layr-desk-calls.txt"
        cls.fake_desk_pull_release = Path(cls.fake_tools.name) / "release-pull"
        fake_desk_pull_release = shlex.quote(str(cls.fake_desk_pull_release))
        cls.fake_desk.write_text(
            "#!/bin/sh\n"
            f"printf '%s\\n' \"$*\" >> {shlex.quote(str(cls.fake_desk_calls))}\n"
            "case \"$1\" in\n"
            "  unlock) [ \"$(cat)\" = 'fixture words' ] || exit 31 ;;\n"
            "  aws-login)\n"
            "    [ \"$#\" -eq 4 ] && [ \"$2\" = --profile ] && \\\n"
            "      [ \"$3\" = fixture-profile ] && [ \"$4\" = --json ] || exit 33\n"
            "    printf '%s\\n' '{\"page\":\"https://signin.example.invalid/authorize\",\"code\":\"fixture-device-code\"}'\n"
            "    ;;\n"
            "  service) printf '%s\\n' '{\"installed\":false}' ;;\n"
            "  status) printf '%s\\n' '{\"projects\":[{\"name\":\"project-one\",\"unlocked\":true}]}' ;;\n"
            "  pull)\n"
            f"    attempts=0; max_attempts={FAKE_PULL_GATE_MAX_ATTEMPTS}\n"
            f"    while [ ! -e {fake_desk_pull_release} ] && [ $attempts -lt $max_attempts ]; do\n"
            "      attempts=$((attempts + 1))\n"
            f"      sleep {FAKE_PULL_GATE_POLL_INTERVAL_SECONDS}\n"
            "    done\n"
            f"    [ -e {fake_desk_pull_release} ] || exit 34\n"
            "    printf '%s\\n' '{\"seq\":7,\"files\":2,\"bytes\":1024}' ;;\n"
            "  *) exit 32 ;;\n"
            "esac\n"
        )
        cls.fake_desk.chmod(0o700)
        previous_desk = os.environ.get("STUDIO_LAYR_DESK_BIN")
        os.environ["STUDIO_LAYR_DESK_BIN"] = str(cls.fake_desk)
        def restore_desk_environment() -> None:
            if previous_desk is None:
                os.environ.pop("STUDIO_LAYR_DESK_BIN", None)
            else:
                os.environ["STUDIO_LAYR_DESK_BIN"] = previous_desk
        cls.addClassCleanup(restore_desk_environment)
        cls.addClassCleanup(cls.fake_tools.cleanup)
        cls.backend = start_contract_backend(cls)
        # Rust backends use the harness's short external VM path to stay below
        # sockaddr_un limits; Python keeps its VM socket under the runtime state.
        guest_dir = cls.backend.vm_state if cls.rust_backend else cls.backend.state / "linux-vm"
        guest_dir.mkdir(parents=True, exist_ok=True)
        cls.guest = GuestServer(guest_dir / "guest.sock")
        cls.guest_thread = threading.Thread(target=cls.guest.serve_forever, daemon=True)
        cls.guest_thread.start()
        cls.addClassCleanup(cls.stop_guest)

    @classmethod
    def stop_guest(cls) -> None:
        cls.guest.shutdown()
        cls.guest_thread.join(timeout=2)
        cls.guest.server_close()

    @classmethod
    def headers(cls):
        return {
            "Origin": cls.backend.origin or "",
            "X-Canvas-Token": cls.backend.session["token"],
            "X-Studio-API-Schema": cls.backend.schema_hash,
        }

    def request(self, path: str, body: dict):
        return self.backend.request("POST", path, body=body, headers=self.headers())

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"), "Rust backend only")
    def test_aws_login_returns_fake_signin_and_replays_completed_job(self):
        body = {"profile": "fixture-profile", "requestId": "vault-aws-login-fixture"}
        previous_calls = (
            self.fake_desk_calls.read_text(encoding="utf-8").splitlines()
            if self.fake_desk_calls.exists()
            else []
        )

        first = self.request("/api/vault/aws-login", body)
        self.assertEqual(first.status, 200, first.body[:1000])

        def completed_login():
            response = self.request("/api/vault/aws-login", body)
            self.assertEqual(response.status, 200, response.body[:1000])
            result = response.json()
            return result if result.get("state") != "running" else None

        result = self.backend.wait_until(completed_login, "fake AWS profile login completion")
        expected = {
            "requestId": body["requestId"],
            "state": "done",
            "page": "https://signin.example.invalid/authorize",
            "code": "fixture-device-code",
        }
        self.assertEqual(result, expected)
        replay = self.request("/api/vault/aws-login", body)
        self.assertEqual(replay.status, 200, replay.body[:1000])
        self.assertEqual(replay.json(), expected)

        calls = self.fake_desk_calls.read_text(encoding="utf-8").splitlines()
        self.assertEqual(
            calls[len(previous_calls):],
            ["aws-login --profile fixture-profile --json"],
            "the Rust route invokes only fake Desk and reuses its job receipt",
        )
        self.assertEqual(
            list(self.backend.vm_state.rglob("vault-mac.json")),
            [],
            "AWS profile login does not change or refresh project vault state",
        )

    def test_project_secret_routes_use_guest_and_never_return_values(self):
        listed = self.request("/api/project-secrets/list", {"projectId": "project-1"})
        self.assertEqual(listed.status, 200, listed.body[:1000])
        self.assertEqual(listed.json(), {"projectId": "project-1", "names": ["TOKEN"]})

        set_result = self.request("/api/project-secrets/set", {
            "projectId": "project-1", "name": "TOKEN", "value": "fixture-secret-value",
            "requestId": "secret-set-1",
        })
        self.assertEqual(set_result.status, 200, set_result.body[:1000])
        self.assertEqual(set_result.json(), {"projectId": "project-1", "names": ["TOKEN"]})
        self.assertNotIn(b"fixture-secret-value", set_result.body)

        removed = self.request("/api/project-secrets/remove", {
            "projectId": "project-1", "name": "TOKEN", "requestId": "secret-remove-1",
        })
        self.assertEqual(removed.status, 200, removed.body[:1000])
        self.assertEqual(removed.json(), {"projectId": "project-1", "names": ["TOKEN"]})

        self.assertEqual([call["method"] for call in self.guest.requests], [
            "secrets.list", "secrets.set", "secrets.remove",
        ])
        self.assertEqual(self.guest.requests[1]["id"], "secrets-set:secret-set-1")
        self.assertEqual(self.guest.requests[2]["id"], "secrets-remove:secret-remove-1")

    def test_mac_vault_operations_use_the_fake_layr_desk(self):
        vm_state = self.backend.vm_state if self.rust_backend else self.backend.state / "linux-vm"
        project = vm_state / "layr-projects/project-1"
        project.mkdir(parents=True)
        (project / "project.json").write_text(json.dumps({"projectId":"project-1","source":"/tmp/Project One"}))
        (project / "vault.json").write_text(json.dumps({"configured":True,"prefix":"project-one"}))
        (project / "vault-site.json").write_text(json.dumps({"site":"https://desk.example/"}))
        pull_parent = self.backend.state / "pull-parent"
        pull_parent.mkdir()
        unlock = self.request("/api/vault/unlock", {"projectId": "project-1", "words": "fixture words"})
        self.assertNotIn(b"fixture words", unlock.body)
        self.assertEqual(unlock.status, 200, unlock.body[:1000])
        self.assertEqual(unlock.json(), {"unlocked": True})

        body = {"projectId": "project-1", "requestId": "pull-1", "to": str(pull_parent)}
        try:
            first = self.request("/api/vault/pull", body)
            self.assertEqual(first.status, 200, first.body[:1000])
            self.assertEqual(first.json()["state"], "running")
        finally:
            self.fake_desk_pull_release.touch()
        result = self.backend.wait_until(
            lambda: (response.json() if (response := self.request("/api/vault/pull", body)).status == 200
                     and response.json().get("state") != "running" else None),
            "fake vault pull completion",
        )
        self.assertEqual(result["state"], "done")
        self.assertEqual((result["seq"], result["files"], result["bytes"]), (7, 2, 1024))


if __name__ == "__main__":
    unittest.main()
