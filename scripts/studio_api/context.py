"""Per-listener services shared by typed FastAPI routers."""
from __future__ import annotations

import asyncio
from concurrent.futures import Future
import gzip
import hashlib
import importlib.metadata
import json
import logging
import os
import platform
import re
import secrets
import sqlite3
import tempfile
import threading
import subprocess
import sys
from contextlib import closing
from functools import lru_cache
from pathlib import Path
from typing import TYPE_CHECKING, Callable, Protocol, cast, get_args, get_origin

from codex_backend_identity import BACKEND_BUILD

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
    "connect-src 'self' https://api.openai.com https://*.ts.net; "
    "img-src 'self' data: blob: https: http:; "
    "media-src 'self' blob: data:; frame-src 'self' blob: http://*.localhost:*; "
    "frame-ancestors 'none'; base-uri 'none'"
)
API_SCHEMA_CACHE_DIRECTORY = "codex-studio-api-schema"
API_SCHEMA_CACHE_FILE = "hash-v1.json"
API_SCHEMA_CANONICALIZER_VERSION = "2"
API_SCHEMA_CACHE_VERIFY_DELAY_SECONDS = 60.0


def api_schema_cache_key(backend_build: str | None = None) -> dict[str, str]:
    """Key the persistent hash cache by every runtime input to OpenAPI."""
    if backend_build is None:
        backend_build = BACKEND_BUILD
    return {
        "backendBuild": backend_build,
        "canonicalizer": API_SCHEMA_CANONICALIZER_VERSION,
        "python": platform.python_version(),
        "fastapi": importlib.metadata.version("fastapi"),
        "pydantic": importlib.metadata.version("pydantic"),
        "starlette": importlib.metadata.version("starlette"),
    }


def _api_schema_cache_path() -> Path:
    from codex_python import cache_root

    return cache_root() / API_SCHEMA_CACHE_DIRECTORY / API_SCHEMA_CACHE_FILE


def _valid_schema_hash(value: object) -> bool:
    return isinstance(value, str) and re.fullmatch(r"[0-9a-f]{64}", value) is not None


def _write_cached_api_schema_hash(key: dict[str, str], value: str) -> None:
    if not _valid_schema_hash(value):
        raise RuntimeError("API schema hash computation returned an invalid value")
    try:
        cache_path = _api_schema_cache_path()
        cache_path.parent.mkdir(parents=True, exist_ok=True)
        descriptor, temporary_name = tempfile.mkstemp(
            prefix="hash-", suffix=".tmp", dir=cache_path.parent,
        )
        try:
            with os.fdopen(descriptor, "w", encoding="utf-8") as cache_file:
                json.dump({"key": key, "hash": value}, cache_file, sort_keys=True)
                cache_file.write("\n")
                cache_file.flush()
                os.fsync(cache_file.fileno())
            os.replace(temporary_name, cache_path)
        finally:
            try:
                os.unlink(temporary_name)
            except FileNotFoundError:
                pass
    except Exception:
        logging.getLogger(__name__).warning(
            "Could not write the API schema identity cache", exc_info=True,
        )


def read_cached_or_compute_api_schema_hash(
    key: dict[str, str], compute: Callable[[], str],
    on_cache_hit: Callable[[str], None] | None = None,
) -> str:
    """Return a matching disk cache or compute and atomically cache the hash."""
    cache_path: Path | None = None
    try:
        cache_path = _api_schema_cache_path()
        entry = json.loads(cache_path.read_text(encoding="utf-8"))
        if (
            isinstance(entry, dict)
            and entry.get("key") == key
            and _valid_schema_hash(entry.get("hash"))
        ):
            value = cast(str, entry["hash"])
            if on_cache_hit is not None:
                on_cache_hit(value)
            return value
    except Exception:
        # Every cache/path/parse problem is a miss. Only compute() failures
        # are allowed to make schema identity unavailable.
        pass

    value = compute()
    if not _valid_schema_hash(value):
        raise RuntimeError("API schema hash computation returned an invalid value")
    _write_cached_api_schema_hash(key, value)
    return value


def compute_api_schema_hash_in_subprocess(*, low_priority: bool = False) -> str:
    """Build the canonical OpenAPI hash in a process isolated from serving GIL."""
    scripts = Path(__file__).resolve().parents[1]
    lower_priority = "import os; os.nice(10); " if low_priority and os.name == "posix" else ""
    code = lower_priority + (
        "from studio_api.schema import openapi_document, api_schema_hash; "
        "print(api_schema_hash(openapi_document()))"
    )
    result = subprocess.run(
        [sys.executable, "-B", "-c", code],
        cwd=scripts,
        check=True,
        capture_output=True,
        text=True,
        timeout=60,
    )
    value = result.stdout.strip()
    if not _valid_schema_hash(value):
        raise RuntimeError("schema verification subprocess returned an invalid hash")
    return value


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
        backend_build: str | None = None,
    ) -> None:
        self.canvas = canvas
        self.token = token if token is not None else secrets.token_urlsafe(32)
        self.remote = remote if remote is not None else cast(RemoteAccessContract, RemoteAccess(canvas.root))
        self.server_port = server_port
        self.unix_socket = unix_socket
        self.schema_only = schema_only
        self.backend_build = backend_build if backend_build is not None else BACKEND_BUILD
        self._api_schema_hash: str | None = None
        self._api_schema_hash_future: Future[str] | None = None
        self._api_schema_hash_future_lock = threading.Lock()
        self._api_schema_cache_verification_started = False
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
            self._sync_store.runtime = runtime
            setattr(runtime, "sync_store", self._sync_store)
        return runtime

    @property
    def api_schema_hash(self) -> str:
        if self._api_schema_hash is None:
            raise RuntimeError("API schema hash is not ready; await get_api_schema_hash()")
        return self._api_schema_hash

    @api_schema_hash.setter
    def api_schema_hash(self, value: str) -> None:
        self._api_schema_hash = value

    def _compute_api_schema_hash(self) -> str:
        key = api_schema_cache_key(self.backend_build)
        value = read_cached_or_compute_api_schema_hash(
            key,
            compute_api_schema_hash_in_subprocess,
            on_cache_hit=lambda cached: self._schedule_api_schema_cache_verification(key, cached),
        )
        if not value:
            raise RuntimeError("API schema hash computation returned an empty value")
        return value

    def _schedule_api_schema_cache_verification(
        self, key: dict[str, str], cached_value: str,
    ) -> None:
        with self._api_schema_hash_future_lock:
            if self._api_schema_cache_verification_started:
                return
            self._api_schema_cache_verification_started = True

        def verify_later() -> None:
            # The subprocess owns Python's CPU-heavy OpenAPI build, so it
            # cannot hold the serving process's GIL during busy page loads.
            threading.Event().wait(API_SCHEMA_CACHE_VERIFY_DELAY_SECONDS)
            try:
                verified = compute_api_schema_hash_in_subprocess(low_priority=True)
                if verified == cached_value:
                    return
                logging.getLogger(__name__).error(
                    "Cached Studio API schema identity was wrong; replacing it"
                )
                _write_cached_api_schema_hash(key, verified)
                self._api_schema_hash = verified
            except Exception:
                logging.getLogger(__name__).exception(
                    "Could not verify cached Studio API schema identity"
                )

        threading.Thread(
            target=verify_later,
            name="studio-api-schema-cache-verification",
            daemon=True,
        ).start()

    def start_api_schema_hash(self) -> Future[str]:
        """Start one background schema-hash computation and share its result."""
        with self._api_schema_hash_future_lock:
            if self._api_schema_hash_future is not None:
                return self._api_schema_hash_future
            future: Future[str] = Future()
            self._api_schema_hash_future = future
            if self._api_schema_hash is not None:
                future.set_result(self._api_schema_hash)
                return future

            def compute() -> None:
                try:
                    value = self._compute_api_schema_hash()
                except Exception as error:
                    logging.getLogger(__name__).exception(
                        "Failed to compute Studio API schema identity"
                    )
                    if not future.done():
                        future.set_exception(error)
                else:
                    self._api_schema_hash = value
                    if not future.done():
                        future.set_result(value)

            threading.Thread(
                target=compute,
                name="studio-api-schema-hash",
                daemon=True,
            ).start()
            return future

    async def get_api_schema_hash(self) -> str:
        """Await the shared background computation without blocking the event loop."""
        if self._api_schema_hash is not None:
            return self._api_schema_hash
        return await asyncio.shield(asyncio.wrap_future(self.start_api_schema_hash()))

    def peek_api_schema_hash(self) -> str | None:
        """Return the cached schema identity only when its background work is done."""
        if self._api_schema_hash is not None:
            return self._api_schema_hash
        future = self._api_schema_hash_future
        if future is None or not future.done():
            return None
        return future.result()

    def terminals(self) -> TerminalManager:
        self._require_runtime()
        with self._lock:
            if self._terminal is None:
                from codex_terminals import TerminalManager

                self._terminal = TerminalManager(self.canvas.root)
            return self._terminal

    def sync(self) -> SyncStore:
        with self._lock:
            if self._sync_store is None:
                from codex_sync import SyncStore
                self._sync_store = SyncStore(
                    self.canvas.connect,
                    self.canvas.transcript,
                    runtime=self.runtime,
                    canvas=self.canvas,
                )
                if self.runtime is not None:
                    setattr(self.runtime, "sync_store", self._sync_store)
            runtime = self.runtime
            if runtime is not None:
                self._sync_store.runtime = runtime
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
                entity_sequence = self.entity_sequence() or 0
                self._resource_hub = ResourceHub(
                    cast(str, identity["workspaceId"]),
                    LazyProgressWatchdog(self.canvas.root),
                    initial_rates,
                    entity_sequence=entity_sequence,
                )
                register_resource_hub(self.canvas.root, self._resource_hub)
                latest_sequence = self.entity_sequence() or 0
                if latest_sequence > entity_sequence:
                    self._resource_hub.publish_entity_sequence(latest_sequence, reset=True)
            return self._resource_hub

    def initialize(self) -> None:
        """Prepare durable workspace identity before accepting requests."""
        if not self.schema_only:
            self.sync()
            self.resource_hub()

    def entity_sequence(self) -> int | None:
        """Read the sync cursor without constructing services or mutating state."""
        uri = self.canvas.db.absolute().as_uri() + "?mode=ro"
        try:
            with closing(sqlite3.connect(uri, uri=True, timeout=1)) as db:
                row = db.execute(
                    "SELECT COALESCE(MAX(seq), 0) FROM sync_entities "
                    "WHERE collection NOT LIKE 'transcript:%'"
                ).fetchone()
                high = int(row[0]) if row is not None else 0
                floor = db.execute(
                    "SELECT value FROM sync_entity_meta WHERE key='entity_tombstone_floor'"
                ).fetchone()
                return max(high, int(floor[0]) if floor else 0)
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
                    self._pricing = PricingCatalog(self.canvas.root)
                self._cost_reader = AccountCostReader(self.canvas.root, runtime.accounts, pricing=self._pricing)
            return self._cost_reader

    def session_costs(self) -> SessionCostReader:
        runtime = self.runtime
        with self._lock:
            if self._pricing is None:
                from codex_pricing import PricingCatalog

                self._pricing = PricingCatalog(self.canvas.root)
            if self._session_cost_reader is None:
                from codex_session_costs import SessionCostReader

                self._session_cost_reader = SessionCostReader(
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
                floor_row = db.execute(
                    "SELECT value FROM sync_entity_meta WHERE key='entity_tombstone_floor'",
                    (),
                ).fetchone()
                tombstone_floor = int(floor_row[0]) if floor_row else 0
            if rows:
                body_value = dict(body_value)
                body_value["_syncEntities"] = [
                    {"id": f"entity:{row[0]}:{row[1]}", "seq": row[2], "payload": row[3], "_deleted": bool(row[4])}
                    for row in rows
                ]
                from codex_sync_entities import MAX_MUTATION_SYNC_ENTITIES

                if len(rows) <= MAX_MUTATION_SYNC_ENTITIES and tombstone_floor <= sync_after:
                    body_value["_syncEntitiesAfter"] = sync_after
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
                    # Reads fail open: a contract mismatch in stored or live data
                    # must not take a read endpoint down. Send the strict JSON
                    # body as is and log the exact mismatch for a DTO correction.
                    # Writes keep the strict failure without a retry hint.
                    try:
                        if request.method not in ("GET", "HEAD"):
                            raise ValueError("write response contract failure")
                        data = json.dumps(body_value, ensure_ascii=False, separators=(",", ":"), allow_nan=False).encode()
                    except (TypeError, ValueError):
                        data = json.dumps({"error": "The server could not validate its response"}).encode()
                        status = 500
                    else:
                        logging.getLogger(__name__).error(
                            "Response contract mismatch for %s %s: %s",
                            request.method, request.url.path, str(error)[:4000],
                        )
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
        policy = CONTENT_SECURITY_POLICY
        if (content_type.startswith("text/html") and request.url.path == "/"
                and re.fullmatch(r"[a-zA-Z0-9_-]{1,128}", request.query_params.get("studio-server", ""))):
            policy = policy.replace("frame-ancestors 'none'", "frame-ancestors 'self' http://127.0.0.1:* http://localhost:*")
        headers["Content-Security-Policy"] = policy
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
                self._cost_reader.close()
                self._cost_reader = None
            if self._terminal is not None:
                self._terminal.close()
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
