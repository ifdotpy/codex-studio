"""Authenticated relay, retry, and external-process-to-SSE contracts."""

from __future__ import annotations

import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import threading
import time
from collections.abc import Sequence
from typing import cast
from unittest.mock import patch
from urllib.error import URLError
from urllib.parse import quote
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.request import Request, urlopen
import unittest

from fastapi import FastAPI, Request as FastAPIRequest
from fastapi.exceptions import RequestValidationError
from fastapi.responses import JSONResponse
from fastapi.testclient import TestClient
from starlette.types import ASGIApp

from studio_api.context import ApiContext
from studio_api.middleware import RequestBoundary
from studio_api.sync.resources.hub import ResourceHub
from studio_api.sync.resources.models import (
    ResourceRef,
    RoomResource,
    StateResource,
    WorkspaceResource,
)
from studio_api.sync.resources.relay import client as relay_client
from studio_api.sync.resources.relay.models import ResourceNotifyRequest
from studio_api.sync.resources.relay.receipts import NotifyReceipts
from studio_api.sync.resources.relay.router import create_router

SCRIPTS = Path(__file__).resolve().parents[4]


class _Remote:
    def request_origin(self, _headers: object, _peer: str, _port: int) -> str:
        return "http://testserver"


class _Context:
    token = "relay-test-token"
    schema_only = True
    remote = _Remote()
    canvas = type("Canvas", (), {"root": "/tmp/relay-test-state"})()

    def __init__(self) -> None:
        self.hub = ResourceHub(f"relay-workspace-{id(self)}")

    def resource_hub(self) -> ResourceHub:
        return self.hub

    async def get_api_schema_hash(self) -> str:
        return "relay-test-schema"

    def peek_api_schema_hash(self) -> str:
        return "relay-test-schema"

    def start_api_schema_hash(self) -> None:
        return None

    def entity_sequence(self) -> int:
        return 1

    def send(self, _request: FastAPIRequest, value: object, status: int = 200, **_kwargs: object) -> JSONResponse:
        if hasattr(value, "model_dump"):
            value = value.model_dump(mode="json", by_alias=True)
        return JSONResponse(value, status_code=status)


def _client(context: _Context, *, boundary: bool = True) -> TestClient:
    app = FastAPI()
    app.include_router(create_router(cast(ApiContext, context)))

    @app.exception_handler(RequestValidationError)
    async def validation_error(_request: FastAPIRequest, _error: RequestValidationError) -> JSONResponse:
        return JSONResponse({"error": "Invalid request"}, status_code=400)

    if boundary:
        app.add_middleware(RequestBoundary, context=cast(ApiContext, context))
    return TestClient(app, base_url="http://testserver")


def _post(
    client: TestClient,
    body: dict[str, object],
    *,
    token: str | None = "relay-test-token",
    origin: str = "http://testserver",
):
    headers = {"Origin": origin}
    if token is not None:
        headers["X-Canvas-Token"] = token
    return client.post("/api/sync/notify", json=body, headers=headers)


class RelayRouteTests(unittest.TestCase):
    def setUp(self) -> None:
        self.context = _Context()
        self.client = _client(self.context)
        self.entity_publication = threading.Event()
        publish = self.context.hub.publish_entity_sequence

        def observe_publication(
            sequence: int,
            entity_sequences: Sequence[int] = (),
            *,
            reset: bool = False,
        ) -> int:
            result = publish(sequence, entity_sequences, reset=reset)
            self.entity_publication.set()
            return result

        self.publication_patch = patch.object(
            self.context.hub,
            "publish_entity_sequence",
            side_effect=observe_publication,
        )
        self.publication_patch.start()
        self.addCleanup(self.publication_patch.stop)

    def wait_for_entity_publication(self) -> None:
        self.assertTrue(
            self.entity_publication.wait(timeout=5),
            "entity publisher did not complete its observable hub update",
        )

    def test_authenticated_typed_post_publishes_and_exact_retry_is_deduplicated(self) -> None:
        body = {"requestId": "stable-notify-1", "resources": [{"kind": "state"}]}
        first = _post(self.client, body)
        second = _post(self.client, body)
        self.wait_for_entity_publication()
        self.assertEqual(first.status_code, 200, first.text)
        self.assertEqual(first.json(), {"requestId": "stable-notify-1", "accepted": True})
        self.assertEqual(second.json(), first.json())
        self.assertEqual(self.context.hub._revision, 1)

        conflict = _post(self.client, {
            "requestId": "stable-notify-1",
            "resources": [{"kind": "room", "roomId": "room-a"}],
        })
        self.assertEqual(conflict.status_code, 409)
        self.assertEqual(self.context.hub._revision, 1)

    def test_unauthenticated_or_invalid_payload_never_publishes(self) -> None:
        body = {"requestId": "stable-notify-2", "resources": [{"kind": "state"}]}
        self.assertEqual(_post(self.client, body, token=None).status_code, 403)
        self.assertEqual(_post(self.client, body, token="wrong-token").status_code, 403)
        self.assertEqual(_post(self.client, body, origin="https://other.example").status_code, 403)
        self.assertEqual(_post(self.client, {**body, "unexpected": True}).status_code, 400)
        self.assertEqual(_post(self.client, {"requestId": "x", "resources": []}).status_code, 400)
        too_many = {"requestId": "too-many", "resources": [{"kind": "state"}] * 257}
        self.assertEqual(_post(self.client, too_many).status_code, 400)
        self.assertEqual(self.context.hub._revision, 0)

    def test_receipt_cache_normalizes_resource_order_and_only_stores_after_publish(self) -> None:
        cache = NotifyReceipts(capacity=1)
        state = ResourceRef(StateResource(kind="state"))
        room = ResourceRef(RoomResource(kind="room", roomId="room-a"))
        first = ResourceNotifyRequest(requestId="same", resources=[state, room, state])
        reordered = ResourceNotifyRequest(requestId="same", resources=[room, state])
        calls: list[tuple[str, ...]] = []

        def publish(resources: object) -> None:
            calls.append(tuple(resource.model_dump_json() for resource in resources))

        self.assertTrue(cache.publish_once("workspace", first, publish))
        self.assertFalse(cache.publish_once("workspace", reordered, publish))
        self.assertEqual(len(calls), 1)

        failed = ResourceNotifyRequest(requestId="retry", resources=[state])
        with self.assertRaises(RuntimeError):
            cache.publish_once("workspace", failed, lambda _resources: (_ for _ in ()).throw(RuntimeError("publish failed")))
        self.assertTrue(cache.publish_once("workspace", failed, publish))

        evict = ResourceNotifyRequest(
            requestId="evict",
            resources=[ResourceRef(WorkspaceResource(kind="workspace", agentId="agent-a"))],
        )
        self.assertTrue(cache.publish_once("workspace", evict, publish))
        self.assertTrue(cache.publish_once("workspace", first, publish))

    def test_response_loss_retries_only_identical_notification(self) -> None:
        body_calls: list[tuple[str, dict[str, object]]] = []
        calls = 0

        def request_json(_url: str, path: str, data: object = None, _token: str = "", **_kwargs: object) -> object:
            nonlocal calls
            if path == "/api/session":
                return {"token": "relay-test-token"}
            if path == "/api/desktop":
                return {"stateDir": "/tmp/relay-test-state"}
            assert isinstance(data, dict)
            body_calls.append((path, data))
            response = _post(self.client, data)
            self.assertEqual(response.status_code, 200, response.text)
            calls += 1
            if calls == 1:
                raise URLError("fixture response lost after server accepted request")
            return response.json()

        with (patch.object(relay_client, "request_json", side_effect=request_json),
              patch("codex_api_client.request_json", side_effect=request_json),
              patch.object(socket, "create_connection",
                           side_effect=AssertionError("relay unit test attempted a network escape")) as connect,
              patch.object(socket.socket, "connect",
                           side_effect=AssertionError("relay unit test attempted a socket escape")) as socket_connect):
            ack = relay_client.ResourceRelayClient("/tmp/relay-test-state", "http://testserver").notify(
                "response-loss-id", [ResourceRef(StateResource(kind="state"))]
            )
        self.wait_for_entity_publication()
        connect.assert_not_called()
        socket_connect.assert_not_called()
        self.assertEqual(ack.requestId, "response-loss-id")
        self.assertEqual(body_calls[0], body_calls[1])
        self.assertEqual(self.context.hub._revision, 1)


class ExternalCliSseTests(unittest.TestCase):
    def _start_stream(self, origin: str, resources: list[dict[str, str]]):
        raw = json.dumps(resources, separators=(",", ":"))
        response = urlopen(
            f"{origin}/api/sync/stream?protocol=3&resources={quote(raw)}",
            timeout=5,
        )
        response.fp.raw._sock.settimeout(5)
        return response

    @staticmethod
    def _read_frame(stream) -> list[str]:
        lines: list[str] = []
        while True:
            line = stream.readline().decode().rstrip("\r\n")
            if not line:
                if lines:
                    return lines
                continue
            lines.append(line)

    def test_second_process_graph_writer_delivers_authenticated_resource_sse(self) -> None:
        sys.path.insert(0, str(SCRIPTS))
        from codex_canvas import Canvas
        import studio_api.app as app_module
        import studio_api.server as server_module
        from studio_api.sync.resources.relay.router import create_router as create_relay_router

        with tempfile.TemporaryDirectory(prefix="relay-cli-") as temporary:
            root = Path(temporary) / "state"
            profile = Path(temporary) / "profile"
            canvas = Canvas(root)
            original_create_app = app_module.create_app

            def with_relay(context: ApiContext) -> ASGIApp:
                app = original_create_app(context)
                if not any(getattr(route, "path", None) == "/api/sync/notify" for route in app.routes):
                    app.include_router(create_relay_router(context))
                return app

            with patch.object(server_module, "create_app", side_effect=with_relay):
                server = server_module.make_server(canvas, port=0)
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            proxy_requests: list[dict[str, object]] = []
            upstream = f"http://127.0.0.1:{server.server_port}"

            class ResponseLossProxy(BaseHTTPRequestHandler):
                post_requests = proxy_requests

                def do_GET(self) -> None:
                    status, content_type, body = self._forward(None)
                    self.send_response(status)
                    self.send_header("Content-Type", content_type)
                    self.send_header("Content-Length", str(len(body)))
                    self.end_headers()
                    self.wfile.write(body)

                def do_POST(self) -> None:
                    length = int(self.headers.get("Content-Length", "0"))
                    raw_body = self.rfile.read(length)
                    parsed = json.loads(raw_body)
                    self.post_requests.append(parsed)
                    response = self._forward(raw_body)
                    if len(self.post_requests) == 1:
                        self.send_response(200)
                        self.send_header("Content-Type", response[1])
                        self.send_header("Content-Length", str(len(response[2]) + 20))
                        self.end_headers()
                        self.wfile.write(response[2][:max(1, len(response[2]) // 2)])
                        self.wfile.flush()
                        self.connection.shutdown(socket.SHUT_RDWR)
                        self.connection.close()
                        return
                    self.send_response(response[0])
                    self.send_header("Content-Type", response[1])
                    self.send_header("Content-Length", str(len(response[2])))
                    self.end_headers()
                    self.wfile.write(response[2])

                def _forward(self, body: bytes | None) -> tuple[int, str, bytes]:
                    headers = {"Content-Type": self.headers.get("Content-Type", "application/json")}
                    for name in ("X-Canvas-Token", "Origin"):
                        if name in self.headers:
                            headers[name] = self.headers[name]
                    upstream_request = Request(upstream + self.path, data=body, headers=headers)
                    with urlopen(upstream_request, timeout=5) as response:
                        return response.status, response.headers.get("Content-Type", "application/json"), response.read()

                def log_message(self, _format: str, *args: object) -> None:
                    return

            proxy = ThreadingHTTPServer(("127.0.0.1", 0), ResponseLossProxy)
            proxy_thread = threading.Thread(target=proxy.serve_forever, daemon=True)
            proxy_thread.start()
            try:
                deadline = time.monotonic() + 5
                while not server.server.started and time.monotonic() < deadline:
                    time.sleep(0.01)
                self.assertTrue(server.server.started, "isolated API server did not start")
                origin = f"http://127.0.0.1:{server.server_port}"
                stream = self._start_stream(origin, [
                    {"kind": "state"},
                    {"kind": "workspace", "agentId": "host-root"},
                ])
                try:
                    initial = self._read_frame(stream)
                    self.assertIn("event: resources", initial)
                    initial_rates = self._read_frame(stream)
                    self.assertIn("event: token-rates", initial_rates)
                    before_revision = server._context.resource_hub()._revision
                    env = {
                        **os.environ,
                        "CODEX_AGENTS_STATE_DIR": str(root),
                        "CODEX_HOME": str(profile),
                        "CODEX_CANVAS_URL": f"http://127.0.0.1:{proxy.server_port}",
                        "CODEX_BOARD_OWNER": "",
                        "CODEX_AGENT_OWNER": "",
                        "PATH": f"{Path(sys.executable).parent}:{os.environ.get('PATH', '')}",
                    }
                    command = subprocess.run(
                        [str(SCRIPTS / "codex-graph"), "agent", "--id", "host-root", "--name", "Lead"],
                        env=env,
                        capture_output=True,
                        text=True,
                        timeout=15,
                    )
                    self.assertEqual(command.returncode, 0, command.stderr)
                    self.assertEqual(len(proxy_requests), 2)
                    self.assertEqual(proxy_requests[0], proxy_requests[1])
                    self.assertEqual(proxy_requests[0]["requestId"], proxy_requests[1]["requestId"])
                    expected_resources = {
                        (("kind", "state"),),
                        (("agentId", "host-root"), ("kind", "workspace")),
                    }
                    observed_resources = set()
                    observed_revisions = []
                    frame_deadline = time.monotonic() + 5
                    while (time.monotonic() < frame_deadline
                           and (observed_resources != expected_resources
                                or not observed_revisions
                                or max(observed_revisions) < before_revision + 2)):
                        event = self._read_frame(stream)
                        if "event: resources" not in event:
                            continue
                        payload = json.loads(
                            next(line[6:] for line in event if line.startswith("data: "))
                        )
                        observed_revisions.append(payload["revision"])
                        observed_resources.update(
                            tuple(sorted(resource.items()))
                            for resource in payload["resources"]
                        )
                        if (observed_resources == expected_resources
                                and max(observed_revisions) == before_revision + 2):
                            break
                    self.assertEqual(observed_resources, expected_resources)
                    self.assertEqual(max(observed_revisions), before_revision + 2)
                    self.assertTrue(all(
                        before_revision < revision <= before_revision + 2
                        for revision in observed_revisions
                    ))
                    with canvas.connect() as db:
                        stored = db.execute("SELECT count(*) FROM graph_agents WHERE id='host-root'").fetchone()
                    self.assertEqual(stored[0], 1)

                    def run_cli(*arguments: str) -> subprocess.CompletedProcess[str]:
                        return subprocess.run(
                            [str(SCRIPTS / arguments[0]), *arguments[1:]],
                            env=env,
                            capture_output=True,
                            text=True,
                            timeout=15,
                        )

                    created = run_cli("codex-chat", "create", "Planning", "--agent", "host-root")
                    self.assertEqual(created.returncode, 0, created.stderr)
                    chat_id = json.loads(created.stdout)["id"]
                    created_event = self._read_frame(stream)
                    created_payload = json.loads(
                        next(line[6:] for line in created_event if line.startswith("data: "))
                    )
                    self.assertIn({"kind": "state"}, created_payload["resources"])
                    stream.close()
                    stream = self._start_stream(origin, [
                        {"kind": "state"},
                        {"kind": "room", "roomId": chat_id},
                    ])
                    self.assertIn("event: resources", self._read_frame(stream))
                    self.assertIn("event: token-rates", self._read_frame(stream))

                    connected = run_cli("codex-chat", "connect", chat_id, "--agent", "host-root")
                    self.assertEqual(connected.returncode, 0, connected.stderr)
                    connected_event = self._read_frame(stream)
                    connected_payload = json.loads(
                        next(line[6:] for line in connected_event if line.startswith("data: "))
                    )
                    self.assertIn(
                        {"kind": "room", "roomId": chat_id},
                        connected_payload["resources"],
                    )

                    posted = run_cli("codex-chat", "post", chat_id, "Native report", "--agent", "host-root")
                    self.assertEqual(posted.returncode, 0, posted.stderr)
                    # The external notification publishes the room and its
                    # unknown entity sequence separately; accept both frames.
                    posted_payloads = []
                    for _ in range(2):
                        posted_event = self._read_frame(stream)
                        posted_payloads.append(
                            json.loads(
                                next(
                                    line[6:]
                                    for line in posted_event
                                    if line.startswith("data: ")
                                )
                            )
                        )
                    posted_resources = [
                        resource
                        for payload in posted_payloads
                        for resource in payload["resources"]
                    ]
                    self.assertIn({"kind": "state"}, posted_resources)
                    self.assertIn({"kind": "room", "roomId": chat_id}, posted_resources)
                    message = canvas.messages(chat_id)[0]
                    self.assertEqual(message["author"], "host-root")
                    self.assertEqual(message["deliveries"], {})
                    self.assertEqual(len(proxy_requests), 5)
                finally:
                    stream.close()
            finally:
                proxy.shutdown()
                proxy_thread.join(timeout=5)
                proxy.server_close()
                server.shutdown()
                thread.join(timeout=5)
                server.server_close()

    def test_failed_external_write_rolls_back_and_emits_no_notification(self) -> None:
        sys.path.insert(0, str(SCRIPTS))
        from codex_canvas import Canvas
        import studio_api.app as app_module
        import studio_api.server as server_module
        from studio_api.sync.resources.relay.router import create_router as create_relay_router

        with tempfile.TemporaryDirectory(prefix="relay-rollback-") as temporary:
            root = Path(temporary) / "state"
            canvas = Canvas(root)
            original_create_app = app_module.create_app

            def with_relay(context: ApiContext) -> ASGIApp:
                app = original_create_app(context)
                if not any(getattr(route, "path", None) == "/api/sync/notify" for route in app.routes):
                    app.include_router(create_relay_router(context))
                return app

            with patch.object(server_module, "create_app", side_effect=with_relay):
                server = server_module.make_server(canvas, port=0)
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            try:
                deadline = time.monotonic() + 5
                while not server.server.started and time.monotonic() < deadline:
                    time.sleep(0.01)
                self.assertTrue(server.server.started)
                env = {
                    **os.environ,
                    "CODEX_AGENTS_STATE_DIR": str(root),
                    "CODEX_CANVAS_URL": f"http://127.0.0.1:{server.server_port}",
                    "CODEX_BOARD_OWNER": "",
                    "CODEX_AGENT_OWNER": "",
                    "PATH": f"{Path(sys.executable).parent}:{os.environ.get('PATH', '')}",
                }
                command = subprocess.run(
                    [str(SCRIPTS / "codex-graph"), "agent", "--id", "orphan", "--name", "Bad", "--parent", "missing"],
                    env=env,
                    capture_output=True,
                    text=True,
                    timeout=15,
                )
                self.assertNotEqual(command.returncode, 0)
                self.assertIn("Register the parent agent first", command.stderr)
                with canvas.connect() as db:
                    stored = db.execute("SELECT 1 FROM graph_agents WHERE id='orphan'").fetchone()
                self.assertIsNone(stored)
                self.assertEqual(server._context.resource_hub()._revision, 0)
            finally:
                server.shutdown()
                thread.join(timeout=5)
                server.server_close()

    def test_source_and_api_state_mismatch_fails_after_commit_without_wrong_hub_event(self) -> None:
        sys.path.insert(0, str(SCRIPTS))
        from codex_canvas import Canvas
        import studio_api.app as app_module
        import studio_api.server as server_module
        from studio_api.sync.resources.relay.router import create_router as create_relay_router

        with tempfile.TemporaryDirectory(prefix="relay-mismatch-") as temporary:
            source_root = Path(temporary) / "source-state"
            api_root = Path(temporary) / "api-state"
            source_canvas = Canvas(source_root)
            api_canvas = Canvas(api_root)
            original_create_app = app_module.create_app

            def with_relay(context: ApiContext) -> ASGIApp:
                app = original_create_app(context)
                if not any(getattr(route, "path", None) == "/api/sync/notify" for route in app.routes):
                    app.include_router(create_relay_router(context))
                return app

            with patch.object(server_module, "create_app", side_effect=with_relay):
                server = server_module.make_server(api_canvas, port=0)
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            try:
                deadline = time.monotonic() + 5
                while not server.server.started and time.monotonic() < deadline:
                    time.sleep(0.01)
                self.assertTrue(server.server.started)
                before_revision = server._context.resource_hub()._revision
                env = {
                    **os.environ,
                    "CODEX_AGENTS_STATE_DIR": str(source_root),
                    "CODEX_CANVAS_URL": f"http://127.0.0.1:{server.server_port}",
                    "CODEX_BOARD_OWNER": "",
                    "CODEX_AGENT_OWNER": "",
                    "PATH": f"{Path(sys.executable).parent}:{os.environ.get('PATH', '')}",
                }
                command = subprocess.run(
                    [str(SCRIPTS / "codex-graph"), "agent", "--id", "source-agent", "--name", "Source"],
                    env=env,
                    capture_output=True,
                    text=True,
                    timeout=15,
                )
                self.assertNotEqual(command.returncode, 0)
                self.assertIn("Source change committed", command.stderr)
                self.assertIn("does not match Studio API state", command.stderr)
                self.assertIn("Reconnect the event stream", command.stderr)
                with source_canvas.connect() as db:
                    source_rows = db.execute("SELECT count(*) FROM graph_agents WHERE id='source-agent'").fetchone()[0]
                with api_canvas.connect() as db:
                    api_rows = db.execute("SELECT count(*) FROM graph_agents WHERE id='source-agent'").fetchone()[0]
                self.assertEqual(source_rows, 1)
                self.assertEqual(api_rows, 0)
                self.assertEqual(server._context.resource_hub()._revision, before_revision)
            finally:
                server.shutdown()
                thread.join(timeout=5)
                server.server_close()


if __name__ == "__main__":
    unittest.main()
