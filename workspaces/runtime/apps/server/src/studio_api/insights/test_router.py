"""Route tests for analytics and cost estimates."""

from __future__ import annotations

import asyncio
from collections.abc import Generator
import json
import sqlite3
import tempfile
import threading
import time
import unittest
from pathlib import Path
from types import SimpleNamespace
from typing import BinaryIO, Callable, cast
from unittest.mock import patch

from fastapi import FastAPI, Request
from fastapi.exceptions import RequestValidationError
from fastapi.responses import JSONResponse, Response
from fastapi.testclient import TestClient
import httpx
import uvicorn

from studio_api.context import ApiContext
from codex_session_costs import SessionCostReader
from studio_api.insights.router import _stream_export, create_router
from studio_api.models import ErrorResponse, JsonValue, ResponseModel


class _Runtime:
    def __init__(self) -> None:
        self.analytics_calls: list[dict[str, str]] = []
        self.export_factory: Callable[..., Generator[bytes, None, None]] = self._valid_export
        self.export_closed = False
        self.invalid_response = False

    def analytics(self, **options: str) -> dict[str, JsonValue]:
        self.analytics_calls.append(options)
        if options.get("view") == "message-info":
            return {
                "at": 123.0,
                "tokens": {"outputTokens": 1200, "reasoningOutputTokens": 350},
                "turnDurationMs": 2500,
                "responseRate": 48.0,
            }
        if getattr(self, "invalid_response", False):
            return {"unexpected": "invalid output"}
        result: dict[str, JsonValue] = {
            "at": 123.0,
            "tokens": {"inputTokens": 7},
        }
        if options.get("timing") == "1":
            result["__serverTiming"] = {"analytics-read": 1.25}
        return result

    def _valid_export(self, **_options: str) -> Generator[bytes, None, None]:
        try:
            yield b'{"ok":'
            yield b"true}"
        finally:
            self.export_closed = True

    def analytics_export_chunks(self, **options: str) -> Generator[bytes, None, None]:
        return self.export_factory(**options)


class _CostReader:
    def __init__(self, context: _Context) -> None:
        self.context = context

    def snapshot(self, account_key: str) -> dict[str, JsonValue]:
        self.context.cost_snapshots.append(account_key)
        return {
            "at": None,
            "error": None,
            "data": None,
            "refreshing": False,
            "stale": True,
            "accountKey": account_key,
        }


class _SessionCostReader:
    def __init__(self, context: _Context) -> None:
        self.context = context

    def snapshot(self, agent_id: str) -> dict[str, JsonValue]:
        self.context.session_snapshots.append(agent_id)
        return {
            "rootId": agent_id,
            "totalUSD": None,
            "pricedSamples": 0,
            "breakdown": {"providers": {}, "models": {}},
            "unknownModels": [],
            "estimated": True,
            "pricingState": "loading",
            "cacheAgeSeconds": 0,
            "refreshing": True,
            "method": "Calculating the session estimate.",
        }


class _Context:
    def __init__(self) -> None:
        self.runtime = _Runtime()
        self.canvas = SimpleNamespace(root=Path("/state"))
        self.cost_snapshots: list[str] = []
        self.session_snapshots: list[str] = []
        self.session_reader: SessionCostReader | None = None

    def costs(self) -> _CostReader:
        return _CostReader(self)

    def session_costs(self) -> _SessionCostReader:
        return self.session_reader or _SessionCostReader(self)

    def send(
        self,
        request: Request,
        value: object,
        status: int = 200,
        *,
        content_type: str = "application/json",
        cache_control: str = "no-store",
        compressed: bool | None = None,
        etag: bool = False,
        weak_etag_fields: tuple[str, ...] = (),
        server_timing: dict[str, float] | None = None,
    ) -> Response:
        del content_type, compressed, etag, weak_etag_fields
        if status >= 400:
            payload = ErrorResponse.model_validate(value).wire_dump()
        else:
            route = request.scope["route"]
            response_type = cast(type[ResponseModel], route.response_model)
            payload = response_type.model_validate(value).wire_dump()
        headers = {"Cache-Control": cache_control}
        if server_timing:
            headers["Server-Timing"] = ", ".join(
                f"{name};dur={duration}" for name, duration in server_timing.items()
            )
        return JSONResponse(payload, status_code=status, headers=headers)


class InsightsRouterTests(unittest.TestCase):
    def setUp(self) -> None:
        self.context = _Context()
        self.app = FastAPI()
        self.app.include_router(create_router(cast(ApiContext, self.context)))

        @self.app.exception_handler(RequestValidationError)
        async def invalid_request(_request: Request, error: RequestValidationError) -> Response:
            return JSONResponse(
                ErrorResponse(error=str(error)).wire_dump(), status_code=400
            )

        self.client = TestClient(self.app, raise_server_exceptions=False)

    def test_actual_cost_snapshot_keeps_the_fingerprint_private_on_http(self) -> None:
        from codex_costs import AccountCostReader as UntypedAccountCostReader
        from codex_costs import CostReader as UntypedCostReader
        from studio_api.insights.models import AccountCostResponse
        from pydantic import ValidationError

        now = time.time()
        report = [{
            "provider": "codex", "source": "local", "currencyCode": "USD",
            "sessionCostUSD": 12.5, "last30DaysCostUSD": 100.25,
            "sessionTokens": 1000, "last30DaysTokens": 4000,
            "historyCoverageIsEstablished": True, "coverage": {"priced": 1, "unpriced": 0},
            "daily": [{"date": time.strftime("%Y-%m-%d", time.localtime(now)), "modelsUsed": ["fixture"],
                       "modelBreakdowns": [{"modelName": "fixture", "cost": 12.5, "totalTokens": 1000}]}],
        }]

        def scanner(_command: object, **options: object) -> object:
            cast(BinaryIO, options["stdout"]).write(json.dumps(report).encode())
            return SimpleNamespace(wait=lambda **_kwargs: 0)

        with tempfile.TemporaryDirectory(prefix="studio-cost-response-") as folder:
            root = Path(folder)
            make_reader = cast(Callable[..., object], UntypedCostReader)
            reader = make_reader(root, command=lambda: ["fixture-scanner"], clock=lambda: now)
            close = cast(Callable[[], None], getattr(reader, "close"))
            self.addCleanup(close)
            with patch("codex_costs.subprocess.Popen", side_effect=scanner):
                cast(Callable[[], None], getattr(reader, "_refresh"))()
            state = cast(dict[str, JsonValue], getattr(reader, "state"))
            fingerprint = state["sourceFingerprint"]
            self.assertIsInstance(fingerprint, str)
            accounts = SimpleNamespace(get=lambda _key: {
                "status": "ready", "provider": "codex", "accountId": "account:fixture", "home": str(root),
            })
            make_accounts = cast(Callable[..., object], UntypedAccountCostReader)
            account_reader = make_accounts(root, accounts, reader_factory=lambda *_args, **_kwargs: reader,
                                           pricing=SimpleNamespace())
            value = cast(Callable[[str], dict[str, JsonValue]], getattr(account_reader, "snapshot"))("fixture")
            self.assertEqual(value["sourceFingerprint"], fingerprint)
            expected = {key: item for key, item in value.items() if key != "sourceFingerprint"}
            self.assertEqual(AccountCostResponse.model_validate(value).wire_dump(), expected)
            context = ApiContext.for_schema()
            setattr(context, "costs", lambda: account_reader)
            app = FastAPI()
            app.include_router(create_router(context))
            with patch("studio_api.context.logging.Logger.error") as error_log:
                response = TestClient(app).get("/api/costs?account_key=fixture")
            self.assertEqual(response.status_code, 200)
            self.assertEqual(response.json(), expected)
            self.assertEqual(state["sourceFingerprint"], fingerprint)
            self.assertEqual(response.json()["data"]["todayUSD"], 12.5)
            error_log.assert_not_called()
            for field, invalid in (("sourceFingerprint", 1), ("unknown", True)):
                with self.subTest(field=field), self.assertRaises(ValidationError):
                    AccountCostResponse.model_validate({**value, field: invalid})
            close()

    def test_openapi_uses_the_typed_query_models(self) -> None:
        openapi = self.client.get("/openapi.json").json()
        paths = openapi["paths"]
        params = {
            (parameter["name"], parameter["in"])
            for parameter in paths["/api/analytics"]["get"]["parameters"]
        }
        self.assertIn(("scope", "query"), params)
        self.assertIn(("from", "query"), params)
        self.assertIn(("export", "query"), params)
        scope_schema = next(
            parameter["schema"]
            for parameter in paths["/api/analytics"]["get"]["parameters"]
            if parameter["name"] == "scope"
        )
        self.assertEqual(scope_schema["enum"], ["agent", "team", "all"])
        self.assertIn(("agent", "query"), {
            (parameter["name"], parameter["in"])
            for parameter in paths["/api/session-cost"]["get"]["parameters"]
        })

    def test_openapi_uses_producer_backed_analytics_record_dtos(self) -> None:
        openapi = self.client.get("/openapi.json").json()
        response_schema = openapi["paths"]["/api/analytics"]["get"]["responses"]["200"]["content"]["application/json"]["schema"]
        self.assertEqual(response_schema["$ref"], "#/components/schemas/AnalyticsResponse")
        response_model = openapi["components"]["schemas"]["AnalyticsResponse"]

        def item_model(name: str) -> str:
            field = response_model["properties"][name]
            array_schema = next(variant for variant in field["anyOf"] if variant.get("type") == "array")
            items = array_schema.get("items")
            if not isinstance(items, dict):
                raise AssertionError(f"{name} schema has no typed array item")
            reference = items.get("$ref")
            if not isinstance(reference, str):
                raise AssertionError(f"{name} schema has no model reference")
            return reference.rsplit("/", 1)[-1]

        self.assertEqual(item_model("timeline"), "AnalyticsUsageRecord")
        self.assertEqual(item_model("chartBuckets"), "AnalyticsUsageRecord")
        self.assertEqual(item_model("calls"), "AnalyticsItemRecord")
        self.assertEqual(item_model("turns"), "AnalyticsTurnRecord")
        self.assertEqual(item_model("rateLimits"), "AnalyticsRateLimitRecord")
        rate_schema = response_model["properties"]["responseRate"]
        self.assertIn(
            {"type": "number"},
            [variant for variant in rate_schema["anyOf"] if variant.get("type")],
        )

    def test_routes_validate_through_the_real_api_context_sender(self) -> None:
        context = ApiContext.for_schema()
        runtime = _Runtime()
        setattr(context.canvas, "runtime", runtime)
        setattr(context, "costs", lambda: _CostReader(self.context))
        app = FastAPI()
        app.include_router(create_router(context))
        client = TestClient(app, raise_server_exceptions=False)

        analytics = client.get("/api/analytics?timing=1")
        costs = client.get("/api/costs")
        message_info = client.get(
            "/api/analytics?agent=agent-a&view=message-info&item=item-a&turn=turn-a"
        )

        self.assertEqual(analytics.status_code, 200)
        self.assertEqual(analytics.json(), {"at": 123.0, "tokens": {"inputTokens": 7}})
        self.assertIn("Server-Timing", analytics.headers)
        self.assertEqual(costs.status_code, 200)
        self.assertIsNone(costs.json()["data"])
        self.assertEqual(message_info.status_code, 200)
        self.assertEqual(
            message_info.json(),
            {
                "at": 123.0,
                "tokens": {"outputTokens": 1200, "reasoningOutputTokens": 350},
                "turnDurationMs": 2500,
                "responseRate": 48.0,
            },
        )

    def test_analytics_keeps_first_query_value_and_timing_header(self) -> None:
        response = self.client.get(
            "/api/analytics?scope=team&scope=all&from=10.5&timing=1"
        )

        self.assertEqual(response.status_code, 200)
        self.assertEqual(
            self.context.runtime.analytics_calls,
            [{"scope": "team", "from": "10.5", "timing": "1"}],
        )
        self.assertEqual(response.json(), {"at": 123.0, "tokens": {"inputTokens": 7}})
        self.assertIn("Server-Timing", response.headers)

    def test_invalid_analytics_enum_does_not_call_runtime(self) -> None:
        response = self.client.get("/api/analytics?scope=unknown")

        self.assertEqual(response.status_code, 400)
        self.assertEqual(response.json()["error"], "Unknown analytics scope")
        self.assertEqual(self.context.runtime.analytics_calls, [])

    def test_legacy_non_trigger_values_remain_ordinary_analytics_options(self) -> None:
        response = self.client.get(
            "/api/analytics?export=other&timing=other&view=unknown"
        )

        self.assertEqual(response.status_code, 200)
        self.assertEqual(
            self.context.runtime.analytics_calls,
            [{"export": "other", "timing": "other", "view": "unknown"}],
        )
        self.assertNotIn("Server-Timing", response.headers)

    def test_response_validation_failure_is_server_error_after_one_service_call(self) -> None:
        self.context.runtime.invalid_response = True

        response = self.client.get("/api/analytics")

        self.assertEqual(response.status_code, 500)
        self.assertEqual(len(self.context.runtime.analytics_calls), 1)

    def test_cost_and_session_cost_retain_nulls_and_validate_agent_first(self) -> None:
        cost = self.client.get(
            "/api/costs?account_key=team-a&account_key=team-b&legacyUnknown=ignored"
        )
        invalid_session = self.client.get("/api/session-cost?agent=not%20valid")
        session = self.client.get("/api/session-cost?agent=agent_123")

        self.assertEqual(cost.status_code, 200)
        self.assertIsNone(cost.json()["data"])
        self.assertEqual(self.context.cost_snapshots, ["team-a"])
        self.assertEqual(invalid_session.status_code, 400)
        self.assertEqual(self.context.session_snapshots, ["agent_123"])
        self.assertEqual(session.json()["pricingState"], "loading")

    def test_session_cost_http_returns_cached_values_after_failed_refresh(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            db_path = root / "canvas.sqlite3"
            with sqlite3.connect(db_path) as db:
                db.executescript("""
                  CREATE TABLE analytics_agents (id TEXT PRIMARY KEY, record TEXT NOT NULL);
                  CREATE TABLE runtime_agents (id TEXT PRIMARY KEY, record TEXT NOT NULL);
                  CREATE TABLE analytics_usage (seq INTEGER PRIMARY KEY, agent TEXT, root TEXT,
                    thread TEXT, turn TEXT, at REAL, record TEXT);
                  CREATE TABLE analytics_usage_roots (root TEXT PRIMARY KEY, generation INTEGER NOT NULL);
                  CREATE TABLE analytics_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                """)
                db.execute("INSERT INTO analytics_agents VALUES (?, ?)",
                           ("lead", json.dumps({"rootId": "lead"})))

            class _Pricing:
                @staticmethod
                def snapshot() -> dict[str, object]:
                    return {"version": 1}

            now = 100.0
            reader = SessionCostReader(db_path, _Pricing(), state_root=root, clock=lambda: now)
            result = {
                "rootId": "lead", "totalUSD": 7.5, "pricedSamples": 1,
                "breakdown": {"providers": {}, "models": {}}, "unknownModels": [],
                "estimated": True, "pricingState": "ready", "method": "cached estimate",
            }
            reader.cache["lead"] = (90.0, result, {})
            reader.__dict__.setdefault("refresh_checks", {})["lead"] = now
            reader.__dict__.setdefault("refresh_errors", {})["lead"] = ValueError(
                "source temporarily unavailable"
            )
            self.context.session_reader = reader

            response = self.client.get("/api/session-cost?agent=lead")

        self.assertEqual(response.status_code, 200)
        self.assertEqual(response.json()["totalUSD"], 7.5)
        self.assertFalse(response.json()["refreshing"])

    def test_removed_disk_route_does_not_scan_worktrees(self) -> None:
        with (
            patch("os.scandir", side_effect=AssertionError("Unexpected folder scan")),
            patch("os.walk", side_effect=AssertionError("Unexpected folder walk")),
        ):
            response = self.client.get("/api/worktree-disk?workers=worker-a,worker-b")
        self.assertEqual(response.status_code, 404)

    def test_export_stream_preserves_legacy_attachment_headers_and_json(self) -> None:
        response = self.client.get("/api/analytics?export=1")

        self.assertEqual(response.status_code, 200)
        self.assertEqual(response.content, b'{"ok":true}')
        self.assertEqual(
            response.headers["content-disposition"],
            'attachment; filename="codex-studio-analytics.json"',
        )
        self.assertEqual(response.headers["content-type"], "application/json; charset=utf-8")
        self.assertEqual(response.headers["cache-control"], "no-store")
        self.assertEqual(response.headers["x-content-type-options"], "nosniff")
        self.assertTrue(self.context.runtime.export_closed)

    def test_export_closes_after_late_error_and_on_async_cancel(self) -> None:
        def late_failure() -> Generator[bytes, None, None]:
            try:
                yield b"{"
                raise ValueError("read failed")
            finally:
                self.context.runtime.export_closed = True

        self.context.runtime.export_factory = lambda **_options: late_failure()
        chunks = self.context.runtime.analytics_export_chunks()
        first = next(chunks)
        body = _stream_export(first, chunks)

        async def consume_failure() -> None:
            self.assertEqual(await body.__anext__(), b"{")
            with self.assertRaisesRegex(ValueError, "read failed"):
                await body.__anext__()

        asyncio.run(consume_failure())
        self.assertTrue(self.context.runtime.export_closed)

        class Closable:
            closed = False

            def __iter__(self) -> Closable:
                return self

            def __next__(self) -> bytes:
                return b"second"

            def close(self) -> None:
                self.closed = True

        closable = Closable()
        body = _stream_export(b"first", cast(Generator[bytes, None, None], closable))

        async def cancel_after_first_chunk() -> None:
            self.assertEqual(await body.__anext__(), b"first")
            await body.aclose()

        asyncio.run(cancel_after_first_chunk())
        self.assertTrue(closable.closed)

    def test_late_export_failure_breaks_the_http_transfer(self) -> None:
        context = _Context()

        def late_failure() -> Generator[bytes, None, None]:
            try:
                yield b'{"partial":'
                raise ValueError("read failed")
            finally:
                context.runtime.export_closed = True

        context.runtime.export_factory = lambda **_options: late_failure()
        app = FastAPI()
        app.include_router(create_router(cast(ApiContext, context)))
        server = uvicorn.Server(
            uvicorn.Config(app, host="127.0.0.1", port=0, log_level="critical", lifespan="off")
        )
        thread = threading.Thread(target=server.run, daemon=True)
        thread.start()
        try:
            deadline = time.monotonic() + 5
            while not server.started and thread.is_alive() and time.monotonic() < deadline:
                time.sleep(0.01)
            self.assertTrue(server.started, "loopback ASGI server did not start")
            self.assertTrue(server.servers)
            sockets = server.servers[0].sockets
            self.assertTrue(sockets)
            port = sockets[0].getsockname()[1]

            received: list[bytes] = []
            with httpx.Client(timeout=5) as client:
                with client.stream("GET", f"http://127.0.0.1:{port}/api/analytics?export=1") as response:
                    self.assertEqual(response.status_code, 200)
                    with self.assertRaises(httpx.RemoteProtocolError):
                        received.extend(response.iter_bytes())

            self.assertEqual(b"".join(received), b'{"partial":')
            self.assertTrue(context.runtime.export_closed)
        finally:
            server.should_exit = True
            thread.join(timeout=5)
        self.assertFalse(thread.is_alive(), "loopback ASGI server failed to stop")

    def test_export_failure_before_first_chunk_returns_error_before_headers(self) -> None:
        def early_failure() -> Generator[bytes, None, None]:
            try:
                raise RuntimeError("database unavailable")
                yield b"unreachable"
            finally:
                self.context.runtime.export_closed = True

        self.context.runtime.export_factory = lambda **_options: early_failure()
        response = self.client.get("/api/analytics?export=1")

        self.assertEqual(response.status_code, 400)
        self.assertEqual(response.json()["error"], "database unavailable")
        self.assertEqual(response.headers.get("content-disposition"), None)
        self.assertTrue(self.context.runtime.export_closed)


if __name__ == "__main__":
    unittest.main()
