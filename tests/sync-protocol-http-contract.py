"""Exercise v1 SSE resume, cursor errors, reset, bounds and negotiation."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import json
import http.client
import os
from pathlib import Path
import sqlite3
import subprocess
import tempfile
import urllib.error
import urllib.request
import stat

root = Path(__file__).resolve().parents[1]
with tempfile.TemporaryDirectory(prefix="sync-protocol-") as directory:
    process = subprocess.Popen(
        ["python3", "-B", str(root / "tests/simple-ui-fixture.py"), directory],
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        env={**os.environ, "CODEX_BOARD_STATE_DIR": directory + "/board"},
    )
    try:
        origin = "http://127.0.0.1:" + process.stdout.readline().strip()

        def get(path, headers=None):
            return urllib.request.urlopen(urllib.request.Request(origin + path, headers=headers or {}), timeout=10)

        protocol = json.load(get("/api/sync/protocol"))
        assert protocol["protocolVersion"] == 1
        assert {"streamChanges", "entityReset"} <= set(protocol["capabilities"])
        socket_path = Path(directory) / "canvas.sock"
        assert stat.S_IMODE(socket_path.stat().st_mode) == 0o600
        class UnixHTTPConnection(http.client.HTTPConnection):
            def connect(self):
                self.sock = __import__("socket").socket(__import__("socket").AF_UNIX)
                self.sock.connect(str(socket_path))
        local = UnixHTTPConnection("localhost")
        local.request("GET", "/api/sync/protocol")
        local_response = local.getresponse()
        assert local_response.status == 200
        assert json.loads(local_response.read())["capabilities"] == protocol["capabilities"]
        local.request("GET", "/api/sync/stream?scope=state%3Aentities%3Av1&after=0",
                      headers={"X-Codex-Sync-Protocol": "1"})
        local_stream = local.getresponse()
        assert local_stream.status == 200
        assert local_stream.readline().decode().startswith("id: ")
        local_stream.close()
        local.close()
        state = json.load(get("/api/state"))
        get("/api/sync/pull?scope=state%3Aentities%3Av1&after=0&limit=500&reset=1").close()
        stream = get("/api/sync/stream?scope=state%3Aentities%3Av1&protocol=1&after=0")
        event_id = int(stream.readline().decode().split(":", 1)[1])
        assert stream.readline().decode().strip() == "event: changes"
        payload = json.loads(stream.readline().decode()[6:])
        assert len(payload["documents"]) <= 100
        assert len(json.dumps(payload).encode()) <= 1024 * 1024
        stream.close()

        resumed = get(f"/api/sync/stream?scope=state%3Aentities%3Av1&protocol=1&after={event_id}",
                      {"Last-Event-ID": str(event_id)})
        resumed_id = int(resumed.readline().decode().split(":", 1)[1])
        assert resumed.readline().decode().strip() == "event: changes"
        resumed_payload = json.loads(resumed.readline().decode()[6:])
        resumed.close()
        assert resumed_id > event_id
        assert all(row["seq"] > event_id for row in resumed_payload["documents"])

        # A consumer that accepts headers but never drains the body is closed;
        # it can resume from its last applied ID without a server-side queue.
        slow = get("/api/sync/stream?scope=state%3Aentities%3Av1&protocol=1&after=0")
        import time
        time.sleep(0.25)
        slow.close()
        recovered = get(f"/api/sync/stream?scope=state%3Aentities%3Av1&protocol=1&after={event_id}")
        assert recovered.readline().decode().startswith("id: ")
        recovered.close()

        ahead = get("/api/sync/stream?scope=drafts&protocol=1&after=999999999")
        assert ahead.readline().decode().strip() == "event: cursor-ahead"
        ahead.close()

        mismatch = urllib.request.Request(origin + "/api/sync/stream?scope=drafts",
                                          headers={"X-Codex-Sync-Protocol": "99"})
        try:
            urllib.request.urlopen(mismatch, timeout=5)
            raise AssertionError("Unsupported protocol was accepted")
        except urllib.error.HTTPError as error:
            assert error.code == 426

        db = sqlite3.connect(Path(directory) / "canvas.sqlite3")
        high = db.execute("SELECT COALESCE(MAX(seq), 0) FROM sync_entities WHERE collection NOT LIKE 'transcript:%'").fetchone()[0]
        db.execute("INSERT INTO sync_entity_meta(key,value) VALUES('entity_tombstone_floor',?) "
                   "ON CONFLICT(key) DO UPDATE SET value=excluded.value", (str(max(high - 1, 2)),))
        db.commit()
        db.close()
        reset = get("/api/sync/stream?scope=state%3Aentities%3Av1&protocol=1&after=1")
        assert reset.readline().decode().strip() == "event: reset"
        reset.close()
        print("sync protocol SSE contract passed")
    finally:
        process.terminate()
        process.wait(timeout=10)
