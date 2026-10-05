"""Per-listener services shared by typed FastAPI routers."""
from __future__ import annotations

import gzip
import hashlib
import json
import secrets
import sqlite3
import threading
from contextlib import closing
from functools import lru_cache
from pathlib import Path
from typing import TYPE_CHECKING, Protocol, cast, get_args, get_origin

from pydantic import BaseModel, RootModel, TypeAdapter

from starlette.requests import Request
from starlette.responses import Response

from codex_remote import RemoteAccess

from studio_api.models import ContractModel, ErrorResponse, JsonValue, ResponseModel

if TYPE_CHECKING:
    from codex_canvas import Canvas
    from codex_costs import AccountCostReader
    from codex_pricing import PricingCatalog
    from codex_runtime import Runtime
    from codex_session_costs import SessionCostReader
    from codex_sync import SyncStore
    from studio_api.sync.resources.hub import ResourceHub
    from codex_terminals import TerminalManager


class HeaderCollection(Protocol):
    def get(self, name: str, default: str | None = None) -> str | None: ...
    def get_all(self, name: str, default: list[str] | None = None) -> list[str]: ...


class RemoteAccessContract(Protocol):
    def origin(self) -> str | None: ...
    def request_origin(self, headers: HeaderCollection, peer: str, port: int) -> str | None: ...

JSON_CONTENT_TYPE = "application/json"
DEFAULT_CACHE_CONTROL = "no-store"
COMPRESS_MINIMUM_BYTES = 1024
GZIP_LEVEL = 3
REFERRER_POLICY = "no-referrer"
CONTENT_SECURITY_POLICY = (
    "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; "
    "connect-src 'self' https://api.openai.com; "
    "img-src 'self' data: blob: https: http:; "
    "media-src 'self' blob: data:; frame-src 'self' blob:; "
    "frame-ancestors 'none'; base-uri 'none'"
)


class ApiContext:
    """Own request-scoped API services while Canvas remains state authority."""

    def __init__(
        self,
        canvas: Canvas,
        token: str | None = None,
        remote: RemoteAccessContract | None = None,
        *,
        server_port: int = 0,
        unix_socket: bool = False,
        schema_only: bool = False,
    ) -> None:
        self.canvas = canvas
        self.token = token if token is not None else secrets.token_urlsafe(32)
        self.remote = remote if remote is not None else cast(RemoteAccessContract, RemoteAccess(canvas.root))  # type: ignore[no-untyped-call]
        self.server_port = server_port
        self.unix_socket = unix_socket
        self.schema_only = schema_only
        self._api_schema_hash: str | None = None
        self._lock = threading.RLock()
        self._maintenance_lock = threading.Lock()
        self._maintenance_last = 0.0
        self._terminal: TerminalManager | None = None
        self._cost_reader: AccountCostReader | None = None
        self._pricing: PricingCatalog | None = None
        self._session_cost_reader: SessionCostReader | None = None
        self._sync_store: SyncStore | None = None
        self._resource_hub: ResourceHub | None = None

    @classmethod
    def for_schema(cls) -> ApiContext:
        """Return inert router context; constructing it performs no state IO."""

        class SchemaCanvas:
            root = Path(".")
            runtime = None

        return cls(cast("Canvas", SchemaCanvas()), token="schema-token", remote=cast(RemoteAccessContract, object()), schema_only=True)

    @property
    def runtime(self) -> Runtime | None:
        """Return the attached runtime, which may not exist during startup."""
        runtime = cast("Runtime | None", self.canvas.runtime)
        if runtime is not None and self._sync_store is not None:
            setattr(runtime, "sync_store", self._sync_store)
        return runtime

    @property
    def api_schema_hash(self) -> str:
        if self._api_schema_hash is None:
            from studio_api.schema import api_schema_hash

            self._api_schema_hash = api_schema_hash()
        return self._api_schema_hash

    @api_schema_hash.setter
    def api_schema_hash(self, value: str) -> None:
        self._api_schema_hash = value

    def terminals(self) -> TerminalManager:
        self._require_runtime()
        with self._lock:
            if self._terminal is None:
                from codex_terminals import TerminalManager

                self._terminal = TerminalManager(self.canvas.root)  # type: ignore[no-untyped-call]
            return self._terminal

    def sync(self) -> SyncStore:
        with self._lock:
            if self._sync_store is None:
                from codex_sync import SyncStore

                def state_signature() -> tuple[object, ...] | None:
                    runtime = self.runtime
                    connected = False
                    volatile = None
                    if runtime:
                        if not runtime.start_lock.acquire(blocking=False):
                            return None
                        try:
                            if not runtime.lock.acquire(blocking=False):
                                return None
                            try:
                                connected = bool(set(runtime.servers) - runtime.offline_accounts) and not runtime.closed
                                monitor = getattr(runtime, "provider_version_monitor", None)
                                warnings = monitor.status()["warnings"] if monitor else []
                                volatile = json.dumps({
                                    "rateLimits": runtime.rate_limits,
                                    "rateLimitsByAccount": {
                                        key: runtime.rate_limits_for(key)  # type: ignore[no-untyped-call]
                                        for key in runtime.rate_limits_by_account
                                    },
                                    "connectionIds": runtime.connection_ids,
                                    "providerWarnings": warnings,
                                }, sort_keys=True, separators=(",", ":"))
                            finally:
                                runtime.lock.release()
                        finally:
                            runtime.start_lock.release()
                    files = []
                    for path in sorted(self.canvas.root.glob("codex-swarm-status.*.json")):
                        try:
                            stat = path.stat()
                        except FileNotFoundError:
                            continue
                        files.append((path.name, stat.st_ino, stat.st_size, stat.st_mtime_ns))
                    from codex_state import process_is_alive, read_threads

                    liveness = tuple(sorted((
                        (row.get("launcherPid"), process_is_alive(cast(int, row["launcherPid"])))
                        for row in read_threads(self.canvas.root)
                        if row.get("launcherPid") is not None
                    ), key=lambda item: str(item[0])))
                    return connected, volatile, tuple(files), liveness

                self._sync_store = SyncStore(  # type: ignore[no-untyped-call]
                    self.canvas.connect,
                    self.snapshot,
                    self.canvas.transcript,
                    chat_snapshot=lambda: self.snapshot(include_work=False),
                    state_signature=state_signature,
                )
                if self.runtime is not None:
                    setattr(self.runtime, "sync_store", self._sync_store)
            return self._sync_store

    def resource_hub(self) -> ResourceHub:
        """Return the process-local typed event hub for this workspace."""
        with self._lock:
            if self._resource_hub is None:
                from codex_token_rate import token_rates
                from studio_api.sync.resources.hub import (
                    LazyProgressWatchdog,
                    ResourceHub,
                    register_resource_hub,
                )
                from studio_api.sync.resources.models import TokenRateSnapshot

                identity = self.sync().identity()
                runtime = self.runtime
                initial_rates = (
                    TokenRateSnapshot.model_validate(token_rates(runtime).workspace_snapshot())
                    if runtime else TokenRateSnapshot(rates={}, teams={})
                )
                self._resource_hub = ResourceHub(
                    cast(str, identity["workspaceId"]),
                    LazyProgressWatchdog(self.canvas.root),
                    initial_rates,
                )
                register_resource_hub(self.canvas.root, self._resource_hub)
            return self._resource_hub

    def initialize(self) -> None:
        """Prepare durable workspace identity before accepting requests."""
        if not self.schema_only:
            self.sync()
            self.resource_hub()

    def snapshot(self, include_work: bool = True) -> dict[str, JsonValue]:
        if self.runtime:
            with self.runtime.read_db() as db:
                runtime_value = self.runtime.snapshot(include_work=include_work, db=db)  # type: ignore[no-untyped-call]
                return cast(dict[str, JsonValue], {
                    **self.canvas.snapshot(runtime_snapshot=runtime_value, db=db),  # type: ignore[no-untyped-call]
                    "runtime": runtime_value,
                })
        return cast(dict[str, JsonValue], {**self.canvas.snapshot(), "runtime": None})  # type: ignore[no-untyped-call]

    def entity_sequence(self) -> int | None:
        """Read the sync cursor without constructing services or mutating state."""
        uri = self.canvas.db.absolute().as_uri() + "?mode=ro"
        try:
            with closing(sqlite3.connect(uri, uri=True, timeout=1)) as db:
                row = db.execute("SELECT COALESCE(MAX(seq), 0) FROM sync_entities").fetchone()
                return int(row[0]) if row is not None else 0
        except sqlite3.OperationalError as error:
            if "no such table" in str(error).lower():
                return None
            raise

    def workspace_id(self) -> str:
        """Read the durable workspace identity without initializing SyncStore."""
        uri = self.canvas.db.absolute().as_uri() + "?mode=ro"
        with closing(sqlite3.connect(uri, uri=True, timeout=1)) as db:
            row = db.execute("SELECT id FROM sync_identity LIMIT 1").fetchone()
        if row is None:
            raise RuntimeError("The server workspace identity is unavailable")
        return str(row[0])

    def costs(self) -> AccountCostReader:
        runtime = self._require_runtime()
        with self._lock:
            if self._cost_reader is None:
                from codex_costs import AccountCostReader
                from codex_pricing import PricingCatalog

                if self._pricing is None:
                    self._pricing = PricingCatalog(self.canvas.root)  # type: ignore[no-untyped-call]
                self._cost_reader = AccountCostReader(self.canvas.root, runtime.accounts, pricing=self._pricing)  # type: ignore[no-untyped-call]
            return self._cost_reader

    def session_costs(self) -> SessionCostReader:
        runtime = self.runtime
        with self._lock:
            if self._pricing is None:
                from codex_pricing import PricingCatalog

                self._pricing = PricingCatalog(self.canvas.root)  # type: ignore[no-untyped-call]
            if self._session_cost_reader is None:
                from codex_session_costs import SessionCostReader

                self._session_cost_reader = SessionCostReader(  # type: ignore[no-untyped-call]
                    self.canvas.root / "canvas.sqlite3",
                    self._pricing,
                    accounts=getattr(runtime, "accounts", None),
                    state_root=self.canvas.root,
                )
            return self._session_cost_reader

    def send(
        self,
        request: Request,
        value: object,
        status: int = 200,
        content_type: str = JSON_CONTENT_TYPE,
        cache_control: str = DEFAULT_CACHE_CONTROL,
        compressed: bytes | None = None,
        etag: bool = False,
        weak_etag_fields: tuple[str, ...] = (),
        server_timing: dict[str, float] | None = None,
    ) -> Response:
        """Validate JSON output against the registered route model before sending."""
        sync_after = request.scope.get("studio_sync_entities_after")
        weak_validator_data: bytes | None = None
        body_value: object
        if content_type.startswith(JSON_CONTENT_TYPE):
            body_value = self._dump_json(value)
        elif isinstance(value, bytes):
            body_value = value
        else:
            raise TypeError("Non-JSON response bodies must be bytes")
        if status < 400 and isinstance(body_value, dict) and sync_after is not None and "_syncEntities" not in body_value:
            with self.sync().connect() as db:
                rows = db.execute(
                    """SELECT collection,id,seq,payload,deleted FROM sync_entities
                       WHERE seq>? AND collection NOT LIKE 'transcript:%' ORDER BY seq""",
                    (sync_after,),
                ).fetchall()
            if rows:
                body_value = dict(body_value)
                body_value["_syncEntities"] = [
                    {"id": f"entity:{row[0]}:{row[1]}", "seq": row[2], "payload": row[3], "_deleted": bool(row[4])}
                    for row in rows
                ]
        if content_type.startswith(JSON_CONTENT_TYPE):
            try:
                route = request.scope.get("route")
                response_model = getattr(route, "response_model", None)
                if status >= 400:
                    response_model = ErrorResponse
                adapter = ApiContext._response_adapter(response_model, id(response_model))
                validated = adapter.validate_python(body_value)
                data = adapter.dump_json(validated, by_alias=True, exclude_unset=True)
                if weak_etag_fields:
                    weak_body = adapter.dump_python(validated, mode="json", by_alias=True, exclude_unset=True)
                    if isinstance(weak_body, dict):
                        body_value = weak_body
                        weak_value = {key: item for key, item in weak_body.items() if key not in weak_etag_fields}
                        weak_validator_data = ApiContext._response_adapter(JsonValue, id(JsonValue)).dump_json(
                            ApiContext._sort_json_keys(weak_value)
                        )
            except Exception as error:
                # The handler may already have performed a durable write. This is
                # a server contract failure and deliberately carries no retry hint.
                if status >= 400:
                    error_value = body_value.get("error", "Request failed") if isinstance(body_value, dict) else "Request failed"
                    error_body = ErrorResponse(error=str(error_value)).wire_dump()
                    data = json.dumps(error_body, ensure_ascii=False, separators=(",", ":")).encode()
                else:
                    data = json.dumps({"error": "The server could not validate its response"}).encode()
                    status = 500
                compressed = None
                etag = False
                weak_etag_fields = ()
                weak_validator_data = None
        else:
            data = cast(bytes, body_value)

        compressible = content_type.startswith(("application/json", "application/manifest+json", "text/", "image/svg+xml"))
        use_gzip = False
        if compressible and len(data) >= COMPRESS_MINIMUM_BYTES and self._accepts_gzip(request.headers.get("accept-encoding", "")):
            candidate = compressed if compressed is not None else gzip.compress(data, compresslevel=GZIP_LEVEL, mtime=0)
            if len(candidate) < len(data):
                data, use_gzip = candidate, True
        validator_data = data
        if weak_validator_data is not None:
            validator_data = weak_validator_data
        validator = None
        if etag:
            weak = "W/" if weak_etag_fields else ""
            validator = f'{weak}"{hashlib.sha256(validator_data).hexdigest()}"'
        headers: dict[str, str] = {"Cache-Control": cache_control, "X-Content-Type-Options": "nosniff"}
        if content_type:
            headers["Content-Type"] = content_type
        if compressible:
            headers["Vary"] = "Accept-Encoding"
        if validator:
            headers["ETag"] = validator
            if any(tag.strip() == "*" or tag.strip().removeprefix("W/") == validator.removeprefix("W/") for tag in request.headers.get("if-none-match", "").split(",")):
                return Response(b"", status_code=304, headers={**headers, "Content-Length": "0"})
        if use_gzip:
            headers["Content-Encoding"] = "gzip"
        if server_timing:
            headers["Server-Timing"] = ", ".join(f"{name};dur={duration:.2f}" for name, duration in server_timing.items())
        headers["Referrer-Policy"] = REFERRER_POLICY
        headers["Content-Security-Policy"] = CONTENT_SECURITY_POLICY
        return Response(data, status_code=status, headers=headers, media_type=None)

    @staticmethod
    def _dump_json(value: object) -> object:
        if isinstance(value, ResponseModel):
            return value.wire_dump()
        if isinstance(value, BaseModel):
            return value.model_dump(mode="json", by_alias=True, exclude_unset=True)
        if isinstance(value, (dict, list, str, int, float, bool)) or value is None:
            return value
        raise TypeError("Unsupported JSON response value")

    @staticmethod
    def _sort_json_keys(value: object) -> object:
        if isinstance(value, dict):
            entries = cast(dict[str, object], value)
            return {key: ApiContext._sort_json_keys(entries[key]) for key in sorted(entries)}
        if isinstance(value, list):
            return [ApiContext._sort_json_keys(item) for item in value]
        return value

    @staticmethod
    @lru_cache(maxsize=256)
    def _response_adapter(response_model: object, _identity: int) -> TypeAdapter[object]:
        """Reuse contract metadata, never response values or validation results."""
        # Equal unions can have different branch order. Keep their adapters distinct.
        if not ApiContext._response_contract(response_model):
            raise TypeError("JSON API route has no explicit response model")
        return TypeAdapter(response_model)

    @staticmethod
    def _response_contract(candidate: object) -> bool:
        if candidate is JsonValue:
            return True
        if isinstance(candidate, type):
            if issubclass(candidate, ResponseModel):
                return True
            if issubclass(candidate, RootModel):
                return ApiContext._nested_response_contract(candidate.model_fields["root"].annotation)
            return False
        members = get_args(candidate)
        origin = get_origin(candidate)
        if origin in (list, tuple):
            return len(members) == 1 and ApiContext._nested_response_contract(members[0])
        if members:
            return all(ApiContext._nested_response_contract(member) for member in members)
        return False

    @staticmethod
    def _nested_response_contract(candidate: object) -> bool:
        if candidate is JsonValue:
            return True
        if candidate is type(None):
            return True
        if isinstance(candidate, type):
            if candidate in (str, int, float, bool):
                return True
            return issubclass(candidate, ContractModel)
        members = get_args(candidate)
        origin = get_origin(candidate)
        if origin in (list, tuple):
            return len(members) == 1 and ApiContext._nested_response_contract(members[0])
        if origin is dict:
            return len(members) == 2 and members[0] is str and ApiContext._nested_response_contract(members[1])
        if members:
            return all(ApiContext._nested_response_contract(member) for member in members)
        return candidate in (str, int, float, bool)

    @staticmethod
    def _accepts_gzip(value: str) -> bool:
        encodings: dict[str, float] = {}
        for part in value.split(","):
            name, *parameters = part.strip().lower().split(";")
            quality = 1.0
            for parameter in parameters:
                if parameter.strip().startswith("q="):
                    try:
                        quality = float(parameter.strip()[2:])
                    except ValueError:
                        quality = 0
            encodings[name] = quality
        return encodings.get("gzip", encodings.get("*", 0)) > 0

    def close(self) -> None:
        resource_hub = None
        with self._lock:
            if self._cost_reader is not None:
                self._cost_reader.close()  # type: ignore[no-untyped-call]
                self._cost_reader = None
            if self._terminal is not None:
                self._terminal.close()  # type: ignore[no-untyped-call]
                self._terminal = None
            if self._session_cost_reader is not None:
                self._session_cost_reader = None
            resource_hub = self._resource_hub
            self._resource_hub = None
        if resource_hub is not None:
            from studio_api.sync.resources.hub import unregister_resource_hub

            unregister_resource_hub(self.canvas.root, resource_hub)
            resource_hub.close()

    def _require_runtime(self) -> Runtime:
        runtime = self.runtime
        if runtime is None:
            raise ValueError("The agent runtime is unavailable")
        return runtime
