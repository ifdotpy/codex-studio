"""Unittest coverage for the I/O HTTP contracts and ranged log reads."""
from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path

from fastapi import FastAPI, Request
from fastapi.exceptions import RequestValidationError
from fastapi.responses import JSONResponse, Response
from fastapi.testclient import TestClient
from pydantic import TypeAdapter, ValidationError

from studio_api.io.router import create_router
from studio_api.models import JsonValue


class TerminalFixture:
    def __init__(self) -> None:
        self.calls: list[tuple[str, object]] = []

    def listing(self) -> dict[str, JsonValue]:
        return {"items": []}

    def output(self, key: str | None, offset: str | None) -> dict[str, JsonValue]:
        self.calls.append(("output", (key, offset)))
        return {"text": "tail", "offset": 4, "truncated": False, "status": "running", "exitCode": None, "error": None}

    def history_output(self, key: str | None, offset: str | None, limit: str | None) -> dict[str, JsonValue]:
        self.calls.append(("history", (key, offset, limit)))
        return {"text": "archived", "offset": 8, "availableOffset": 8, "historyStart": 0,
                "hasMore": False, "truncated": False, "status": "exited", "exitCode": 0, "error": None}

    def create(self, runtime: object, data: dict[str, JsonValue]) -> dict[str, JsonValue]:
        self.calls.append(("create", data))
        return {"id": data["id"], "agent": data["agent"], "title": "Terminal", "cwd": "/workspace",
                "status": "running", "created": 1.0, "updated": 1.0, "exitCode": None}

    def action(self, action: str, data: dict[str, JsonValue]) -> dict[str, JsonValue]:
        self.calls.append((action, data))
        if action == "input":
            return {"ok": True, "delivery": "sent"}
        return {"id": data["id"], "agent": "a", "title": "Terminal", "cwd": "/workspace",
                "status": "running", "created": 1.0, "updated": 1.0, "exitCode": None}


class RuntimeFixture:
    def __init__(self, log_path: Path) -> None:
        self.log_path = log_path
        self.calls: list[tuple[str, object]] = []

    def monitor_log(self, key: str) -> dict[str, JsonValue]:
        return {"path": self.log_path, "fallback": None, "size": self.log_path.stat().st_size,
                "name": key + ".log", "mime": "text/plain", "truncated": True}

    def file_info(self, agent: str | None, path: str | None, asset: str | None) -> dict[str, JsonValue]:
        self.calls.append(("file_info", (agent, path, asset)))
        return {"path": path or "/asset", "name": "sample.txt", "mime": "text/plain", "size": 3}

    def file_content(self, agent: str | None, path: str | None, asset: str | None) -> tuple[bytes, str, str]:
        self.calls.append(("file_content", (agent, path, asset)))
        return b"abc", "text/plain", "sample.txt"

    def monitor_input(self, key: str, data: dict[str, JsonValue]) -> dict[str, JsonValue]:
        self.calls.append(("monitor_input", (key, data)))
        return {"processId": key}

    def cancel_monitor(self, key: str) -> dict[str, JsonValue]:
        self.calls.append(("cancel_monitor", key))
        return {"id": key, "status": "running", "cancelRequested": True}

    def upload_asset(self, data: dict[str, JsonValue]) -> dict[str, JsonValue]:
        self.calls.append(("upload_asset", data))
        return {"id": data.get("id") or "generated", "agent": data["agent"], "name": data["name"],
                "mime": "text/plain", "image": False, "size": 3, "hash": "digest", "created": 1.0}


class ContextFixture:
    def __init__(self, runtime: RuntimeFixture) -> None:
        self.runtime = runtime
        self._terminals = TerminalFixture()

    def terminals(self) -> TerminalFixture:
        return self._terminals

    @property
    def canvas(self) -> object:
        return object()

    def send(self, request: Request, value: object, **kwargs: object) -> Response:
        response_type = request.scope["route"].response_model
        if response_type:
            adapter = TypeAdapter(response_type)
            validated = adapter.validate_json(json.dumps(value))
            value = adapter.dump_python(validated, mode="json", exclude_none=True)
        return JSONResponse(value, status_code=int(kwargs.get("status", 200)))


def make_app(runtime: RuntimeFixture) -> tuple[FastAPI, ContextFixture]:
    context = ContextFixture(runtime)
    app = FastAPI()
    app.include_router(create_router(context))

    @app.exception_handler(RequestValidationError)
    async def validation_error(_: Request, error: RequestValidationError) -> JSONResponse:
        # Match the legacy boundary: input validation is a 400, with no raw
        # submitted values reflected in the error response.
        return JSONResponse({"error": "Invalid request"}, status_code=400)

    @app.exception_handler(ValidationError)
    async def response_validation_error(_: Request, error: ValidationError) -> JSONResponse:
        return JSONResponse({"error": "Invalid response"}, status_code=500)

    return app, context


class IORouterTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.log = Path(self.temp.name) / "watch.log"
        self.log.write_bytes(b"0123456789")
        self.runtime = RuntimeFixture(self.log)
        self.app, self.context = make_app(self.runtime)
        self.client = TestClient(self.app)

    def tearDown(self) -> None:
        self.client.close()
        self.temp.cleanup()

    def test_terminal_query_modes_preserve_parse_qs_first_value_and_blank_rules(self) -> None:
        live = self.client.get("/api/terminals/output?id=&id=term&offset=&offset=7")
        self.assertEqual(live.status_code, 200)
        self.assertEqual(self.context.terminals().calls[-1], ("output", ("term", "7")))
        history = self.client.get("/api/terminals/output?id=term&history=&history=1&offset=3")
        self.assertEqual(history.status_code, 200)
        self.assertEqual(self.context.terminals().calls[-1], ("history", ("term", "3", "65536")))
        not_history = self.client.get("/api/terminals/output?id=term&history=1&history=1")
        self.assertEqual(not_history.status_code, 200)
        self.assertEqual(self.context.terminals().calls[-1][0], "output")

    def test_query_models_are_published_to_openapi(self) -> None:
        paths = self.app.openapi()["paths"]
        terminal_names = {item["name"] for item in paths["/api/terminals/output"]["get"]["parameters"]}
        file_names = {item["name"] for item in paths["/api/file"]["get"]["parameters"]}
        self.assertTrue({"id", "offset", "history", "limit"}.issubset(terminal_names))
        self.assertEqual(file_names, {"agent", "path", "asset"})

    def test_terminal_input_keeps_durable_request_identity(self) -> None:
        response = self.client.post("/api/terminals/input", json={"id": "term", "text": "go", "request_id": "durable-1"})
        self.assertEqual(response.json(), {"ok": True, "delivery": "sent"})
        self.assertEqual(self.context.terminals().calls[-1], ("input", {"id": "term", "text": "go", "request_id": "durable-1"}))

    def test_invalid_terminal_input_fails_before_manager_call(self) -> None:
        response = self.client.post("/api/terminals/input", json={"id": "term", "text": "é" * 32769, "request_id": "durable-1"})
        self.assertEqual(response.status_code, 400)
        self.assertFalse(self.context.terminals().calls)

    def test_monitor_log_stream_supports_ranges_and_rejects_bad_ranges(self) -> None:
        whole = self.client.get("/api/monitor/log?id=watch-1")
        self.assertEqual(whole.status_code, 200)
        self.assertEqual(whole.content, b"0123456789")
        self.assertEqual(whole.headers["content-disposition"], 'attachment; filename="watch-1.log"')
        part = self.client.get("/api/monitor/log?id=watch-1", headers={"Range": "bytes=2-5"})
        self.assertEqual(part.status_code, 206)
        self.assertEqual(part.content, b"2345")
        self.assertEqual(part.headers["content-range"], "bytes 2-5/10")
        invalid = self.client.get("/api/monitor/log?id=watch-1", headers={"Range": "bytes=-"})
        self.assertEqual(invalid.status_code, 416)
        self.assertEqual(invalid.headers["content-range"], "bytes */10")

    def test_file_query_and_base64_wire_shape(self) -> None:
        result = self.client.get("/api/file?agent=first&agent=second&path=&path=notes.txt")
        self.assertEqual(result.status_code, 200)
        self.assertEqual(result.json(), {"name": "sample.txt", "mime": "text/plain", "base64": "YWJj"})
        self.assertEqual(self.runtime.calls[-1], ("file_content", ("first", "notes.txt", None)))

    def test_monitor_input_forwards_provider_result_and_original_fields(self) -> None:
        response = self.client.post("/api/monitor/input", json={"id": "watch-1", "text": "x", "closeStdin": True})
        self.assertEqual(response.status_code, 200)
        self.assertEqual(response.json(), {"processId": "watch-1"})
        self.assertEqual(self.runtime.calls[-1], ("monitor_input", ("watch-1", {"id": "watch-1", "text": "x", "closeStdin": True})))

    def test_invalid_monitor_input_fails_before_runtime_call(self) -> None:
        response = self.client.post("/api/monitor/input", json={"id": "watch-1", "text": "x" * 32001})
        self.assertEqual(response.status_code, 400)
        self.assertFalse(self.runtime.calls)

    def test_monitor_cancel_response_uses_existing_receipt_shape(self) -> None:
        response = self.client.post("/api/monitor/cancel", json={"id": "watch-1"})
        self.assertEqual(response.status_code, 200)
        self.assertEqual(response.json(), {"id": "watch-1", "status": "running", "cancelRequested": True})

    def test_upload_passes_omitted_fields_without_fabricating_request_values(self) -> None:
        response = self.client.post("/api/assets", json={"agent": "agent-1", "name": "a.txt", "data": "YWJj"})
        self.assertEqual(response.status_code, 200)
        self.assertEqual(self.runtime.calls[-1], ("upload_asset", {"agent": "agent-1", "name": "a.txt", "data": "YWJj"}))


if __name__ == "__main__":
    unittest.main()
