"""Check the opt-in native host through a hidden WebView and isolated fixtures."""
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import hashlib
import os
from pathlib import Path
import subprocess
import tempfile
import threading
import uuid

ROOT = Path(__file__).resolve().parents[4]
BINARY = ROOT / "target/debug/studio-native-client"


def run(directory, mode, extra=None):
    report = directory / "webview-result.json"
    report.unlink(missing_ok=True)
    env = dict(os.environ, STUDIO_NATIVE_CHECK_DIR=str(directory), STUDIO_NATIVE_CHECK_MODE=mode,
               STUDIO_NATIVE_CHECK_NETWORK="0", CODEX_AGENTS_SUPERVISOR_MODE="0")
    env.pop("CODEX_DESKTOP_PROFILE", None)
    env.pop("CODEX_DESKTOP_EPHEMERAL_PROFILE", None)
    env.pop("CODEX_NATIVE_PROFILE_IMPORT", None)
    env.pop("STUDIO_NATIVE_CHECK_STORE_ID", None)
    env.update(extra or {})
    try:
        completed = subprocess.run([str(BINARY), "--hidden", "--attar-check-scenario"], env=env, timeout=60, capture_output=True, text=True)
    except subprocess.TimeoutExpired as error:
        details = (error.stderr or b"").decode(errors="replace")
        report_text = report.read_text() if report.exists() else "No WebView report"
        raise SystemExit(f"Native {mode} timed out: {details[-4000:]}\n{report_text}") from None
    if completed.returncode or not report.exists():
        raise SystemExit(f"Native {mode} failed ({completed.returncode}): {completed.stderr[-4000:]}")
    result = json.loads(report.read_text())
    if result != {"passed": True, "mode": mode}:
        raise SystemExit(f"Native {mode} failed: {result}")
    for line in completed.stderr.splitlines():
        if line.startswith("Hidden native step:"):
            print(line, flush=True)
    print(f"Hidden WebView {mode}: passed", flush=True)


with tempfile.TemporaryDirectory(prefix="studio-native-webview-") as temporary:
    directory = Path(temporary).resolve()
    for profile, mode in (("store-a", "profile-store-write"), ("store-b", "profile-store-empty"), ("store-a", "profile-store-read")):
        target = directory / profile
        run(target, mode, {"CODEX_DESKTOP_PROFILE": str(target)})
    archive = directory / "profile.json"
    archive.write_text(json.dumps({"version": 1, "id": "fixture-profile", "owners": {"root": "http://127.0.0.1:4620"}, "snapshots": [{
        "version": 1, "origin": "http://127.0.0.1:4620", "preferences": {"migrated": "saved Ж🦀"}, "databases": [{
            "name": "migration-fixture", "version": 1, "stores": [{"name": "records", "keyPath": "id", "autoIncrement": False, "indexes": [], "records": [
                [["value", "voice"], ["object", [["id", ["value", "voice"]], ["pcm", ["view", "Int16Array", ["bytes", "AIAAAP9/"]]], ["file", ["file", "bytes.bin", "application/octet-stream", 1234, ["bytes", "AP8q"]]]]]]
            ]}, {"name": "changes", "keyPath": None, "autoIncrement": True, "nextKey": 101, "indexes": [], "records": []}]
        }]
    }]}))
    store_id = uuid.uuid4().hex
    print(f"Isolated WebKit profile store: {store_id}", flush=True)
    run(directory / "profile-client", "profile-import", {"CODEX_NATIVE_PROFILE_IMPORT": str(archive), "STUDIO_NATIVE_CHECK_STORE_ID": store_id})
    chunked = json.loads(archive.read_text())
    chunked["version"] = 2
    chunked["parts"] = "profile.parts"
    parts = directory / chunked["parts"]
    parts.mkdir(mode=0o700)
    for snapshot in chunked["snapshots"]:
        snapshot["version"] = 2
        for database in snapshot["databases"]:
            for store in database["stores"]:
                records = store.pop("records")
                store["chunks"] = []
                if records:
                    data = json.dumps(records, ensure_ascii=False, separators=(",", ":")).encode()
                    digest = hashlib.sha256(data).hexdigest()
                    (parts / f"{digest}.json").write_bytes(data)
                    store["chunks"].append({"sha256": digest, "bytes": len(data), "records": len(records)})
    archive.write_text(json.dumps(chunked))
    run(directory / "chunk-profile-client", "profile-import", {"CODEX_NATIVE_PROFILE_IMPORT": str(archive), "STUDIO_NATIVE_CHECK_STORE_ID": uuid.uuid4().hex})
    for mode in ("write", "read", "frames"):
        run(directory, mode)

    commands = []
    identity_state = str(directory / "wrong-state")

    class Fixture(BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def send(self, status, body, headers=None):
            self.send_response(status)
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Connection", "close")
            for name, value in (headers or {}).items():
                self.send_header(name, value)
            self.end_headers()
            self.wfile.write(body)

        def do_GET(self):
            if self.path == "/api/native-check-events":
                self.send(200, "id: rev-1\nevent: updated\ndata: Привет 🦀\ndata: second\n\n".encode(), {"Content-Type": "text/event-stream"})
                return
            if self.path == "/api/sync/identity":
                self.send(200, json.dumps({"workspaceId": "a" * 32, "chatState": True}).encode())
                return
            if self.path.startswith("/api/sync/pull?scope=drafts&"):
                self.send(200, json.dumps({"workspaceId": "a" * 32, "documents": [], "checkpoint": {"seq": 0}}).encode())
                return
            if self.path == "/api/desktop":
                body = dict(application="codex-agents", protocol=1, pid=os.getpid(),
                            stateDir=identity_state, backendBuild="a" * 64)
            elif self.path == "/api/session":
                body = {"token": "fixture-native-token"}
            else:
                self.send(404, b'{"error":"Unknown fixture path"}')
                return
            self.send(200, json.dumps(body).encode(), {"Content-Type": "application/json"})

        def do_POST(self):
            body = self.rfile.read(int(self.headers.get("Content-Length", "0")))
            if self.path == "/api/sync/drafts":
                self.send(200, b'[]')
                return
            if self.path != "/api/native-check":
                self.send(404, b'{}')
                return
            commands.append((dict((key.lower(), value) for key, value in self.headers.items()), body))
            self.send(409, b"a\0\xff", {"X-Studio-API-Schema-Mismatch": "1"})

    fixture = ThreadingHTTPServer(("127.0.0.1", 0), Fixture)
    thread = threading.Thread(target=fixture.serve_forever, daemon=True)
    thread.start()
    env = dict(CODEX_AGENTS_STATE_DIR=str(directory), CODEX_DESKTOP_PORT=str(fixture.server_port),
               STUDIO_NATIVE_CHECK_NETWORK="1")
    try:
        run(directory, "network-invalid", env)
        assert not commands, "An invalid backend received a command"
        identity_state = str(directory)
        run(directory, "network", env)
        run(directory / "shell-client", "network-shell", env)
        run(directory, "network-stream", env)
        run(directory, "network-drafts-write", env)
        run(directory, "network-drafts-read", env)
        run(directory, "drafts-offline-write")
        run(directory, "drafts-offline-read")
        assert len(commands) == 2, f"Expected two distinct commands, got {len(commands)}"
        assert commands[1][1] == b"null"
        assert commands[1][0]["x-studio-request-id"] == "frame-http-command"
        headers, body = commands[0]
        assert body == b"null", body
        assert headers["x-canvas-token"] == "fixture-native-token"
        assert headers["x-studio-request-id"] == "original-http-command"
        assert headers["x-studio-api-schema"] == "fixture-client-schema"
        assert headers["x-canvas-workspace"] == "fixture-workspace"
    finally:
        fixture.shutdown()
        fixture.server_close()
        thread.join()
