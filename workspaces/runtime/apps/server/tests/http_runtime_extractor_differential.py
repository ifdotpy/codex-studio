"""Generated OpenAPI request corpus checked against the real FastAPI boundary."""
from __future__ import annotations

import asyncio
import copy
import json
import os
from pathlib import Path
import subprocess
import tempfile
import sys
import types
from typing import Any
from urllib.parse import unquote

ROOT = Path(__file__).resolve().parents[5]
PYTHON_SRC = ROOT / "workspaces/runtime/apps/server/src"
if str(PYTHON_SRC) not in __import__("sys").path:
    __import__("sys").path.insert(0, str(PYTHON_SRC))
CORPUS_GENERATOR = ROOT / "workspaces/runtime/packages/runtime-http/tests/generate_openapi_request_corpus.py"
MARKER = "RUST_HTTP_GENERATED_CORPUS="
# Every entry is an exact generated case. Temporary schema/model differences expire
# at the next schema-tightening remeasurement; route rules remain tied to these cases.
_EXPECTED_DIRECTIONS = {
            ("rename_api_rename_post", "name:type:NoneType", "rust_accept_python_reject"):
        "permanent route rule: Python requires a name unless request_id is present",
    ("push_drafts_api_sync_drafts_post", "rows:maxItems:below", "rust_accept_python_reject"):
        "permanent route rule: draft payload identity is cross-field validated",
    ("push_preferences_api_sync_preferences_post", "fields:compound:2", "rust_accept_python_reject"):
        "permanent route rule: preference key and scope are cross-field validated",
}
# These opaque values are not validation differences. Their documented
# precision behavior is checked after decoding by opaque_integer_difference.
for _parameter in ("after", "initialHigh"):
    _EXPECTED_DIRECTIONS[(
        "pull_api_sync_pull_get",
        f"query:{_parameter}:1267650600228229401496703205376",
        "rust_reject_python_accept",
    )] = "temporary: Python accepts an integer above the documented sync parameter maximum"
_EXPECTED_DIRECTIONS[(
    "branch_api_branch_post",
    "before:type:NoneType",
    "rust_accept_python_reject",
)] = "temporary: Python rejects null where the request schema allows a nullable branch cursor"
# On the pre-tightening Python models, several UUID fields are emitted as
# unconstrained strings in OpenAPI, Python's integer query parsing is lax, and
# Python accepts bool for integer protocol fields. These exact rows expire when
# the model/schema tightening lands.
_uuid_fields = {
    "claude_add_start_api_accounts_claude_add_post": ("login_id",),
    "claude_login_start_api_accounts_claude_login_post": ("login_id",),
    "claude_login_cancel_api_accounts_claude_login_cancel_post": ("login_id",),
    "claude_login_code_api_accounts_claude_login_code_post": ("login_id",),
    "account_delete_api_accounts_delete_post": ("request_id",),
    "account_login_api_accounts_login_post": ("login_id",),
    "account_login_cancel_api_accounts_login_cancel_post": ("login_id",),
    "account_name_api_accounts_name_post": ("request_id",),
    "reset_limits_api_limits_reset_post": ("request_id",),
}
for _operation, _fields in _uuid_fields.items():
    for _field in _fields:
        _EXPECTED_DIRECTIONS[(_operation, f"{_field}:type:str", "rust_accept_python_reject")] = (
            "temporary: Python UUID field is emitted as an unconstrained OpenAPI string"
        )
for _label in ("id:type:str", "id:minLength:above", "id:maxLength:below"):
    _EXPECTED_DIRECTIONS[("create_agent_api_agents_post", _label, "rust_accept_python_reject")] = (
        "temporary: Python UUID field is emitted as an unconstrained OpenAPI string"
    )

_lax_integer_queries = {
    "agent_chat_api_agent_chat_get": ("before", "after", "limit"),
    "search_api_search_get": ("limit",),
    "pull_api_sync_pull_get": ("after", "limit", "initialHigh"),
    "sync_stream_api_sync_stream_get": ("after",),
    "transcript_api_transcript_get": ("limit",),
    "transcript_page_api_transcript_page_get": ("limit",),
    "transcript_search_api_transcript_search_get": ("limit",),
    "workspace_tasks_api_workspace_tasks_get": ("limit",),
}
for _operation, _fields in _lax_integer_queries.items():
    for _field in _fields:
        for _text in ("1.0", "1_000", " 1 "):
            _EXPECTED_DIRECTIONS[(
                _operation,
                f"query:{_field}:{_text}",
                "rust_reject_python_accept",
            )] = "temporary: Python accepts lax integer query spellings; the boundary requires ASCII decimal"

for _value in ("", "x", "1", "1.0", "1_000", " 1 ", "true", "false", "yes", "NaN", "Infinity", "1267650600228229401496703205376"):
    _EXPECTED_DIRECTIONS[(
        "analytics_api_analytics_get",
        f"query:scope:{_value}",
        "rust_reject_python_accept",
    )] = "temporary: Python accepts analytics scope values excluded by the OpenAPI enum"

_boolean_integer_fields = {
    "manage_federation_api_federation_post": ("invitation/protocol",),
    "message_api_federation_v1_message_post": ("envelope/protocol", "protocol"),
    "pair_api_federation_v1_pair_post": ("protocol",),
    "pull_api_federation_v1_pull_post": ("protocol",),
    "status_api_federation_v1_status_post": ("protocol",),
    "manage_api_multi_server_post": ("invitation/protocol",),
    "auto_pair_api_multi_server_v1_auto_pair_post": ("protocol",),
    "pair_api_multi_server_v1_pair_post": ("protocol",),
}
for _operation, _fields in _boolean_integer_fields.items():
    for _field in _fields:
        _EXPECTED_DIRECTIONS[(_operation, f"{_field}:lax:True", "rust_reject_python_accept")] = (
            "temporary: Python accepts bool for a strict integer protocol field"
        )

_EXPECTED_DIRECTIONS[("queue_action_api_queue_post", "id:type:NoneType", "rust_accept_python_reject")] = (
    "temporary: Python request model rejects null for a document-nullable queue id"
)
# The authored declarations are terse directions; the runtime table stores an exact
# operation, case label, both HTTP statuses, and the reason for each exception.
_STATUS_BY_DIRECTION = {
    "rust_accept_python_reject": (200, 400),
    "rust_reject_python_accept": (400, 200),
    "status:400->422": (400, 422),
}
_EXPECTED_STATUS_DIFFERENCES: dict[tuple[str, str], tuple[int, int, str]] = {}
for (_operation, _label, _direction), _cause in _EXPECTED_DIRECTIONS.items():
    if _operation == "*":
        raise AssertionError("expected-difference entries must name an exact operation")
    _rust_status, _python_status = _STATUS_BY_DIRECTION[_direction]
    _EXPECTED_STATUS_DIFFERENCES[(_operation, _label)] = (
        _rust_status, _python_status, _cause
    )
_EXPECTED_STATUS_DIFFERENCES[("account_discover_api_accounts_discover_post", "body:empty")] = (
    200, 413, "temporary: Python request boundary rejects an empty body although the document makes this body optional",
)
_EXPECTED_STATUS_DIFFERENCES[("account_discover_api_accounts_discover_post", "body:missing")] = (
    200, 415, "temporary: Python request boundary requires JSON Content-Type when an optional body is absent",
)
_python_query_models = {
    "claude_login_status_api_accounts_claude_login_get",
    "file_content_api_file_get",
    "file_info_api_file_info_get",
    "limits_api_limits_get",
    "models_api_models_get",
    "monitor_log_api_monitor_log_get",
    "terminal_output_api_terminals_output_get",
}

UNBOUNDED_INTEGER_CAUSE = "integer without declared range, pending document bounds"
UNBOUNDED_PARAMETER_INTEGER_CAUSE = "unbounded parameter integer outside native range, pending document bounds"
REPEATED_SCALAR_QUERY_CAUSE = "temporary: pre-tightening Python accepts repeated declared scalar query parameters"
OPAQUE_JSON_INTEGER_CAUSE = "permanent: integer outside i64/u64 inside opaque JsonValue is carried as f64"
INTEGER_NUMBER_UNION_CAUSE = "temporary: integer|number union on the pre-tightening document"
INTEGER_NUMBER_UNION_POINTERS = frozenset({
    "#/components/schemas/BrowserForward/properties/expiresAt",
    "#/components/schemas/PanelLayoutBody/properties/contentHeight",
    "#/components/schemas/PanelLayoutBody/properties/contentWidth",
    "#/components/schemas/PanelLayoutBody/properties/height",
    "#/components/schemas/PanelLayoutBody/properties/overflowX",
    "#/components/schemas/PanelLayoutBody/properties/overflowY",
    "#/components/schemas/PanelLayoutBody/properties/width",
    "#/components/schemas/RuleBody/properties/at",
    "#/components/schemas/RuleBody/properties/created",
    "#/components/schemas/RuleBody/properties/fileActivityAt",
    "#/components/schemas/RuleBody/properties/lastAt",
    "#/components/schemas/RuleBody/properties/lastFinished",
    "#/components/schemas/RuleBody/properties/lastStallFinished",
    "#/components/schemas/RuleBody/properties/lowSince",
    "#/components/schemas/RuleBody/properties/nextAt",
    "#/components/schemas/SignedEnvelope/properties/created",
})
REQUIRED_PREDICTED_DIFFERENCE_CAUSES = frozenset({
    REPEATED_SCALAR_QUERY_CAUSE,
    UNBOUNDED_PARAMETER_INTEGER_CAUSE,
    UNBOUNDED_INTEGER_CAUSE,
    OPAQUE_JSON_INTEGER_CAUSE,
    INTEGER_NUMBER_UNION_CAUSE,
})


_MISSING = object()


def opaque_integer_difference(case: dict[str, Any], rust_row: dict[str, Any], python_row: dict[str, Any], operation: dict[str, Any], document: dict[str, Any]) -> str | None:
    label = case.get("label", "")
    if (
        not rust_row.get("accepted")
        or not python_row.get("accepted")
        or rust_row.get("status") != python_row.get("status")
        or ":lax:" not in label
    ):
        return None
    try:
        integer = int(label.rsplit(":lax:", 1)[1])
    except ValueError:
        return None
    if -(1 << 63) <= integer <= (1 << 64) - 1:
        return None
    location = label.split(":lax:", 1)[0].split("/")
    schema = operation.get("requestBody", {}).get("content", {}).get("application/json", {}).get("schema", {})
    components = document.get("components", {}).get("schemas", {})
    current_schema: Any = schema
    opaque_json = False
    for segment in location:
        while isinstance(current_schema, dict) and "$ref" in current_schema:
            reference = current_schema["$ref"]
            opaque_json |= reference == "#/components/schemas/JsonValue"
            current_schema = components.get(reference.rsplit("/", 1)[-1], {})
        branches = current_schema.get("anyOf", current_schema.get("oneOf", [])) if isinstance(current_schema, dict) else []
        if branches:
            if segment.isdecimal():
                current_schema = next((branch for branch in branches if branch.get("type") == "array"), {})
            else:
                current_schema = next((branch for branch in branches if schema_has_property(branch, segment, components)), {})
            while isinstance(current_schema, dict) and "$ref" in current_schema:
                reference = current_schema["$ref"]
                opaque_json |= reference == "#/components/schemas/JsonValue"
                current_schema = components.get(reference.rsplit("/", 1)[-1], {})
        if isinstance(current_schema, dict) and current_schema.get("type") == "object":
            if segment in current_schema.get("properties", {}):
                current_schema = current_schema["properties"][segment]
            elif segment == "key" and isinstance(current_schema.get("additionalProperties"), dict):
                current_schema = current_schema["additionalProperties"]
            else:
                return None
        elif isinstance(current_schema, dict) and current_schema.get("type") == "array":
            current_schema = current_schema.get("items", {})
        elif segment.isdecimal():
            continue
        else:
            return None
    while isinstance(current_schema, dict) and "$ref" in current_schema:
        reference = current_schema["$ref"]
        opaque_json |= reference == "#/components/schemas/JsonValue"
        current_schema = components.get(reference.rsplit("/", 1)[-1], {})
    if opaque_json:
        cause = OPAQUE_JSON_INTEGER_CAUSE
    else:
        stack = [current_schema]
        integer_branches = []
        terminal_types = []
        while stack:
            branch = stack.pop()
            if not isinstance(branch, dict):
                continue
            if "$ref" in branch:
                reference = branch["$ref"]
                opaque_json |= reference == "#/components/schemas/JsonValue"
                stack.append(components.get(reference.rsplit("/", 1)[-1], {}))
            elif "anyOf" in branch or "oneOf" in branch:
                stack.extend(branch.get("anyOf", branch.get("oneOf", [])))
            else:
                terminal_types.append(branch.get("type"))
                if branch.get("type") == "integer":
                    integer_branches.append(branch)
        if opaque_json:
            cause = OPAQUE_JSON_INTEGER_CAUSE
        else:
            only_integer_or_null = bool(integer_branches) and all(
                value in {"integer", "null"} for value in terminal_types
            )
            bound_name = "minimum" if integer < 0 else "maximum"
            if (
                not only_integer_or_null
                or not integer_branches
                or all(bound_name in branch for branch in integer_branches)
            ):
                return None
            cause = UNBOUNDED_INTEGER_CAUSE
    rust_value = rust_row.get("value", {}).get("body")
    python_value = python_row.get("value", {}).get("body")
    for segment in location:
        if isinstance(rust_value, dict) and isinstance(python_value, dict):
            rust_value = rust_value.get(segment, _MISSING)
            python_value = python_value.get(segment, _MISSING)
        elif isinstance(rust_value, list) and isinstance(python_value, list) and segment.isdecimal():
            index = int(segment)
            if index >= len(rust_value) or index >= len(python_value):
                return None
            rust_value = rust_value[index]
            python_value = python_value[index]
        else:
            return None
    if (
        type(rust_value) is float
        and type(python_value) is int
        and (python_value < -(1 << 63) or python_value > (1 << 64) - 1)
        and rust_value == float(python_value)
    ):
        return cause
    return None


def unbounded_parameter_integer_difference(
    case: dict[str, Any], rust_row: dict[str, Any], python_row: dict[str, Any], operation: dict[str, Any], document: dict[str, Any]
) -> tuple[str, str] | None:
    if not (
        rust_row.get("accepted") is False
        and rust_row.get("status") == 400
        and python_row.get("accepted") is True
        and python_row.get("status") == 200
    ):
        return None
    label = case.get("label", "")
    if not label.startswith(("query:", "header:")):
        return None
    section, rest = label.split(":", 1)
    name, separator, token = rest.rpartition(":")
    if not separator or not name:
        return None
    try:
        integer = int(token)
    except ValueError:
        return None
    if -(1 << 63) <= integer <= (1 << 64) - 1:
        return None
    parameter = next((
        parameter for parameter in operation.get("parameters", [])
        if parameter.get("in") == section and parameter.get("name", "").lower() == name.lower()
    ), None)
    if parameter is None:
        return None
    schema = parameter.get("schema", {})
    components = document.get("components", {}).get("schemas", {})
    pointer = f"#/paths/{case['path'].replace('~', '~0').replace('/', '~1')}/{case['method'].lower()}/parameters/{operation['parameters'].index(parameter)}/schema"
    if "$ref" in schema:
        reference = schema["$ref"]
        prefix = "#/components/schemas/"
        if not reference.startswith(prefix) or "/" in reference[len(prefix):] or "~" in reference[len(prefix):]:
            return None
        schema = components.get(reference[len(prefix):])
    if not _integer_without_wire_bound(schema, integer):
        return None
    value_section = "headers" if section == "header" else section
    python_value = python_row.get("value", {}).get(value_section, {}).get(name.lower() if section == "header" else name)
    if type(python_value) is not int or python_value != integer:
        return None
    return pointer, UNBOUNDED_PARAMETER_INTEGER_CAUSE


def repeated_scalar_query_difference(
    case: dict[str, Any], operation: dict[str, Any]
) -> str | None:
    label = case.get("label", "")
    if not label.startswith(("query:repeated:", "query:repeated-empty:", "query:repeated-encoded:")):
        return None
    parts = label.split(":", 2)
    if len(parts) != 3:
        return None
    name = parts[2]
    parameter = next((
        parameter for parameter in operation.get("parameters", [])
        if parameter.get("in") == "query" and parameter.get("name") == name
    ), None)
    if parameter is None:
        return None
    schema = parameter.get("schema", {})
    if schema.get("type") == "array":
        return None
    return REPEATED_SCALAR_QUERY_CAUSE


def schema_has_property(schema: Any, name: str, components: dict[str, Any]) -> bool:
    while isinstance(schema, dict) and "$ref" in schema:
        schema = components.get(schema["$ref"].rsplit("/", 1)[-1], {})
    if isinstance(schema, dict) and schema.get("type") == "object":
        return name in schema.get("properties", {})
    return False


def rust_observations(corpus_path: Path) -> list[dict[str, Any]]:
    environment = os.environ.copy()
    environment["STUDIO_HTTP_REQUEST_CORPUS"] = str(corpus_path)
    process = subprocess.run(
        [
            "cargo", "test", "--locked", "-p", "studio-runtime-http",
            "print_generated_openapi_request_corpus", "--", "--ignored", "--nocapture",
        ],
        cwd=ROOT, env=environment, check=False, capture_output=True, text=True,
    )
    if process.returncode:
        raise RuntimeError(process.stderr + process.stdout)
    for line in process.stdout.splitlines():
        if line.startswith(MARKER):
            return json.loads(line[len(MARKER):])
    raise RuntimeError("Rust generated-corpus test did not print its observations")


async def python_observations(corpus_path: Path) -> list[dict[str, Any]]:
    cases: list[dict[str, Any]] = json.loads(corpus_path.read_text(encoding="utf-8"))
    import contextvars
    import inspect
    from fastapi import FastAPI
    from starlette.responses import JSONResponse
    from studio_api.app import create_app
    from studio_api.context import ApiContext
    from studio_api.federation.models import MessageRequest, PairRequest, PullRequest, StatusRequest
    from studio_api.middleware import RequestBoundary
    from urllib.parse import parse_qsl

    class Context:
        token = "token"
        schema_only = True
        remote = types.SimpleNamespace(request_origin=lambda *_args: "http://localhost:12345")
        send = ApiContext.send
        _dump_json = staticmethod(ApiContext._dump_json)
        _response_adapter = staticmethod(ApiContext._response_adapter)
        _accepts_gzip = staticmethod(ApiContext._accepts_gzip)
        def peek_api_schema_hash(self) -> str: return "hash"
        async def get_api_schema_hash(self) -> str: return "hash"
        def workspace_id(self) -> str: return "workspace"

    context = Context()
    source = create_app(context)
    app = FastAPI(dependencies=source.router.dependencies)
    app.exception_handlers.update(source.exception_handlers)
    signed_request_models = {
        "/api/federation/v1/pair": PairRequest,
        "/api/federation/v1/status": StatusRequest,
        "/api/federation/v1/message": MessageRequest,
        "/api/federation/v1/pull": PullRequest,
    }
    current_case: contextvars.ContextVar[dict[str, Any]] = contextvars.ContextVar("current_case")

    def install(route: Any) -> None:
        if hasattr(route, "original_router"):
            for nested in route.original_router.routes:
                install(nested)
        elif hasattr(route, "dependant") and getattr(route, "include_in_schema", False):
            async def stub(**kwargs: Any) -> JSONResponse:
                def plain(value: Any) -> Any:
                    if hasattr(value, "scope") and hasattr(value, "receive"):
                        return None
                    if hasattr(value, "model_dump"):
                        return value.model_dump(mode="json", by_alias=True, exclude_unset=True)
                    if isinstance(value, dict): return {key: plain(item) for key, item in value.items()}
                    if isinstance(value, (list, tuple)): return [plain(item) for item in value]
                    return value
                def values(parameters: list[Any], *, header: bool = False) -> dict[str, Any]:
                    result: dict[str, Any] = {}
                    case = current_case.get()
                    supplied = (
                        {name.lower() for name in case.get("headers", {})}
                        if header
                        else {name for name, _ in parse_qsl(case.get("rawQuery", ""), keep_blank_values=True)}
                    )
                    for parameter in parameters:
                        name = parameter.alias or parameter.name
                        if (name.lower() if header else name) not in supplied:
                            continue
                        value = kwargs.get(parameter.name, kwargs.get(name, _MISSING))
                        if value is not _MISSING:
                            result[name.lower() if header else name] = plain(value)
                    return result

                received: dict[str, Any] = {
                    "query": values(route.dependant.query_params),
                    "headers": values(route.dependant.header_params, header=True),
                }
                for name, value in kwargs.items():
                    if name not in {"request", "body"} and hasattr(value, "model_dump"):
                        dumped = plain(value)
                        if isinstance(dumped, dict):
                            received["query"].update(dumped)
                for section in ("query", "headers"):
                    nested = received[section].pop(section, None)
                    if isinstance(nested, dict):
                        received[section].update(nested)
                    case = current_case.get()
                    supplied = (
                        {name.lower() for name in case.get("headers", {})}
                        if section == "headers"
                        else {name for name, _ in parse_qsl(case.get("rawQuery", ""), keep_blank_values=True)}
                    )
                    received[section] = {
                        name: value for name, value in received[section].items()
                        if not name.startswith("_")
                        and (name.lower() if section == "headers" else name) in supplied
                    }
                body_params = route.dependant.body_params
                if len(body_params) == 1:
                    parameter = body_params[0]
                    name = parameter.alias or parameter.name
                    value = kwargs.get(parameter.name, kwargs.get(name, _MISSING))
                    if value is not _MISSING:
                        received["body"] = plain(value)
                elif body_params:
                    received["body"] = {
                        (parameter.alias or parameter.name): plain(
                            kwargs.get(parameter.name, kwargs.get(parameter.alias or parameter.name))
                        )
                        for parameter in body_params
                    }
                elif signed_model is not None:
                    value = kwargs.get("body", _MISSING)
                    if value is not _MISSING:
                        received["body"] = plain(value)
                return JSONResponse({"received": received})
            signed_model = signed_request_models.get(route.path) if "POST" in route.methods else None
            if signed_model is not None:
                stub.__signature__ = inspect.Signature(
                    [
                        inspect.Parameter(
                            "body",
                            inspect.Parameter.POSITIONAL_OR_KEYWORD,
                            annotation=signed_model,
                        )
                    ]
                )
            else:
                stub.__signature__ = inspect.signature(route.endpoint, eval_str=True)
            app.add_api_route(route.path, stub, methods=list(route.methods))

    for route in source.routes:
        install(route)
    app.add_middleware(RequestBoundary, context=context)

    async def issue(case: dict[str, Any]) -> dict[str, Any]:
        case_token = current_case.set(case)
        path = case.get("requestPath", case["path"])
        query = case.get("rawQuery", "")
        body_present = case.get("bodyPresent", True)
        raw = case.get("raw", "").encode() if body_present else b""
        headers = {
            "host": "localhost:12345", "x-canvas-token": "token",
        }
        if body_present:
            headers["content-type"] = "application/json"
            headers["content-length"] = str(len(raw))
        headers.update({name.lower(): value for name, value in case.get("headers", {}).items()})
        if path == "/api/sync/drafts": headers["x-canvas-workspace"] = "workspace"
        scope = {
            "type": "http", "asgi": {"version": "3.0"}, "http_version": "1.1",
            "scheme": "http", "method": case["method"], "path": unquote(path),
            "raw_path": path.encode(), "query_string": query.encode(),
            "headers": [(key.encode(), value.encode()) for key, value in headers.items()],
            "client": ("127.0.0.1", 42), "server": ("localhost", 12345),
        }
        messages: list[dict[str, Any]] = []
        async def receive() -> dict[str, Any]: return {"type": "http.request", "body": raw, "more_body": False}
        async def send(message: dict[str, Any]) -> None: messages.append(message)
        try:
            await app(scope, receive, send)
            start = next(item for item in messages if item["type"] == "http.response.start")
            body = b"".join(item.get("body", b"") for item in messages if item["type"] == "http.response.body")
            value = json.loads(body)
            accepted = start["status"] < 400
            row: dict[str, Any] = {"id": case["id"], "accepted": accepted, "status": start["status"]}
            if accepted: row["value"] = value.get("received", {})
            else: row["body"] = value
            return row
        except Exception as error:
            return {"id": case["id"], "exception": f"{type(error).__name__}: {error}"}
        finally:
            current_case.reset(case_token)

    return [await issue(case) for case in cases]


def strict_equal(left: Any, right: Any) -> bool:
    if type(left) is not type(right):
        return False
    if isinstance(left, dict):
        return left.keys() == right.keys() and all(strict_equal(left[key], right[key]) for key in left)
    if isinstance(left, list):
        return len(left) == len(right) and all(strict_equal(a, b) for a, b in zip(left, right, strict=True))
    return left == right


def _case_integer(case: dict[str, Any]) -> int | None:
    label = case.get("label", "")
    try:
        if ":lax:" in label:
            return int(label.rsplit(":lax:", 1)[1])
        if label.startswith(("query:", "header:")):
            return int(label.rpartition(":")[2])
    except ValueError:
        return None
    return None


def _synthetic_body_for_numeric_case(case: dict[str, Any], leaf_value: int | float) -> dict[str, Any]:
    path = case.get("label", "").split(":lax:", 1)[0].split("/")
    root: dict[str, Any] = {}
    current: Any = root
    for index, part in enumerate(path):
        last = index == len(path) - 1
        next_is_index = not last and path[index + 1].isdecimal()
        if isinstance(current, dict):
            if last:
                current[part] = leaf_value
            else:
                current[part] = [] if next_is_index else {}
                current = current[part]
        else:
            position = int(part)
            while len(current) <= position:
                current.append(None)
            if last:
                current[position] = leaf_value
            else:
                current[position] = [] if next_is_index else {}
                current = current[position]
    return root


def _integer_without_wire_bound(schema: Any, integer: int) -> bool:
    if not isinstance(schema, dict):
        return False
    if "const" in schema or "enum" in schema:
        return False
    integer_schemas: list[dict[str, Any]] = []
    kind = schema.get("type")
    if kind == "integer" or isinstance(kind, list) and "integer" in kind:
        integer_schemas.append(schema)
    else:
        branches = schema.get("anyOf", schema.get("oneOf"))
        if not isinstance(branches, list):
            return False
        kinds = [branch.get("type") if isinstance(branch, dict) else None for branch in branches]
        if not branches or any(branch_kind not in ("integer", "null") for branch_kind in kinds) or "integer" not in kinds:
            return False
        integer_schemas.extend(branch for branch in branches if branch.get("type") == "integer")
    bound_name = "minimum" if integer < 0 else "maximum"
    return all(bound_name not in integer_schema for integer_schema in integer_schemas)


def _approved_integer_number_union_without_wire_bound(schema: Any, integer: int) -> bool:
    """Only the enumerated pre-tightening integer|number unions get this exception."""
    if not isinstance(schema, dict):
        return False
    if set(schema) - {"anyOf", "title"}:
        return False
    branches = schema.get("anyOf")
    if not isinstance(branches, list):
        return False
    integer_branches = [
        branch for branch in branches
        if isinstance(branch, dict) and branch.get("type") == "integer"
    ]
    number_branches = [
        branch for branch in branches
        if isinstance(branch, dict) and branch.get("type") == "number"
    ]
    allowed_types = {"integer", "number", "null"}
    branch_types = [branch.get("type") for branch in branches if isinstance(branch, dict)]
    if (
        len(integer_branches) != 1
        or len(number_branches) != 1
        or len(branch_types) != len(branches)
        or any(branch_type not in allowed_types for branch_type in branch_types)
        or len(branches) not in (2, 3)
        or (len(branches) == 3 and branch_types.count("null") != 1)
        or any(set(branch) != {"type"} for branch in branches)
    ):
        return False
    bound = "minimum" if integer < 0 else "maximum"
    return bound not in integer_branches[0]


def _schema_at_pointer(document: dict[str, Any], pointer: str) -> Any:
    value: Any = document
    for segment in pointer.removeprefix("#/").split("/"):
        segment = segment.replace("~1", "/").replace("~0", "~")
        if isinstance(value, dict):
            if segment not in value:
                return None
            value = value[segment]
        elif isinstance(value, list) and segment.isdecimal() and int(segment) < len(value):
            value = value[int(segment)]
        else:
            return None
    return value


def _predicted_difference(
    case: dict[str, Any], operation: dict[str, Any], document: dict[str, Any]
) -> tuple[int, int, str] | None:
    """Classify from the generated case and schema before inspecting responses."""
    repeated = repeated_scalar_query_difference(case, operation)
    if repeated:
        return (400, 200, repeated)
    integer = _case_integer(case)
    if integer is None or -(1 << 63) <= integer <= (1 << 64) - 1:
        return None
    label = case.get("label", "")
    if label.startswith(("query:", "header:")):
        section, rest = label.split(":", 1)
        name = rest.rpartition(":")[0]
        parameter_in = "header" if section == "header" else "query"
        parameter = next((item for item in operation.get("parameters", [])
                          if item.get("in") == parameter_in
                          and item.get("name", "").lower() == name.lower()), None)
        if parameter and _integer_without_wire_bound(parameter.get("schema"), integer):
            return (400, 200, UNBOUNDED_PARAMETER_INTEGER_CAUSE)
        return None
    pointer = body_schema_pointer(document, operation, case)
    if pointer in INTEGER_NUMBER_UNION_POINTERS and _approved_integer_number_union_without_wire_bound(
        _schema_at_pointer(document, pointer), integer
    ):
        return (200, 200, INTEGER_NUMBER_UNION_CAUSE)
    if pointer == "#/components/schemas/JsonValue":
        return (200, 200, OPAQUE_JSON_INTEGER_CAUSE)
    python_body = _synthetic_body_for_numeric_case(case, integer)
    rust_body = _synthetic_body_for_numeric_case(case, float(integer))
    synthetic_row = lambda body: {"accepted": True, "status": 200, "value": {"body": body}}
    precision_cause = opaque_integer_difference(
        case, synthetic_row(rust_body), synthetic_row(python_body), operation, document
    )
    if precision_cause is not None:
        return (200, 200, precision_cause)
    schema = _schema_at_pointer(document, pointer)
    if _integer_without_wire_bound(schema, integer):
        return (200, 200, UNBOUNDED_INTEGER_CAUSE)
    return None


def parameter_schema_pointer(case: dict[str, Any], operation: dict[str, Any]) -> str | None:
    label = case.get("label", "")
    if not label.startswith(("query:", "header:")):
        return None
    section, rest = label.split(":", 1)
    name = rest.rpartition(":")[0]
    parameter_in = "header" if section == "header" else "query"
    parameter = next((item for item in operation.get("parameters", [])
                      if item.get("in") == parameter_in
                      and item.get("name", "").lower() == name.lower()), None)
    if parameter is None:
        return None
    index = operation["parameters"].index(parameter)
    escaped_path = case["path"].replace("~", "~0").replace("/", "~1")
    return f"#/paths/{escaped_path}/{case['method'].lower()}/parameters/{index}/schema"


def _normalize_allowed_integer_leaf(
    case: dict[str, Any], operation: dict[str, Any], document: dict[str, Any],
    rust_value: Any, python_value: Any, cause: str,
) -> Any | None:
    """Copy one known integer precision difference; preserve all sibling checks."""
    integer = _case_integer(case)
    if integer is None:
        return None
    pointer = body_schema_pointer(document, operation, case)
    if cause == INTEGER_NUMBER_UNION_CAUSE:
        if not (
            (integer < -(1 << 63) or integer > (1 << 64) - 1)
            and pointer in INTEGER_NUMBER_UNION_POINTERS
            and _approved_integer_number_union_without_wire_bound(
                _schema_at_pointer(document, pointer), integer
            )
        ):
            return None
    elif cause == OPAQUE_JSON_INTEGER_CAUSE:
        if pointer != "#/components/schemas/JsonValue":
            return None
    elif cause == UNBOUNDED_INTEGER_CAUSE:
        schema = _schema_at_pointer(document, pointer)
        if not _integer_without_wire_bound(schema, integer):
            return None
    else:
        return None
    if type(python_value) is not dict or type(rust_value) is not dict:
        return None
    pointer_parts = case_pointer(case).strip("/").split("/")
    field_path = pointer_parts[1:] if pointer_parts and pointer_parts[0] == "body" else pointer_parts
    py_node = python_value
    rust_node = rust_value
    for part in field_path[:-1]:
        if type(py_node) is dict and type(rust_node) is dict:
            py_node = py_node.get(part, _MISSING)
            rust_node = rust_node.get(part, _MISSING)
        elif (
            type(py_node) is list
            and type(rust_node) is list
            and part.isdecimal()
            and int(part) < len(py_node)
            and int(part) < len(rust_node)
        ):
            py_node = py_node[int(part)]
            rust_node = rust_node[int(part)]
        else:
            return None
    leaf = field_path[-1]
    if type(py_node) is dict and type(rust_node) is dict:
        python_leaf = py_node.get(leaf, _MISSING)
        rust_leaf = rust_node.get(leaf, _MISSING)
    elif (
        type(py_node) is list
        and type(rust_node) is list
        and leaf.isdecimal()
        and int(leaf) < len(py_node)
        and int(leaf) < len(rust_node)
    ):
        leaf_index = int(leaf)
        python_leaf = py_node[leaf_index]
        rust_leaf = rust_node[leaf_index]
    else:
        return None
    if not (type(python_leaf) is int and python_leaf == integer
            and type(rust_leaf) is float and rust_leaf == float(integer)):
        return None
    normalized = copy.deepcopy(python_value)
    # The same path can cross object properties and array indexes.
    node = normalized
    for part in field_path[:-1]:
        node = node[int(part)] if type(node) is list else node[part]
    if type(node) is list:
        node[int(leaf)] = rust_leaf
    else:
        node[leaf] = rust_leaf
    return normalized


def located_envelope(row: dict[str, Any]) -> bool:
    body = row.get("body")
    if not (
        isinstance(body, dict)
        and isinstance(body.get("error"), str)
    ):
        return False
    location = body.get("location")
    if not isinstance(location, list) or not location:
        details = body.get("details")
        location = details[0].get("location") if isinstance(details, list) and details and isinstance(details[0], dict) else None
    return row["status"] not in (400, 422) or isinstance(location, list) and bool(location)


def allowed_pre_tightening_python_boundary_envelope(
    case: dict[str, Any], rust_row: dict[str, Any], python_row: dict[str, Any]
) -> bool:
    """Allow only known request-boundary errors missing location on old Python."""
    if (
        rust_row.get("status") != 400
        or python_row.get("status") != 400
        or not isinstance(python_row.get("body"), dict)
        or set(python_row["body"]) != {"error"}
    ):
        return False
    error = python_row["body"].get("error")
    label = case.get("label", "")
    if label == "body:malformed":
        return error == "Invalid JSON"
    if label == "body:missing" and case.get("bodyPresent") is False:
        return error in {"JSON object required", "Invalid JSON"}
    if label.startswith(":type:") and case.get("bodyPresent") is True:
        try:
            raw_value = json.loads(case.get("raw", ""))
        except (TypeError, ValueError):
            return False
        return not isinstance(raw_value, dict) and error == "JSON object required"
    return False


def case_pointer(case: dict[str, Any]) -> str:
    label = case["label"]
    if label.startswith(("query:", "header:")):
        parts = label.split(":", 2)
        if len(parts) > 1 and parts[1] not in {"extra", "extra-blank", "extras", "missing"}:
            return "/" + parts[0] + "/" + parts[1].replace("~", "~0").replace("/", "~1")
    field_path = label.split(":", 1)[0]
    if field_path in {"baseline", "body", "query"}:
        return "/"
    return "/body/" + "/".join(
        segment.replace("~", "~0").replace("/", "~1")
        for segment in field_path.split("/")
    )


def body_schema_pointer(document: dict[str, Any], operation: dict[str, Any], case: dict[str, Any]) -> str:
    pointer = "#/paths/" + case["path"].replace("~", "~0").replace("/", "~1")
    pointer += "/" + case["method"].lower() + "/requestBody/content/application~1json/schema"
    schema: Any = operation.get("requestBody", {}).get("content", {}).get("application/json", {}).get("schema", {})
    components = document.get("components", {}).get("schemas", {})

    def resolve(value: Any) -> tuple[Any, str | None]:
        while isinstance(value, dict) and "$ref" in value:
            reference = value["$ref"]
            if not reference.startswith("#/components/schemas/"):
                break
            pointer_value = reference
            value = components.get(reference.rsplit("/", 1)[-1], {})
            return resolve(value)[0], pointer_value
        return value, None

    schema, referenced = resolve(schema)
    if referenced is not None:
        pointer = referenced
    label_path = case["label"].split(":", 1)[0]
    for segment in label_path.split("/"):
        if isinstance(schema, dict) and segment in schema.get("properties", {}):
            schema = schema["properties"][segment]
            pointer += "/properties/" + segment.replace("~", "~0").replace("/", "~1")
        elif (
            isinstance(schema, dict)
            and segment == "key"
            and isinstance(schema.get("additionalProperties"), dict)
        ):
            schema = schema["additionalProperties"]
            pointer += "/additionalProperties"
        elif isinstance(schema, dict) and schema.get("type") == "array" and segment.isdecimal():
            schema = schema.get("items", {})
            pointer += "/items"
        else:
            branches = schema.get("anyOf", schema.get("oneOf", [])) if isinstance(schema, dict) else []
            found = None
            for keyword in ("anyOf", "oneOf"):
                branches = schema.get(keyword, []) if isinstance(schema, dict) else []
                for index, branch in enumerate(branches):
                    resolved, _ = resolve(branch)
                    if segment in resolved.get("properties", {}) or (
                        segment.isdecimal() and resolved.get("type") == "array"
                    ):
                        found = (keyword, index, branch)
                        break
                if found:
                    break
            if not found:
                return pointer
            keyword, index, schema = found
            pointer += f"/{keyword}/{index}"
            schema, referenced = resolve(schema)
            if referenced is not None:
                pointer = referenced
            if segment in schema.get("properties", {}):
                schema = schema["properties"][segment]
                pointer += "/properties/" + segment.replace("~", "~0").replace("/", "~1")
            elif schema.get("type") == "array" and segment.isdecimal():
                schema = schema.get("items", {})
                pointer += "/items"
        schema, referenced = resolve(schema)
        if referenced is not None:
            pointer = referenced
    return pointer


def classify(cases: list[dict[str, Any]], rust: list[dict[str, Any]], python: list[dict[str, Any]], document: dict[str, Any]) -> dict[str, Any]:
    if not (len(cases) == len(rust) == len(python)):
        raise AssertionError(f"generated corpus lengths differ: cases={len(cases)}, Rust={len(rust)}, Python={len(python)}")
    allowances = dict(_EXPECTED_STATUS_DIFFERENCES)
    # Expand the pre-tightening unknown-query rule into exact operation/case
    # entries from this run's generated corpus; never store a wildcard.
    for case in cases:
        if not case["label"].startswith("query:extra"):
            continue
        operation_id = document["paths"][case["path"]][case["method"].lower()]["operationId"]
        if operation_id in _python_query_models:
            continue
        key = (operation_id, case["label"])
        if key in allowances:
            raise AssertionError(f"duplicate generated expected-difference entry: {key}")
        allowances[key] = (
            400,
            200,
            "temporary: pre-tightening Python ignores undeclared query names; expires when the global guard is in the baseline",
        )
    for key, expected in allowances.items():
        if (
            key[0] == "*"
            or not key[0]
            or not key[1]
            or len(expected) != 3
            or type(expected[0]) is not int
            or type(expected[1]) is not int
            or not isinstance(expected[2], str)
            or not expected[2].strip()
        ):
            raise AssertionError(f"invalid expected-difference entry {key}: {expected!r}")
    totals: dict[str, Any] = {
        "cases": len(cases),
        "accepted": 0,
        "rejected": 0,
        "mismatches": 0,
        "mismatches_by_cause": {},
        "mismatch_direction": {},
        "agreement_on_refusal_by_cause": {
            cause: 0 for cause in REQUIRED_PREDICTED_DIFFERENCE_CAUSES
        },
        "agreement_on_refusal_cases": [],
        "typed_integer_precision_fields": [],
        "opaque_json_precision_fields": [],
        "integer_number_union_fields": [],
        "expected_difference_cases": [],
        "temporary_envelope_cases": [],
    }
    repeated_predicates = 0
    predicted_cases_by_cause: dict[str, int] = {}
    expected_differences_by_predicted_cause: dict[str, int] = {}
    predicted_union_pointers: set[str] = set()
    for case, rust_row, python_row in zip(cases, rust, python, strict=True):
        operation_id = document["paths"][case["path"]][case["method"].lower()]["operationId"]
        key = (operation_id, case["label"])
        if "exception" in rust_row or "exception" in python_row:
            raise AssertionError(f"exception operation={operation_id} case={case['label']}: Rust={rust_row} Python={python_row}")
        for side, row in (("Rust", rust_row), ("Python", python_row)):
            if row["status"] >= 500:
                raise AssertionError(f"{side} returned {row['status']} operation={operation_id} case={case['label']}")
        if rust_row["accepted"]:
            totals["accepted"] += 1
        else:
            totals["rejected"] += 1
        if not rust_row["accepted"] and not located_envelope(rust_row):
            raise AssertionError(f"Rust refusal lacks its located error envelope operation={operation_id} case={case['label']}: {rust_row}")
        allowed_python_envelope = allowed_pre_tightening_python_boundary_envelope(
            case, rust_row, python_row
        )
        if not python_row["accepted"] and not located_envelope(python_row) and not allowed_python_envelope:
            raise AssertionError(f"Python refusal lacks its located error envelope operation={operation_id} case={case['label']}: {python_row}")
        operation = document["paths"][case["path"]][case["method"].lower()]
        statuses = (rust_row["status"], python_row["status"])
        predicted = _predicted_difference(case, operation, document)
        if predicted is not None and predicted[2] == REPEATED_SCALAR_QUERY_CAUSE:
            repeated_predicates += 1
        expected = allowances.get(key) or predicted
        predicted_class = predicted if key not in allowances else None
        if predicted_class is not None:
            cause = predicted_class[2]
            predicted_cases_by_cause[cause] = predicted_cases_by_cause.get(cause, 0) + 1
            if cause == INTEGER_NUMBER_UNION_CAUSE:
                predicted_union_pointers.add(body_schema_pointer(document, operation, case))
        agreement_refusal = (
            predicted_class is not None
            and statuses == (400, 400)
            and not rust_row["accepted"]
            and not python_row["accepted"]
            and located_envelope(rust_row)
        )
        if agreement_refusal:
            cause = predicted_class[2]
            totals["agreement_on_refusal_by_cause"][cause] = (
                totals["agreement_on_refusal_by_cause"].get(cause, 0) + 1
            )
            totals["agreement_on_refusal_cases"].append({
                "operation": operation_id,
                "pointer": (
                    body_schema_pointer(document, operation, case)
                    if case_pointer(case).startswith("/body/")
                    else case_pointer(case)
                ),
                "case": case["label"],
                "rust_status": statuses[0],
                "python_status": statuses[1],
                "cause": cause,
            })
            if cause in (UNBOUNDED_INTEGER_CAUSE, UNBOUNDED_PARAMETER_INTEGER_CAUSE):
                precision_field = {
                    "operation": operation_id,
                    "pointer": (
                        body_schema_pointer(document, operation, case)
                        if case_pointer(case).startswith("/body/")
                        else parameter_schema_pointer(case, operation) or case_pointer(case)
                    ),
                    "case": case["label"],
                    "cause": cause,
                    "agreement_on_refusal": True,
                }
                if precision_field not in totals["typed_integer_precision_fields"]:
                    totals["typed_integer_precision_fields"].append(precision_field)
            elif cause == OPAQUE_JSON_INTEGER_CAUSE:
                precision_field = {
                    "operation": operation_id,
                    "pointer": body_schema_pointer(document, operation, case),
                    "case": case["label"],
                    "cause": cause,
                    "agreement_on_refusal": True,
                }
                if precision_field not in totals["opaque_json_precision_fields"]:
                    totals["opaque_json_precision_fields"].append(precision_field)
            if cause == INTEGER_NUMBER_UNION_CAUSE:
                union_field = {
                    "operation": operation_id,
                    "pointer": body_schema_pointer(document, operation, case),
                }
                if union_field not in totals["integer_number_union_fields"]:
                    totals["integer_number_union_fields"].append(union_field)
            continue
        if predicted_class is not None and statuses != predicted_class[:2]:
            raise AssertionError(
                f"expected class {predicted_class[2]!r} predicts statuses {predicted_class[:2]} "
                f"or agreement-on-refusal (400, 400), operation={operation_id} "
                f"case={case['label']!r}, observed {statuses}"
            )
        if predicted_class is not None:
            cause = predicted_class[2]
            expected_differences_by_predicted_cause[cause] = (
                expected_differences_by_predicted_cause.get(cause, 0) + 1
            )
        if expected is not None and statuses != expected[:2]:
            raise AssertionError(
                f"expected class {expected[2]!r} predicts statuses {expected[:2]} "
                f"operation={operation_id} case={case['label']!r}, observed {statuses}"
            )
        parameter_pointer = None
        if expected is not None and expected[2] == UNBOUNDED_PARAMETER_INTEGER_CAUSE:
            parameter_difference = unbounded_parameter_integer_difference(
                case, rust_row, python_row, operation, document
            )
            if parameter_difference is None:
                raise AssertionError(f"unbounded parameter case failed its exact predicate: {operation_id} {case['label']}")
            parameter_pointer, cause = parameter_difference
            if cause != expected[2]:
                raise AssertionError(f"wrong parameter cause for {operation_id} {case['label']}: {cause}")
        mismatch = None
        normalized_python_value = None
        if rust_row["accepted"] and python_row["accepted"]:
            if expected is not None and expected[2] in (UNBOUNDED_INTEGER_CAUSE, INTEGER_NUMBER_UNION_CAUSE, OPAQUE_JSON_INTEGER_CAUSE):
                normalized_python_value = _normalize_allowed_integer_leaf(
                    case, operation, document,
                    rust_row.get("value", {}).get("body"),
                    python_row.get("value", {}).get("body"),
                    expected[2],
                )
                if normalized_python_value is None:
                    raise AssertionError(f"numeric allowance predicate did not match exact leaf: {operation_id} {case['label']}")
            compare_python_value = python_row.get("value")
            if normalized_python_value is not None:
                compare_python_value = copy.deepcopy(compare_python_value)
                compare_python_value["body"] = normalized_python_value
            if not strict_equal(rust_row.get("value"), compare_python_value):
                mismatch = f"accepted typed values differ: Rust={rust_row.get('value')!r}, Python={python_row.get('value')!r}"
        elif statuses[0] != statuses[1] or rust_row["accepted"] != python_row["accepted"]:
            mismatch = "status/acceptance differs"
        elif not rust_row["accepted"] and rust_row["status"] == python_row["status"] == 400 and (
            not located_envelope(rust_row) or (not located_envelope(python_row) and not allowed_python_envelope)
        ):
            mismatch = "validation rejection lacks the located error envelope"
        elif allowed_python_envelope and not located_envelope(python_row):
            mismatch = "pre-tightening Python request-boundary envelope lacks location"
        if expected is not None and expected[2] == "temporary: Python refusal without a location on the pre-tightening baseline":
            if not allowed_python_envelope:
                raise AssertionError(f"Python envelope class no longer matches {operation_id} {case['label']}")
            mismatch = "pre-tightening Python request-boundary envelope lacks location"
        elif mismatch is None and expected is None:
            continue
        elif mismatch is None and normalized_python_value is not None:
            mismatch = "accepted value differs only at the allowed numeric leaf"
        elif mismatch is None:
            raise AssertionError(f"expected difference was not observed: {operation_id} {case['label']} ({expected[2]})")
        elif normalized_python_value is not None and mismatch.startswith("accepted typed values differ"):
            raise AssertionError(
                f"numeric allowance has an unrelated value mismatch after leaf normalization: "
                f"{operation_id} {case['label']}: {mismatch}"
            )
        if mismatch == "pre-tightening Python request-boundary envelope lacks location":
            expected = (
                400,
                400,
                "temporary: Python refusal without a location on the pre-tightening baseline",
            )
        allowances.pop(key, None)
        actual = (statuses[0], statuses[1])
        if expected is None or expected[:2] != actual or rust_row["accepted"] != (actual[0] < 400) or python_row["accepted"] != (actual[1] < 400):
            raise AssertionError(f"unclassified differential operation={operation_id} case={case['label']!r} expected={expected} actual={actual}: {mismatch}")
        if "undeclared query" in expected[2]:
            from urllib.parse import parse_qsl
            names = [name for name, _value in parse_qsl(case.get("rawQuery", ""), keep_blank_values=True)]
            declared = {p.get("name") for p in operation.get("parameters", []) if p.get("in") == "query"}
            extras = {name for name in names if name not in declared}
            location = rust_row.get("body", {}).get("location")
            if not extras or not isinstance(location, list) or len(location) != 2 or location[0] != "query" or location[1] not in extras:
                raise AssertionError(f"unknown-query allowance requires exact Rust location ['query', name]: {operation_id} {case['label']} {rust_row}")
        if rust_row["accepted"] and python_row["accepted"]:
            direction = "accepted_value_mismatch"
        elif rust_row["accepted"]:
            direction = "rust_accept_python_reject"
        elif python_row["accepted"]:
            direction = "rust_reject_python_accept"
        else:
            direction = "rejection_envelope_mismatch"
        totals["mismatches"] += 1
        totals["mismatch_direction"][direction] = totals["mismatch_direction"].get(direction, 0) + 1
        cause = expected[2]
        totals["mismatches_by_cause"][cause] = totals["mismatches_by_cause"].get(cause, 0) + 1
        totals["expected_difference_cases"].append({
            "operation": operation_id,
            "pointer": body_schema_pointer(document, operation, case) if case_pointer(case).startswith("/body/") else case_pointer(case),
            "case": case["label"],
            "rust_status": statuses[0],
            "python_status": statuses[1],
            "cause": cause,
        })
        if cause == "temporary: Python refusal without a location on the pre-tightening baseline":
            totals["temporary_envelope_cases"].append({
                "operation": operation_id,
                "case": case["label"],
                "rust_status": statuses[0],
                "python_status": statuses[1],
                "python_error": python_row["body"].get("error"),
                "cause": cause,
            })
        if cause in (UNBOUNDED_INTEGER_CAUSE, OPAQUE_JSON_INTEGER_CAUSE):
            precision_field = {
                "operation": operation_id,
                "pointer": (
                    body_schema_pointer(document, operation, case)
                    if case_pointer(case).startswith("/body/")
                    else case_pointer(case)
                ),
                "case": case["label"],
                "cause": cause,
            }
            field_list = "typed_integer_precision_fields" if cause == UNBOUNDED_INTEGER_CAUSE else "opaque_json_precision_fields"
            totals[field_list].append(precision_field)
        elif cause == INTEGER_NUMBER_UNION_CAUSE:
            union_field = {
                "operation": operation_id,
                "pointer": body_schema_pointer(document, operation, case),
            }
            if union_field not in totals["integer_number_union_fields"]:
                totals["integer_number_union_fields"].append(union_field)
        elif cause == UNBOUNDED_PARAMETER_INTEGER_CAUSE:
            totals["typed_integer_precision_fields"].append({
                "operation": operation_id,
                "pointer": parameter_pointer,
                "case": case["label"],
                "cause": cause,
            })
    if len(cases) > 100 and repeated_predicates == 0:
        raise AssertionError("repeated scalar query predicate matched no generated cases")
    totals["predicted_cases_by_cause"] = {
        cause: predicted_cases_by_cause.get(cause, 0)
        for cause in REQUIRED_PREDICTED_DIFFERENCE_CAUSES
    }
    totals["expected_differences_by_predicted_cause"] = {
        cause: expected_differences_by_predicted_cause.get(cause, 0)
        for cause in REQUIRED_PREDICTED_DIFFERENCE_CAUSES
    }
    required_causes = (
        REQUIRED_PREDICTED_DIFFERENCE_CAUSES
        if len(cases) > 100
        else predicted_cases_by_cause.keys()
    )
    missing_expected_differences = sorted(
        cause for cause in required_causes
        if expected_differences_by_predicted_cause.get(cause, 0) == 0
    )
    if missing_expected_differences:
        raise AssertionError(
            "predicted differential classes had no case with their exact expected difference: "
            f"{missing_expected_differences}"
        )
    if len(cases) > 100 and predicted_union_pointers != INTEGER_NUMBER_UNION_POINTERS:
        raise AssertionError(
            "integer|number union allowlist fields were not all exercised: "
            f"missing={sorted(INTEGER_NUMBER_UNION_POINTERS - predicted_union_pointers)}, "
            f"unexpected={sorted(predicted_union_pointers - INTEGER_NUMBER_UNION_POINTERS)}"
        )
    if allowances:
        raise AssertionError(f"expected differential entries were unused: {sorted(allowances.items())[:10]}")
    return totals


def main() -> int:
    with tempfile.TemporaryDirectory(prefix="studio-http-request-corpus-") as temporary_dir:
        corpus_dir = Path(temporary_dir)
        generator = subprocess.run(
            [sys.executable, str(CORPUS_GENERATOR), str(corpus_dir)],
            cwd=ROOT, check=False, capture_output=True, text=True,
        )
        if generator.returncode:
            raise RuntimeError(generator.stderr + generator.stdout)
        corpus_path = corpus_dir / "openapi-request-corpus.json"
        cases: list[dict[str, Any]] = json.loads(corpus_path.read_text(encoding="utf-8"))
        rust = rust_observations(corpus_path)
        python = asyncio.run(python_observations(corpus_path))
        document = json.loads((ROOT / "workspaces/runtime/packages/runtime-http/openapi.json").read_text(encoding="utf-8"))
        totals = classify(cases, rust, python, document)
    detail_path = Path("/var/tmp/cs-rust-edge/http-runtime-differential-details.json")
    detail_path.write_text(json.dumps(totals, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    totals.pop("expected_difference_cases", None)
    print("generated extractor differential: " + json.dumps(totals, sort_keys=True))
    print(f"expected-difference detail: {detail_path}")
    return 0

if __name__ == "__main__":
    raise SystemExit(main())
