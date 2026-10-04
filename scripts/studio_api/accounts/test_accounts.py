"""Contract checks for the accounts API module."""

from __future__ import annotations

import unittest
import base64
import importlib.util
import json
import os
from pathlib import Path
import sys
import shutil
import tempfile
from typing import Protocol, cast
from uuid import uuid4
from unittest.mock import patch

from fastapi import FastAPI
from fastapi.exceptions import RequestValidationError
from fastapi.testclient import TestClient
from starlette.requests import Request
from starlette.responses import JSONResponse
from pydantic import TypeAdapter, ValidationError

from studio_api.models import ErrorResponse, JsonValue, ResponseModel
from .router import create_router
from .models import (
    Account,
    AccountsResponse,
    ClaudeOptions,
    ClaudeCommand,
    ClaudeSessionResponse,
    ClaudeSessionStateResponse,
    ModelCatalogResponse,
    ProjectWriteRequest,
    ResetRequest,
    UsageLimitsResponse,
)


class AccountsModelTests(unittest.TestCase):
    def test_account_contract_never_accepts_credential_material(self) -> None:
        good = Account.model_validate({
            "id": "default", "home": "/profiles/default", "label": "Codex",
            "source": "Codex CLI", "status": "ready", "accountId": "acct-1",
        })
        self.assertEqual(good.accountId, "acct-1")
        with self.assertRaises(ValidationError):
            Account.model_validate({
                "id": "default", "home": "/profiles/default", "label": "Codex",
                "source": "Codex CLI", "status": "ready", "accessToken": "secret",
            })

    def test_account_snapshot_has_explicit_collections(self) -> None:
        result = AccountsResponse.model_validate({
            "accounts": [], "archivedAccounts": [], "defaultAccountKey": "default",
            "logins": [], "supportsDisconnect": True, "supportsDelete": True,
        })
        self.assertEqual(result.defaultAccountKey, "default")
        with self.assertRaises(ValidationError):
            AccountsResponse.model_validate({"accounts": [], "defaultAccountKey": "default", "token": "secret"})

    def test_project_updates_keep_explicit_nulls(self) -> None:
        request = ProjectWriteRequest.model_validate({
            "action": "set_worker_base", "path": "/project", "base_ref": None,
            "expected_revision": 0,
        })
        self.assertIn("base_ref", request.model_dump(mode="json", exclude_unset=True))
        with self.assertRaises(ValidationError):
            ProjectWriteRequest.model_validate({"action": "unknown", "path": "/project"})

    def test_reset_requires_durable_uuid_before_dispatch(self) -> None:
        request = ResetRequest.model_validate({
            "account_id": "account-1", "credit_id": "credit-1", "request_id": str(uuid4()),
        })
        self.assertEqual(request.request_id, str(request.request_id))
        with self.assertRaises(ValidationError):
            ResetRequest.model_validate({
                "account_id": "account-1", "credit_id": "credit-1", "request_id": "bad-id",
            })

    def test_provider_catalog_names_are_open_strings(self) -> None:
        catalog = ModelCatalogResponse.model_validate({"data": [{"model": "vendor/new-model", "tier": "preview"}]})
        self.assertIsNotNone(catalog.data)
        if catalog.data is None:
            self.fail("catalog omitted its data list")
        self.assertEqual(catalog.data[0].model, "vendor/new-model")

    def test_usage_payload_is_explicitly_provider_owned(self) -> None:
        limits = UsageLimitsResponse.model_validate({
            "accountKey": "default", "data": {"accountId": "acct-1", "newField": [True, 1]},
            "at": 10.5, "error": None,
        })
        self.assertIsInstance(limits.data, dict)

    def test_claude_options_and_session_outputs_have_typed_shapes(self) -> None:
        options = ClaudeOptions.model_validate({
            "binaryPath": "claude", "customModels": [{"id": "model-x", "label": "Model X"}],
        })
        if options.customModels is None:
            self.fail("custom model list is missing")
        self.assertEqual(options.customModels[0].id, "model-x")
        state = ClaudeSessionStateResponse.model_validate({
            "settings": {"permissionMode": "default", "thinking": True},
            "turns": [{"id": "turn-1", "status": "completed", "text": "Earlier prompt"}],
            "tasks": [{"task_id": "task-1", "description": "Inspect files", "status": "running"}],
            "controlOperation": {"requestId": "request-1", "turnId": "turn-1", "phase": "provider_pending"},
            "version": "2.0", "contextWindow": 100000, "totalTokens": 30,
        })
        self.assertEqual(state.turns[0].id, "turn-1")
        command_list: list[ClaudeCommand] = TypeAdapter(list[ClaudeCommand]).validate_python([
            {"name": "review", "description": "Review changes", "argumentHint": "<path>", "builtin": False, "aliases": []},
        ])
        self.assertEqual(command_list[0].name, "review")


class _RuntimeFixture:
    def __init__(self, account_store: "_AccountStore") -> None:
        self.accounts = account_store
        self.limit_reads: list[str] = []
        self.catalog_reads: list[str] = []
        self.cached_limits: dict[str, dict[str, object]] = {
            "first": {"accountKey": "first", "data": {"accountId": "first"}, "at": 1.0, "error": None}
        }

    def accounts_snapshot(self) -> dict[str, object]:
        return cast(dict[str, object], self.accounts.snapshot())

    def rate_limits_for(self, key: str) -> dict[str, object]:
        return self.cached_limits.get(key, {"accountKey": key, "data": None, "at": None, "error": None})

    def limits(self, key: str) -> dict[str, object]:
        self.limit_reads.append(key)
        return {"accountKey": key, "data": {"accountId": key}, "at": 2.0, "error": None}

    def catalog(self, key: str) -> dict[str, object]:
        self.catalog_reads.append(key)
        return {"data": [{"model": "fixture/model"}]}


class _ContextFixture:
    def __init__(self, runtime: _RuntimeFixture) -> None:
        self.runtime = runtime

    def send(self, request: Request, value: JsonValue, status: int = 200, **_kwargs: object) -> JSONResponse:
        route = request.scope["route"]
        model_type = route.response_model
        validated = model_type.model_validate(value)
        content = validated.wire_dump() if isinstance(validated, ResponseModel) else validated.model_dump(mode="json")
        return JSONResponse(content=content, status_code=status)


class _AccountStore(Protocol):
    def snapshot(self) -> dict[str, JsonValue]: ...

    def register(self, home: str) -> str: ...


class AccountsRouterTests(unittest.TestCase):
    def setUp(self) -> None:
        self.cache = Path.home() / ".cache" / "codex-studio-fastapi" / "tests-tmp" / ("accounts-" + str(uuid4()))
        self.cache.mkdir(parents=True)
        self.tmp = tempfile.TemporaryDirectory(dir=self.cache)
        self.root = Path(self.tmp.name)
        self.home = self.root / "home"
        self.profile = self.home / ".codex"
        self.profile.mkdir(parents=True)
        self._write_auth(self.profile, "account-default")
        self.environment = patch.dict(os.environ, {"CODEX_HOME": str(self.profile)})
        self.environment.start()
        self.path_home = patch("pathlib.Path.home", return_value=self.home)
        self.path_home.start()
        self.claude = patch("codex_claude.installed", return_value=None)
        self.claude.start()
        from codex_accounts import AccountStore

        self.store = cast(_AccountStore, AccountStore(self.root / "state"))  # type: ignore[no-untyped-call]
        self.runtime = _RuntimeFixture(self.store)
        self.app = FastAPI()
        self.app.include_router(create_router(_ContextFixture(self.runtime)))

        @self.app.exception_handler(RequestValidationError)
        def request_error(_request: object, _error: RequestValidationError) -> JSONResponse:
            payload = ErrorResponse(error="Invalid request").wire_dump()
            return JSONResponse(status_code=400, content=payload)

        self.client = TestClient(self.app)

    def tearDown(self) -> None:
        self.claude.stop()
        self.path_home.stop()
        self.environment.stop()
        self.tmp.cleanup()
        shutil.rmtree(self.cache, ignore_errors=True)

    @staticmethod
    def _write_auth(home: Path, account_id: str) -> None:
        claims = {"https://api.openai.com/auth": {"chatgpt_account_id": account_id}}
        payload = base64.urlsafe_b64encode(json.dumps(claims).encode()).decode().rstrip("=")
        (home / "auth.json").write_text(json.dumps({
            "tokens": {"account_id": account_id, "id_token": "x." + payload + ".sig", "access_token": "fixture-secret"}
        }))

    def test_accounts_response_comes_from_account_store_and_hides_credentials(self) -> None:
        response = self.client.get("/api/accounts")
        self.assertEqual(response.status_code, 200)
        self.assertEqual(response.json()["accounts"][0]["accountId"], "account-default")
        self.assertNotIn("fixture-secret", response.text)

    def test_delete_route_preserves_exact_durable_request_id(self) -> None:
        second = self.root / "second-profile"
        second.mkdir()
        self._write_auth(second, "account-second")
        account_key = self.store.register(str(second))
        request_id = str(uuid4())
        body = {"account_key": account_key, "request_id": request_id}
        first = self.client.post("/api/accounts/delete", json=body)
        retry = self.client.post("/api/accounts/delete", json=body)
        self.assertEqual(first.status_code, 200)
        self.assertEqual(retry.json(), first.json())
        self.assertIn(account_key, [row["id"] for row in retry.json()["archivedAccounts"]])
        store_data = cast(dict[str, object], getattr(self.store, "data"))
        receipts = cast(dict[str, object], store_data["deleteReceipts"])
        receipt = cast(dict[str, object], receipts[request_id])
        self.assertEqual(receipt["accountKey"], account_key)

    def test_invalid_reset_uuid_is_rejected_before_service_call(self) -> None:
        from codex_limit_resets import consume_reset

        with patch("codex_limit_resets.consume_reset", wraps=consume_reset) as consume:
            response = self.client.post("/api/limits/reset", json={
                "account_id": "account-default", "credit_id": "credit-1", "request_id": "invalid",
            })
        self.assertEqual(response.status_code, 400)
        consume.assert_not_called()

    def test_legacy_first_nonempty_account_and_singleton_cached_behavior(self) -> None:
        cached = self.client.get("/api/limits?account_key=&account_key=first&account_key=second&cached=1")
        self.assertEqual(cached.status_code, 200)
        self.assertEqual(cached.json()["accountKey"], "first")
        self.assertEqual(self.runtime.limit_reads, [])
        repeated = self.client.get("/api/limits?account_key=first&cached=1&cached=1")
        self.assertEqual(repeated.status_code, 200)
        self.assertEqual(self.runtime.limit_reads, ["first"])

    def test_models_workers_flag_requires_single_value_and_query_schema_is_visible(self) -> None:
        with patch("codex_worker_accounts.catalog", return_value={"data": [{"model": "worker/model"}]}) as worker_catalog:
            repeated = self.client.get("/api/models?account_key=&account_key=first&account_key=second&workers=1&workers=1")
            self.assertEqual(repeated.status_code, 200)
            self.assertEqual(self.runtime.catalog_reads, ["first"])
            self.assertEqual(worker_catalog.call_count, 0)
            singleton = self.client.get("/api/models?account_key=first&workers=1")
            self.assertEqual(singleton.status_code, 200)
            worker_catalog.assert_called_once_with(self.runtime, "first")
        operation = self.app.openapi()["paths"]["/api/limits"]["get"]
        self.assertEqual({param["name"] for param in operation["parameters"]}, {"account_key", "cached"})
        self.assertIn("400", operation["responses"])
        self.assertIn("ErrorResponse", json.dumps(operation["responses"]["400"]))
        self.assertNotIn("content", operation["responses"]["422"])
        self.assertNotIn("HTTPValidationError", json.dumps(operation["responses"]))

    def test_catalog_pending_keeps_legacy_400_body(self) -> None:
        from codex_catalog import CatalogPending

        with patch.object(self.runtime, "catalog", side_effect=CatalogPending("Still loading")):
            response = self.client.get("/api/models")
        self.assertEqual(response.status_code, 400)
        self.assertEqual(response.json(), {"error": "Still loading", "catalogPending": True})

    def test_real_reset_producer_reuses_exact_request_after_retry(self) -> None:
        fixture_path = Path(__file__).resolve().parents[3] / "tests" / "runtime-accounts-contract.py"
        sys.path.insert(0, str(fixture_path.parent))
        spec = importlib.util.spec_from_file_location("account_reset_fixture", fixture_path)
        if spec is None or spec.loader is None:
            self.fail("could not load isolated reset fixture")
        fixture = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(fixture)
        isolated = fixture.AccountContracts()
        isolated.setUp()
        try:
            isolated.server = isolated.runtime.connect()
            isolated.server.billing_id = "billing-default"
            app = FastAPI()
            runtime = isolated.runtime
            app.include_router(create_router(_ContextFixture(runtime)))
            @app.exception_handler(RequestValidationError)
            def request_error(_request: object, _error: RequestValidationError) -> JSONResponse:
                return JSONResponse(status_code=400, content=ErrorResponse(error="Invalid request").wire_dump())

            client = TestClient(app)
            request_id = str(uuid4())
            payload = {
                "account_id": "billing-default", "credit_id": "same-credit", "request_id": request_id,
            }
            first = client.post("/api/limits/reset", json=payload)
            retry = client.post("/api/limits/reset", json=payload)
            self.assertEqual(first.status_code, 200, first.text)
            self.assertEqual(first.json()["request_id"], request_id)
            self.assertEqual(first.json()["outcome"], "reset")
            self.assertEqual(retry.json()["request_id"], request_id)
            self.assertEqual(retry.json()["outcome"], "reset")
            self.assertEqual(len(isolated.server.consumes), 1)
        finally:
            isolated.tearDown()


if __name__ == "__main__":
    unittest.main()
