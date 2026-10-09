"""Contract checks for the accounts API module."""

from __future__ import annotations

import unittest
import base64
import asyncio
import importlib.util
import json
import os
from pathlib import Path
import sys
import shutil
import tempfile
import threading
from typing import Callable, Protocol, cast
from uuid import uuid4
from unittest.mock import patch

from fastapi import FastAPI
from fastapi.exceptions import RequestValidationError
from fastapi.testclient import TestClient
from starlette.requests import Request
from starlette.responses import JSONResponse
from pydantic import TypeAdapter, ValidationError
from studio_api.sync.resources.hub import (
    ResourceHub,
    register_resource_hub,
    unregister_resource_hub,
)
from studio_api.sync.resources.models import AccountsResource, ResourceRef
from studio_api.accounts.events import (
    publish_account_change as publish_account_change_to_hub,
)

from studio_api.context import ApiContext
from studio_api.models import ContractModel, ErrorResponse, JsonValue, ResponseModel
from .router import create_router
from .models import (
    Account,
    AccountsResponse,
    ClaudeOptions,
    ClaudeCommand,
    ClaudeDeliveryResponse,
    ClaudeSessionResponse,
    ClaudeSessionStateResponse,
    ModelCatalogResponse,
    PeerTeamConvertRequest,
    PeerTeamRequest,
    PeerTeamsResponse,
    PeerRadioResponse,
    ProjectWriteRequest,
    ResetRequest,
    UsageLimitsResponse,
    RateLimitBucket,
)


class AccountsModelTests(unittest.TestCase):
    def test_limits_accept_the_actual_notification_timestamp_and_cached_route(self) -> None:
        from contextlib import closing, nullcontext
        import sqlite3
        from types import SimpleNamespace
        from codex_runtime import Runtime

        rate_limits_for = cast(Callable[..., dict[str, JsonValue]], Runtime.rate_limits_for)
        store = cast(Callable[..., bool], Runtime.store_rate_limits)
        notification = cast(Callable[..., None], Runtime.notification)
        with closing(sqlite3.connect(":memory:")) as db:
            runtime = SimpleNamespace(
                rate_limits={"accountKey": "default", "data": None, "at": None, "error": None},
                rate_limits_by_account={}, connection_current=lambda *_args: True,
                db=lambda: nullcontext(db), analytics_safe=lambda *_args: None,
                analytics_limit=lambda *_args: None, usage_resume_limits_changed=lambda *_args: None,
                _stage_resource_change=lambda *_args: None,
            )
            runtime.rate_limits_for = lambda key: rate_limits_for(runtime, key)
            runtime.store_rate_limits = lambda key, value: store(runtime, key, value)
            runtime.limits = lambda *_args: self.fail("Cached limits must not start a native read")
            with patch("codex_runtime.consume_native_notification", return_value=False), \
                    patch("codex_sync_entities.patch"), patch("codex_runtime.time.time", return_value=10.0):
                notification(runtime, {
                    "method": "account/rateLimits/updated", "_studioReceivedAt": 5.0,
                    "params": {"rateLimits": {"limitId": "codex", "primary": {"usedPercent": 27.0}}},
                }, "fixture-account")
            value = rate_limits_for(runtime, "fixture-account")
            validated = UsageLimitsResponse.model_validate(value)
            self.assertEqual(validated.processedAt, 10.0)
            self.assertEqual(validated.at, 5.0)
            context = ApiContext.for_schema()
            setattr(context.canvas, "runtime", runtime)
            app = FastAPI()
            app.include_router(create_router(context))
            with patch("studio_api.context.logging.Logger.error") as error_log:
                response = TestClient(app).get("/api/limits?account_key=fixture-account&cached=1")
            self.assertEqual(response.status_code, 200)
            self.assertEqual(response.json(), value)
            error_log.assert_not_called()
            for field, invalid in (("processedAt", "10"), ("unknown", True)):
                with self.subTest(field=field), self.assertRaises(ValidationError):
                    UsageLimitsResponse.model_validate({**value, field: invalid})

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

    def test_usage_payload_has_explicit_internal_shape_and_provider_extensions(self) -> None:
        limits = UsageLimitsResponse.model_validate({
            "accountKey": "default", "data": {
                "accountId": "acct-1", "ordinaryUsageAllowed": True,
                "rateLimits": {
                    "limitId": "codex", "limitName": "Codex",
                    "primary": {"usedPercent": 42, "resetsAt": 2000000000, "windowDurationMins": 300},
                    "individualLimit": {"remainingPercent": 15}, "spendControlReached": False,
                },
                "rateLimitsByLimitId": {"codex": {"limitId": "codex"}},
                "rateLimitResetCredits": {
                    "availableCount": 1,
                    "credits": [{"id": "credit-1", "status": "available", "resetType": "codexRateLimits", "expiresAt": 2000000000}],
                },
                "providerExtension": {"newField": [True, 1]},
            },
            "at": 10.5, "error": None,
        })
        if (limits.data is None or limits.data.rateLimits is None
                or limits.data.rateLimits.primary is None
                or limits.data.rateLimits.individualLimit is None
                or limits.data.rateLimitResetCredits is None):
            self.fail("limits omitted a typed rate bucket")
        self.assertEqual(limits.data.rateLimits.primary.usedPercent, 42)
        self.assertEqual(limits.data.rateLimits.individualLimit.remainingPercent, 15)
        self.assertEqual(limits.data.rateLimitResetCredits.credits[0].status, "available")
        self.assertIn("providerExtension", limits.data.__pydantic_extra__ or {})
        with self.assertRaises(ValidationError):
            RateLimitBucket.model_validate({"limitId": "codex", "primary": {"usedPercent": "100"}})

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
        state_schema = ClaudeSessionStateResponse.model_json_schema()
        self.assertIn("turns", state_schema.get("required", []))
        command_list: list[ClaudeCommand] = TypeAdapter(list[ClaudeCommand]).validate_python([
            {"name": "review", "description": "Review changes", "argumentHint": "<path>", "builtin": False, "aliases": []},
        ])
        self.assertEqual(command_list[0].name, "review")

    def test_openapi_exposes_typed_session_and_limits_success_contracts(self) -> None:
        app = FastAPI()
        app.include_router(create_router(ApiContext.for_schema()))
        schemas = app.openapi()["components"]["schemas"]
        state = schemas["ClaudeSessionStateResponse"]
        self.assertIn("turns", state["required"])
        conversion = schemas["PeerTeamConvertRequest"]
        self.assertIn("target", conversion["required"])
        self.assertNotIn("team_id", conversion["properties"])
        limits_data = schemas["UsageLimitsResponse"]["properties"]["data"]
        self.assertIn("UsageLimitsData", json.dumps(limits_data))
        self.assertIn("RateLimitBucket", json.dumps(schemas["UsageLimitsData"]))

    def test_peer_team_radio_union_accepts_legacy_callers_and_types_room(self) -> None:
        for body in (
            {
                "action": "radio", "radio_action": "create", "request_id": "radio-create",
                "path": "/project", "name": "Shared chat", "participants": [
                    {"account_key": "one", "model": "gpt-6-luna"},
                    {"account_key": "two", "model": "claude-sonnet", "effort": "high"},
                ],
            },
            {
                "action": "radio", "radio_action": "open", "request_id": "radio-open",
                "path": "/project", "team_id": "team-id",
            },
            {
                "action": "radio", "radio_action": "send", "request_id": "radio-send",
                "path": "/project", "team_id": "team-id", "expected_revision": 1, "text": "Hello",
            },
            {
                "action": "radio", "radio_action": "pass", "request_id": "radio-pass",
                "path": "/project", "team_id": "team-id", "expected_revision": 1, "target": "agent-2",
            },
            {
                "action": "radio", "radio_action": "stop", "request_id": "radio-stop",
                "path": "/project", "team_id": "team-id", "expected_revision": 1,
            },
        ):
            TypeAdapter(PeerTeamRequest).validate_python(body)
        with self.assertRaises(ValidationError):
            TypeAdapter(PeerTeamRequest).validate_python({
                "action": "radio", "radio_action": "create", "request_id": "radio-create",
                "path": "/project", "name": "Shared chat",
            })
        room_response: PeerTeamsResponse = TypeAdapter(PeerTeamsResponse).validate_python({
            "room": {
                "id": "radio:team", "kind": "private", "members": ["one", "two"],
                "projectPath": "/project", "customName": "Shared chat", "created": 1.0,
                "updated": 2.0, "userHidden": False,
                "radio": {
                    "teamId": "team", "revision": 1, "status": "waiting", "speaker": "one",
                    "next": ["two"], "active": None, "error": None, "seen": {},
                },
            },
        })
        self.assertIsInstance(room_response, PeerRadioResponse)
        self.assertEqual(cast(PeerRadioResponse, room_response).room.radio.status, "waiting")
        schema_app = FastAPI()
        schema_app.include_router(create_router(ApiContext.for_schema()))
        schema = schema_app.openapi()["paths"]["/api/peer-teams"]["post"]["requestBody"]
        schema_text = json.dumps(schema)
        self.assertIn('"propertyName": "radio_action"', schema_text)
        self.assertIn("PeerTeamRadioCreateRequest", schema_text)

    def test_peer_convert_request_preserves_source_and_destination_leads(self) -> None:
        request = PeerTeamConvertRequest.model_validate({
            "action": "convert", "path": "/project", "member": "source-lead",
            "target": "destination-lead", "expected_revision": 4, "request_id": "convert-4",
        })
        self.assertEqual(request.member, "source-lead")
        self.assertEqual(request.target, "destination-lead")
        for unsupported in (
            {"team_id": "peer-team"},
            {"account_key": "account"},
            {"model": "model-x"},
            {"reasoning_effort": "high"},
        ):
            with self.subTest(unsupported=unsupported), self.assertRaises(ValidationError):
                PeerTeamConvertRequest.model_validate({
                    "action": "convert", "path": "/project", "member": "source-lead",
                    "target": "destination-lead", "expected_revision": 4, "request_id": "convert-4",
                    **unsupported,
                })

    def test_peer_radio_open_response_matches_actual_producer(self) -> None:
        fixture_path = Path(__file__).resolve().parents[3] / "tests" / "runtime-accounts-contract.py"
        sys.path.insert(0, str(fixture_path.parent))
        spec = importlib.util.spec_from_file_location("peer_radio_contract_fixture", fixture_path)
        if spec is None or spec.loader is None:
            self.fail("could not load isolated peer radio fixture")
        fixture = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(fixture)
        isolated = fixture.AccountContracts()
        isolated.setUp()
        try:
            first, second = isolated.lead(), isolated.lead(isolated.other_key)
            context = ApiContext.for_schema()
            setattr(context.canvas, "runtime", isolated.runtime)
            app = FastAPI()
            app.include_router(create_router(context))
            team_id = str(uuid4())
            with TestClient(app) as client:
                saved_team = client.post("/api/peer-teams", json={
                    "action": "save", "path": str(isolated.root), "team_id": team_id,
                    "request_id": str(uuid4()), "expected_revision": 0, "name": "Peer team",
                    "members": [first["id"], second["id"]],
                })
                self.assertEqual(saved_team.status_code, 200, saved_team.text)
                opened = client.post("/api/peer-teams", json={
                    "action": "radio", "radio_action": "open", "path": str(isolated.root),
                    "team_id": team_id, "request_id": str(uuid4()),
                })
            self.assertEqual(opened.status_code, 200, opened.text)
            radio_response: PeerTeamsResponse = TypeAdapter(PeerTeamsResponse).validate_python(opened.json())
            self.assertIsInstance(radio_response, PeerRadioResponse)
            room = cast(PeerRadioResponse, radio_response).room
            self.assertEqual(set(room.members or ()), {first["id"], second["id"]})
            self.assertEqual(room.radio.teamId, team_id)
            self.assertEqual(room.radio.status, "idle")
            self.assertIsNotNone(room.updated)
        finally:
            isolated.tearDown()

    def test_peer_convert_route_matches_actual_producer_and_replays_receipt(self) -> None:
        fixture_path = Path(__file__).resolve().parents[3] / "tests" / "runtime-accounts-contract.py"
        sys.path.insert(0, str(fixture_path.parent))
        spec = importlib.util.spec_from_file_location("peer_convert_contract_fixture", fixture_path)
        if spec is None or spec.loader is None:
            self.fail("could not load isolated peer conversion fixture")
        fixture = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(fixture)
        isolated = fixture.AccountContracts()
        isolated.setUp()
        try:
            source = isolated.runtime.create({
                "name": "Source", "prompt": "", "cwd": str(isolated.root), "account_key": "default",
            }, draft=True)
            teammate = isolated.runtime.create({
                "name": "Teammate", "prompt": "", "cwd": str(isolated.root), "account_key": isolated.other_key,
            }, draft=True)
            destination = isolated.runtime.create({
                "name": "Destination", "prompt": "", "cwd": str(isolated.root), "account_key": "default",
            }, draft=True)
            context = ApiContext.for_schema()
            setattr(context.canvas, "runtime", isolated.runtime)
            app = FastAPI()
            app.include_router(create_router(context))
            team_id = str(uuid4())
            body = {
                "action": "convert", "path": str(isolated.root), "member": source["id"],
                "target": destination["id"], "expected_revision": 1, "request_id": str(uuid4()),
            }
            with TestClient(app) as client:
                saved = client.post("/api/peer-teams", json={
                    "action": "save", "path": str(isolated.root), "team_id": team_id,
                    "request_id": str(uuid4()), "expected_revision": 0, "name": "Peer team",
                    "members": [source["id"], teammate["id"]],
                })
                self.assertEqual(saved.status_code, 200, saved.text)
                converted = client.post("/api/peer-teams", json=body)
                replay = client.post("/api/peer-teams", json=body)
            self.assertEqual(converted.status_code, 200, converted.text)
            self.assertEqual(replay.status_code, 200, replay.text)
            self.assertEqual(converted.json()["id"], source["id"])
            self.assertEqual(converted.json()["parentId"], destination["id"])
            self.assertEqual(converted.json()["rootId"], destination["id"])
            self.assertEqual(converted.json(), replay.json())
        finally:
            isolated.tearDown()


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
        return {
            "accountKey": key,
            "data": {
                "accountId": key,
                "rateLimits": {"limitId": "codex", "primary": {"usedPercent": 27}},
                "rateLimitsByLimitId": {"codex": {"limitId": "codex", "primary": {"usedPercent": 27}}},
                "rateLimitResetCredits": {"availableCount": 0, "credits": []},
                "providerExtension": {"payloadVersion": 2},
            },
            "at": 2.0,
            "error": None,
        }

    def catalog(self, key: str) -> dict[str, object]:
        self.catalog_reads.append(key)
        return {"data": [{"model": "fixture/model"}]}


class _ContextFixture:
    def __init__(self, runtime: _RuntimeFixture) -> None:
        self.runtime = runtime

    def send(self, request: Request, value: JsonValue, status: int = 200, **_kwargs: object) -> JSONResponse:
        route = request.scope["route"]
        validated = TypeAdapter[object](route.response_model).validate_python(value)
        if isinstance(validated, ResponseModel):
            content: JsonValue = validated.wire_dump()
        elif isinstance(validated, ContractModel):
            content = cast(JsonValue, validated.model_dump(mode="json"))
        else:
            content = cast(JsonValue, validated)
        return JSONResponse(content=content, status_code=status)


class _AccountStore(Protocol):
    def snapshot(self) -> dict[str, JsonValue]: ...

    def register(self, home: str) -> str: ...

    def discover(self) -> dict[str, JsonValue]: ...


class _TimedLock(Protocol):
    def acquire(self, blocking: bool = True, timeout: float = -1) -> bool: ...

    def release(self) -> None: ...


def _available_to_other_thread(lock: _TimedLock) -> bool:
    acquired_by_other: list[bool] = []
    finished = threading.Event()

    def probe() -> None:
        acquired = False
        try:
            acquired = lock.acquire(timeout=0.1)
            acquired_by_other.append(acquired)
        finally:
            try:
                if acquired:
                    lock.release()
            finally:
                finished.set()

    probe_thread = threading.Thread(target=probe, daemon=True)
    probe_thread.start()
    if not finished.wait(timeout=0.5):
        probe_thread.join(timeout=0.1)
        return False
    probe_thread.join(timeout=0.1)
    return not probe_thread.is_alive() and acquired_by_other == [True]


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
        self.app.include_router(create_router(cast(ApiContext, _ContextFixture(self.runtime))))

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

    def test_account_name_set_clear_validation_and_exact_replay(self) -> None:
        from codex_accounts import AccountStore

        store = cast(AccountStore, self.store)
        store.data["accounts"]["claude"] = {
            "id": "claude", "home": str(self.profile), "provider": "claude",
            "label": "Claude Code", "source": "Claude Code", "status": "ready",
        }
        store._save()
        with patch("codex_claude.auth_metadata", return_value={
            "status": "ready", "email": "claude@example.invalid", "accountId": "claude-fixture",
        }):
            for key in ("default", "claude"):
                with self.subTest(provider=key):
                    body = {"account_key": key, "label": "  Work 1  ", "request_id": str(uuid4())}
                    first = self.client.post("/api/accounts/name", json=body)
                    self.assertEqual(first.status_code, 200, first.text)
                    self.assertEqual(next(row for row in first.json()["accounts"] if row["id"] == key)["label"], "Work 1")
                    clear = {**body, "label": "   ", "request_id": str(uuid4())}
                    cleared = self.client.post("/api/accounts/name", json=clear)
                    self.assertEqual(cleared.status_code, 200, cleared.text)
                    self.assertEqual(next(row for row in cleared.json()["accounts"] if row["id"] == key)["label"], "")
                    # An old receipt must not restore the old label after a later edit.
                    retry = self.client.post("/api/accounts/name", json=body)
                    self.assertEqual(retry.status_code, 200)
                    self.assertEqual(next(row for row in retry.json()["accounts"] if row["id"] == key)["label"], "")
                    with self.assertRaisesRegex(ValueError, "different content"):
                        self.client.post("/api/accounts/name", json={**body, "label": "Work 2"})
                    for invalid in ("x" * 33, None, 123):
                        response = self.client.post("/api/accounts/name", json={**body, "label": invalid, "request_id": str(uuid4())})
                        self.assertEqual(response.status_code, 400)
                    boundary = self.client.post("/api/accounts/name", json={**body, "label": " " + "😀" * 32 + " ", "request_id": str(uuid4())})
                    self.assertEqual(boundary.status_code, 200)
                    invalid_id = self.client.post("/api/accounts/name", json={**body, "request_id": "invalid"})
                    self.assertEqual(invalid_id.status_code, 400)
                    self.assertEqual(store.data["accounts"][key]["label"], "😀" * 32)
            reloaded = AccountStore(self.root / "state")
            self.assertEqual(reloaded.data["accounts"]["default"]["label"], "😀" * 32)
            self.assertEqual(reloaded.data["accounts"]["claude"]["label"], "😀" * 32)

    def test_accounts_response_comes_from_account_store_and_hides_credentials(self) -> None:
        response = self.client.get("/api/accounts")
        self.assertEqual(response.status_code, 200)
        self.assertEqual(response.json()["accounts"][0]["accountId"], "account-default")
        self.assertNotIn("fixture-secret", response.text)

    def test_accounts_get_does_not_publish_and_mutation_retries_are_deduplicated(self) -> None:
        state_dir = cast(Path, getattr(self.store, "root")).parent
        hub = ResourceHub("account-route-workspace")
        loop = asyncio.new_event_loop()
        register_resource_hub(state_dir, hub)
        subscription = hub.subscribe(
            [ResourceRef(AccountsResource(kind="accounts"))], loop=loop
        )
        try:
            self.assertIsNone(loop.run_until_complete(subscription.next_event(timeout=0.01)))
            self.assertEqual(self.client.get("/api/accounts").status_code, 200)
            self.assertIsNone(loop.run_until_complete(subscription.next_event(timeout=0.01)))
            second = self.root / "second-profile"
            second.mkdir()
            self._write_auth(second, "account-second")
            body = {"home": str(second)}
            response = self.client.post("/api/accounts/register", json=body)
            self.assertEqual(response.status_code, 200, response.text)
            event = loop.run_until_complete(subscription.next_event(timeout=1))
            self.assertIsNotNone(event)
            assert event is not None
            self.assertEqual(event.reason, "change")
            self.assertEqual(event.resources, [ResourceRef(AccountsResource(kind="accounts"))])
            repeated = self.client.post("/api/accounts/register", json=body)
            self.assertEqual(repeated.status_code, 200, repeated.text)
            self.assertIsNone(loop.run_until_complete(subscription.next_event(timeout=0.01)))
        finally:
            subscription.close()
            unregister_resource_hub(state_dir, hub)
            loop.close()
        self.assertEqual(response.status_code, 200)
        self.assertEqual(repeated.json(), response.json())

    def test_async_native_login_completion_publishes_after_account_lock(self) -> None:
        import codex_accounts

        request_id = str(uuid4())
        store_data = cast(dict[str, object], getattr(self.store, "data"))
        store_data["logins"] = {
            request_id: {
                "requestId": request_id,
                "accountKey": "default",
                "status": "pending",
                "loginId": "native-login",
            }
        }
        lock_was_available: list[bool] = []
        account_lock = getattr(self.store, "lock")
        state_dir = cast(Path, getattr(self.store, "root")).parent
        hub = ResourceHub("native-login-workspace")
        loop = asyncio.new_event_loop()
        register_resource_hub(state_dir, hub)
        subscription = hub.subscribe(
            [ResourceRef(AccountsResource(kind="accounts"))], loop=loop
        )

        def published(state_dir: Path) -> None:
            lock_was_available.append(_available_to_other_thread(cast(_TimedLock, account_lock)))
            self.assertEqual(state_dir, cast(Path, getattr(self.store, "root")).parent)
            publish_account_change_to_hub(state_dir)

        try:
            with patch.object(
                codex_accounts, "publish_account_change", side_effect=published
            ) as publish:
                getattr(self.store, "login_completed")(
                    "default", {"loginId": "native-login", "success": False}
                )
                getattr(self.store, "login_completed")(
                    "default", {"loginId": "native-login", "success": False}
                )

            publish.assert_called_once()
            self.assertEqual(lock_was_available, [True])
            event = loop.run_until_complete(subscription.next_event(timeout=1))
            self.assertIsNotNone(event)
            assert event is not None
            self.assertEqual(event.reason, "change")
            self.assertEqual(event.resources, [ResourceRef(AccountsResource(kind="accounts"))])
        finally:
            subscription.close()
            unregister_resource_hub(state_dir, hub)
            loop.close()

    def test_account_route_uses_actual_api_context_response_validation(self) -> None:
        context = ApiContext.for_schema()
        setattr(context.canvas, "runtime", self.runtime)
        app = FastAPI()
        app.include_router(create_router(context))
        with TestClient(app) as client:
            response = client.get("/api/accounts")
        self.assertEqual(response.status_code, 200, response.text)
        self.assertEqual(response.json()["defaultAccountKey"], "default")
        self.assertNotIn("_syncEntities", response.json())

    def test_codex_login_route_returns_the_typed_receipt_and_hides_internal_guards(self) -> None:
        result = {
            "requestId": str(uuid4()), "accountKey": "default", "status": "pending",
            "loginId": "native-login", "verificationUrl": "https://auth.openai.com/device",
            "userCode": "TEST-CODE", "createdAt": 12.5, "email": "person@example.invalid",
            "expiresAt": 90.5,
            "reauthAccountKey": "default", "expectedAccountId": "private-account-id",
        }
        context = ApiContext.for_schema()
        setattr(context.canvas, "runtime", self.runtime)
        app = FastAPI()
        app.include_router(create_router(context))
        with patch.object(self.store, "start_login", return_value=result):
            response = TestClient(app).post("/api/accounts/login", json={"login_id": result["requestId"]})
        self.assertEqual(response.status_code, 200, response.text)
        self.assertEqual(response.json()["requestId"], result["requestId"])
        self.assertEqual(response.json()["userCode"], "TEST-CODE")
        self.assertEqual(response.json()["email"], "person@example.invalid")
        self.assertEqual(response.json()["expiresAt"], 90.5)
        self.assertNotIn("expectedAccountId", response.json())
        self.assertNotIn("reauthAccountKey", response.json())

    def test_claude_add_login_route_accepts_email_and_label(self) -> None:
        class LoginService:
            def status(self, request_id: str | None) -> JsonValue:
                raise ValueError("Unknown Claude sign-in request")

            def start(self, account_key: str, request_id: str) -> JsonValue:
                raise AssertionError("Existing account sign-in must not start")

            def start_add(self, email: str | None, label: str | None, request_id: str) -> JsonValue:
                self.started = (email, label, request_id)
                return {
                    "requestId": request_id, "accountKey": "pending-" + request_id,
                    "status": "starting", "email": email, "plan": None, "codeSubmitted": False,
                }

            def code(self, request_id: str, code: str) -> JsonValue:
                raise AssertionError("Code submission must not start")

            def cancel(self, request_id: str) -> JsonValue:
                raise AssertionError("Cancellation must not start")

        service = LoginService()
        request_id = str(uuid4())
        with patch("codex_claude_login.manager", return_value=service):
            response = self.client.post("/api/accounts/claude/add", json={
                "login_id": request_id, "email": " person@example.invalid ", "label": " Work ",
            })
            invalid = self.client.post("/api/accounts/claude/add", json={
                "login_id": str(uuid4()), "unexpected": True,
            })
        self.assertEqual(response.status_code, 200, response.text)
        self.assertEqual(service.started, ("person@example.invalid", "Work", request_id))
        self.assertEqual(response.json()["status"], "starting")
        self.assertEqual(invalid.status_code, 400)

    def test_discover_keeps_optional_empty_body_in_openapi_and_runtime(self) -> None:
        operation = self.app.openapi()["paths"]["/api/accounts/discover"]["post"]
        self.assertIn("requestBody", operation)
        self.assertFalse(operation["requestBody"].get("required", False))
        self.assertEqual(operation["requestBody"]["content"]["application/json"]["schema"]["$ref"].split("/")[-1], "AccountDiscoverRequest")
        self.assertEqual(self.client.post("/api/accounts/discover").status_code, 200)
        self.assertEqual(self.client.post("/api/accounts/discover", json={}).status_code, 200)

    def test_discover_rejects_unknown_body_before_account_scan(self) -> None:
        with patch.object(self.store, "discover", wraps=self.store.discover) as discover:
            response = self.client.post("/api/accounts/discover", json={"unexpected": True})
        self.assertEqual(response.status_code, 400)
        discover.assert_not_called()

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

    def test_incomplete_radio_create_is_rejected_before_peer_service(self) -> None:
        from codex_peer_teams import manage

        with patch("codex_peer_teams.manage", wraps=manage) as peer_manage:
            response = self.client.post("/api/peer-teams", json={
                "action": "radio", "radio_action": "create", "request_id": str(uuid4()),
                "path": str(self.root), "name": "Shared chat",
            })
        self.assertEqual(response.status_code, 400)
        peer_manage.assert_not_called()

    def test_peer_convert_invalid_contract_is_rejected_before_service(self) -> None:
        from codex_peer_teams import manage

        with patch("codex_peer_teams.manage", wraps=manage) as peer_manage:
            response = self.client.post("/api/peer-teams", json={
                "action": "convert", "path": str(self.root), "member": "source",
                "team_id": "not-a-conversion-target", "expected_revision": 0,
                "request_id": str(uuid4()),
            })
        self.assertEqual(response.status_code, 400)
        peer_manage.assert_not_called()

    def test_legacy_first_nonempty_account_and_singleton_cached_behavior(self) -> None:
        cached = self.client.get("/api/limits?account_key=&account_key=first&account_key=second&cached=1")
        self.assertEqual(cached.status_code, 200)
        self.assertEqual(cached.json()["accountKey"], "first")
        self.assertEqual(self.runtime.limit_reads, [])
        repeated = self.client.get("/api/limits?account_key=first&cached=1&cached=1")
        self.assertEqual(repeated.status_code, 200)
        self.assertEqual(repeated.json()["data"]["rateLimits"]["primary"]["usedPercent"], 27)
        self.assertEqual(repeated.json()["data"]["providerExtension"]["payloadVersion"], 2)
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
        models_operation = self.app.openapi()["paths"]["/api/models"]["get"]
        retry_parameter = next(
            param for param in models_operation["parameters"] if param["name"] == "retry"
        )
        self.assertEqual(
            next(value["const"] for value in retry_parameter["schema"]["anyOf"] if "const" in value),
            "1",
        )

    def test_worker_catalog_preserves_actual_unavailable_account_metadata(self) -> None:
        from codex_catalog import CatalogUnavailable

        class WorkerAccounts:
            def list(self) -> list[dict[str, object]]:
                return [
                    {"id": "first", "status": "ready", "provider": "codex"},
                    {"id": "second", "status": "ready", "provider": "codex"},
                ]

            def default(self) -> str:
                return "first"

            def get(self, key: str) -> dict[str, object]:
                return {"id": key, "status": "ready", "provider": "codex"}

        class WorkerRuntime(_RuntimeFixture):
            def __init__(self) -> None:
                super().__init__(cast(_AccountStore, WorkerAccounts()))

            def catalog(self, key: str) -> dict[str, object]:
                if key == "first":
                    raise CatalogUnavailable("fixture account offline")
                return {"data": [{"model": "worker/model"}]}

        worker_app = FastAPI()
        worker_app.include_router(create_router(cast(ApiContext, _ContextFixture(WorkerRuntime()))))
        with TestClient(worker_app) as client:
            response = client.get("/api/models?account_key=first&workers=1")
        self.assertEqual(response.status_code, 200)
        body = ModelCatalogResponse.model_validate(response.json())
        if body.data is None:
            self.fail("worker catalog omitted models")
        self.assertEqual(body.data[0].model, "worker/model")
        self.assertEqual(body.unavailableAccounts[0].accountKey, "first")
        self.assertEqual(body.unavailableAccounts[0].error, "fixture account offline")

    def test_catalog_pending_keeps_legacy_400_body(self) -> None:
        from codex_catalog import CatalogPending

        with patch.object(self.runtime, "catalog", side_effect=CatalogPending("Still loading")):
            response = self.client.get("/api/models")
        self.assertEqual(response.status_code, 400)
        self.assertEqual(response.json(), {"error": "Still loading", "catalogPending": True})

    def test_catalog_unavailable_returns_terminal_error_body(self) -> None:
        from codex_catalog import CatalogUnavailable

        with patch.object(self.runtime, "catalog", side_effect=CatalogUnavailable("Catalog is unavailable")):
            response = self.client.get("/api/models")
        self.assertEqual(response.status_code, 400)
        self.assertEqual(response.json(), {"error": "Catalog is unavailable"})

    def test_models_route_forwards_only_singleton_explicit_retry(self) -> None:
        from codex_catalog import DISPLAY_RETRY

        seen: list[bool] = []

        def catalog(_account_key: str) -> dict[str, object]:
            seen.append(DISPLAY_RETRY.get())
            return {"data": []}

        with patch.object(self.runtime, "catalog", side_effect=catalog):
            self.assertEqual(self.client.get("/api/models").status_code, 200)
            self.assertEqual(self.client.get("/api/models?retry=1").status_code, 200)
            self.assertEqual(
                self.client.get("/api/models?retry=1&retry=1").status_code,
                200,
            )
        self.assertEqual(seen, [False, True, False])

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
            context = ApiContext.for_schema()
            setattr(context.canvas, "runtime", runtime)
            app.include_router(create_router(context))
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

    def test_project_mutations_accept_actual_project_records(self) -> None:
        fixture_path = Path(__file__).resolve().parents[3] / "tests" / "runtime-accounts-contract.py"
        sys.path.insert(0, str(fixture_path.parent))
        spec = importlib.util.spec_from_file_location("project_contract_fixture", fixture_path)
        if spec is None or spec.loader is None:
            self.fail("could not load isolated project fixture")
        fixture = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(fixture)
        isolated = fixture.AccountContracts()
        isolated.setUp()
        try:
            app = FastAPI()
            context = ApiContext.for_schema()
            setattr(context.canvas, "runtime", isolated.runtime)
            app.include_router(create_router(context))
            project_path = isolated.root / "project"
            project_path.mkdir()
            with TestClient(app) as client:
                registered = client.post("/api/projects", json={"path": str(project_path)})
                self.assertEqual(registered.status_code, 200, registered.text)
                project = registered.json()
                self.assertEqual(project["path"], str(project_path.resolve()))
                self.assertEqual(project["name"], "project")
                self.assertEqual(project["accountKey"], "default")
                self.assertEqual(project["accountRevision"], 1)
                self.assertIsInstance(project["created"], float)
                account = client.post("/api/projects", json={
                    "action": "set_account", "path": str(project_path),
                    "account_key": isolated.other_key, "expected_revision": 1,
                })
                self.assertEqual(account.status_code, 200, account.text)
                self.assertEqual(account.json()["accountKey"], isolated.other_key)
                self.assertEqual(account.json()["accountRevision"], 2)
                updated = client.post("/api/projects", json={
                    "action": "set_worker_base", "path": str(project_path),
                    "base_ref": "main", "expected_revision": 0,
                })
                self.assertEqual(updated.status_code, 200, updated.text)
                self.assertEqual(updated.json()["workerBaseRef"], "main")
                self.assertEqual(updated.json()["workerBaseRevision"], 1)
                organized = client.post("/api/projects", json={
                    "action": "rename", "path": str(project_path),
                    "name": "Renamed", "expected_revision": 0,
                })
                self.assertEqual(organized.status_code, 200, organized.text)
                self.assertEqual(organized.json()["name"], "Renamed")
                self.assertEqual(organized.json()["organizationRevision"], 1)
                removed = client.post("/api/projects", json={"action": "remove", "path": str(project_path)})
                self.assertEqual(removed.status_code, 200, removed.text)
                self.assertEqual(removed.json(), {"id": str(project_path.resolve()), "removed": True})
        finally:
            isolated.tearDown()

    def test_delivery_receipt_accepts_actual_runtime_replay_states(self) -> None:
        fixture_path = Path(__file__).resolve().parents[3] / "tests" / "runtime-accounts-contract.py"
        sys.path.insert(0, str(fixture_path.parent))
        spec = importlib.util.spec_from_file_location("delivery_contract_fixture", fixture_path)
        if spec is None or spec.loader is None:
            self.fail("could not load isolated delivery fixture")
        fixture = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(fixture)
        isolated = fixture.AccountContracts()
        isolated.setUp()
        try:
            lead = isolated.lead()
            request_id = str(uuid4())
            isolated.runtime.send(lead["id"], "exact command", message_id=request_id)
            for status in ("reserved", "stored_only"):
                with isolated.runtime.db() as db:
                    db.execute("UPDATE runtime_events SET status=? WHERE id=?", (status, request_id))
                receipt = isolated.runtime.send(lead["id"], "exact command", message_id=request_id)
                validated = ClaudeDeliveryResponse.model_validate(receipt)
                self.assertEqual(validated.status, status)
                self.assertEqual(validated.id, request_id)
        finally:
            isolated.tearDown()


if __name__ == "__main__":
    unittest.main()
