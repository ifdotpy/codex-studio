"""Route tests for the analytics/cost/disk vertical slice."""

from __future__ import annotations

import asyncio
from collections.abc import Generator
import unittest
from pathlib import Path
from types import SimpleNamespace
from typing import Callable, cast
from unittest.mock import patch

from fastapi import FastAPI, Request
from fastapi.exceptions import RequestValidationError
from fastapi.responses import JSONResponse, Response
from fastapi.testclient import TestClient

from studio_api.context import ApiContext
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

    def costs(self) -> _CostReader:
        return _CostReader(self)

    def session_costs(self) -> _SessionCostReader:
        return _SessionCostReader(self)

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

    def test_openapi_uses_the_typed_query_models(self) -> None:
        paths = self.client.get("/openapi.json").json()["paths"]
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

    def test_disk_query_is_validated_before_scanner_side_effect(self) -> None:
        scanner = SimpleNamespace(
            snapshot=lambda ids: {
                "workers": {key: {"state": "ready", "bytes": 3, "measure": "allocated blocks"} for key in ids},
                "totalBytes": 3,
                "limitBytes": 100,
                "warning": False,
                "scanning": False,
                "error": None,
                "measure": "allocated blocks",
            }
        )
        with patch("codex_worktree_disk.scanner", return_value=scanner) as get_scanner:
            response = self.client.get("/api/worktree-disk?workers=worker-a,worker-b")

        self.assertEqual(response.status_code, 200)
        self.assertEqual(set(response.json()["workers"]), {"worker-a", "worker-b"})
        get_scanner.assert_called_once_with(Path("/state"))

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
        response = self.client.get("/api/analytics?export=1")
        self.assertEqual(response.status_code, 200)
        self.assertEqual(response.content, b"{")
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
        self.assertEqual(asyncio.run(body.__anext__()), b"first")
        asyncio.run(body.aclose())
        self.assertTrue(closable.closed)

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
