"""Per-listener services shared by typed FastAPI routers."""
from __future__ import annotations

import gzip
import hashlib
import json
import secrets
import sqlite3
import threading
from contextlib import closing
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
    from codex_terminals import TerminalManager
    from codex_runtime import Runtime


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
        self._lock = threading.RLock()
        self._maintenance_lock = threading.Lock()
        self._maintenance_last = 0.0
        self._terminal: TerminalManager | None = None
        self._cost_reader: AccountCostReader | None = None
        self._pricing: PricingCatalog | None = None
        self._session_cost_reader: SessionCostReader | None = None
        self._sync_store: SyncStore | None = None

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
                                rate_limits = json.loads(json.dumps(runtime.rate_limits, sort_keys=True))
                                by_account = {
                                    key: json.loads(json.dumps(runtime.rate_limits_for(key), sort_keys=True))  # type: ignore[no-untyped-call]
                                    for key in runtime.rate_limits_by_account
                                }
                                connection_ids = dict(runtime.connection_ids)
                                monitor = getattr(runtime, "provider_version_monitor", None)
                                warnings = monitor.status()["warnings"] if monitor else []
                                volatile = json.dumps({
                                    "rateLimits": rate_limits,
                                    "rateLimitsByAccount": by_account,
                                    "connectionIds": connection_ids,
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

    def initialize(self) -> None:
        """Prepare durable workspace identity before accepting requests."""
        if not self.schema_only:
            self.sync()

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
                elif not self._response_contract(response_model):
                    raise TypeError("JSON API route has no explicit response model")
                adapter: TypeAdapter[object] = TypeAdapter(response_model)
                validated = adapter.validate_python(body_value)
                body_value = adapter.dump_python(validated, mode="json", by_alias=True, exclude_unset=True)
                if not isinstance(body_value, (dict, list, str, int, float, bool)) and body_value is not None:
                    raise TypeError("Response contract must produce a JSON value")
                data = json.dumps(body_value, ensure_ascii=False, separators=(",", ":")).encode()
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
        else:
            data = cast(bytes, body_value)

        compressible = content_type.startswith(("application/json", "application/manifest+json", "text/", "image/svg+xml"))
        use_gzip = False
        if compressible and len(data) >= COMPRESS_MINIMUM_BYTES and self._accepts_gzip(request.headers.get("accept-encoding", "")):
            candidate = compressed if compressed is not None else gzip.compress(data, compresslevel=GZIP_LEVEL, mtime=0)
            if len(candidate) < len(data):
                data, use_gzip = candidate, True
        validator_data = data
        if weak_etag_fields and isinstance(body_value, dict):
            validator_data = json.dumps(
                {key: item for key, item in body_value.items() if key not in weak_etag_fields},
                ensure_ascii=False, sort_keys=True,
            ).encode()
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
        return Response(data, status_code=status, headers=headers, media_type=None)

    @staticmethod
    def _dump_json(value: object) -> object:
        if isinstance(value, ResponseModel):
            return value.wire_dump()
        if isinstance(value, BaseModel):
            return value.model_dump(mode="json", by_alias=True, exclude_unset=True)
        if isinstance(value, dict):
            return dict(value)
        if isinstance(value, (list, str, int, float, bool)) or value is None:
            return value
        raise TypeError("Unsupported JSON response value")

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
        with self._lock:
            if self._cost_reader is not None:
                self._cost_reader.close()  # type: ignore[no-untyped-call]
                self._cost_reader = None
            if self._terminal is not None:
                self._terminal.close()  # type: ignore[no-untyped-call]
                self._terminal = None
            if self._session_cost_reader is not None:
                self._session_cost_reader = None

    def _require_runtime(self) -> Runtime:
        runtime = self.runtime
        if runtime is None:
            raise ValueError("The agent runtime is unavailable")
        return runtime
