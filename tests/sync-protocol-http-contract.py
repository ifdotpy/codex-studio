"""Exercise protocol-3 sync negotiation and typed resource events over HTTP."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import json
import http.client
import os
from pathlib import Path
import subprocess
import tempfile
import urllib.error
import urllib.parse
import urllib.request
import stat
import socket

root = Path(__file__).resolve().parents[1]
with tempfile.TemporaryDirectory(prefix="sync-protocol-") as directory:
    process = subprocess.Popen(
        ["python3", "-B", str(root / "tests/simple-ui-fixture.py"), directory],
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        env={**os.environ, "CODEX_BOARD_STATE_DIR": directory + "/board"},
    )
    try:
        port = process.stdout.readline().strip()
        assert port.isdigit(), process.stderr.read()
        origin = "http://127.0.0.1:" + port

        def get(path, headers=None):
            return urllib.request.urlopen(
                urllib.request.Request(origin + path, headers=headers or {}), timeout=10
            )

        protocol_response = get("/api/sync/protocol")
        api_schema = protocol_response.headers.get("X-Studio-API-Schema")
        protocol = json.load(protocol_response)
        assert protocol["protocolVersion"] == 3
        assert protocol["supportedVersions"] == [3]
        assert protocol["maxStreamBytes"] > 0
        socket_path = Path(directory) / "canvas.sock"
        assert stat.S_IMODE(socket_path.stat().st_mode) == 0o600

        def get_unix(path, read_lines=0):
            connection = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            try:
                connection.settimeout(10)
                connection.connect(str(socket_path))
                connection.sendall(
                    f"GET {path} HTTP/1.1\r\nHost: localhost\r\n"
                    "Connection: close\r\n\r\n".encode()
                )
                response = http.client.HTTPResponse(connection)
                response.begin()
                body = (
                    b"".join(response.fp.readline() for _ in range(read_lines))
                    if read_lines
                    else response.read()
                )
                return response.status, dict(response.getheaders()), body
            finally:
                connection.close()

        unix_status, unix_headers, unix_body = get_unix("/api/sync/protocol")
        assert unix_status == 200
        assert json.loads(unix_body)["supportedVersions"] == [3]
        unix_schema = unix_headers.get("X-Studio-API-Schema")
        unix_parameters = {"protocol": 3, "resources": json.dumps([{"kind": "state"}], separators=(",", ":"))}
        if unix_schema:
            unix_parameters["apiSchema"] = unix_schema
        unix_query = urllib.parse.urlencode(unix_parameters)
        unix_status, _, unix_body = get_unix("/api/sync/stream?" + unix_query, read_lines=10)
        assert unix_status == 200
        assert b"event: resources" in unix_body

        resources = json.dumps([{"kind": "state"}], separators=(",", ":"))
        parameters = {"protocol": 3, "resources": resources}
        if api_schema:
            parameters["apiSchema"] = api_schema
        query = urllib.parse.urlencode(parameters)
        path = "/api/sync/stream?" + query

        def resource_event(stream):
            while True:
                fields = {}
                while True:
                    line = stream.readline().decode().strip()
                    if not line:
                        break
                    key, _, value = line.partition(":")
                    fields[key] = value.lstrip()
                if fields.get("event") == "resources":
                    return fields

        with get(path) as stream:
            event = resource_event(stream)
        event_id = event.get("id")
        assert event_id
        payload = json.loads(event["data"])
        assert payload["protocol"] == 3
        assert payload["workspaceId"]
        assert payload["reason"] == "initial"
        assert payload["resources"] == [{"kind": "state"}]

        reconnect_headers = {"Last-Event-ID": event_id}
        with get(path, reconnect_headers) as stream:
            resumed = json.loads(resource_event(stream)["data"])
        assert resumed["reason"] == "reconnect"
        assert resumed["resources"] == [{"kind": "state"}]

        for selector, headers in (
            ("?protocol=1", {}),
            ("?protocol=2", {}),
            ("", {}),
            ("?protocol=3", {"X-Codex-Sync-Protocol": "1"}),
            ("?protocol=3", {"X-Codex-Sync-Protocol": "2"}),
        ):
            try:
                get("/api/sync/stream" + selector, headers)
                raise AssertionError(f"unsupported or missing selector was accepted: {selector}")
            except urllib.error.HTTPError as error:
                assert error.code == 426, error.read()

        for removed in ("/api/sync/generations", "/api/transcript/stream"):
            try:
                get(removed)
                raise AssertionError(f"removed route still exists: {removed}")
            except urllib.error.HTTPError as error:
                assert error.code in (404, 405), error.read()
        print("protocol-3 sync HTTP contract passed")
    finally:
        process.terminate()
        process.wait(timeout=10)
