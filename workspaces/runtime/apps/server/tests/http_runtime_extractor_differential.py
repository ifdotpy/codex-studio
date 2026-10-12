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
    ("queue_action_api_queue_post", "id:type:NoneType", "rust_accept_python_reject"):
        "permanent Python model rule: studio_api.work.models.QueueCancelBody.has_message_identity",
    **{
        ("update_rules_api_rules_post", label, "rust_accept_python_reject"):
        "permanent Python model rule: studio_api.work.models.RuleBody.validate_save_kind"
        for label in (
            "at:type:int", "at:type:float", "at:lax:18446744073709551616",
            "at:lax:1267650600228229401496703205376", "at:lax:1.5",
            "at:minimum:below", "at:minimum:above", "at:maximum:below",
            "at:maximum:above", "at:maximum:equal",
        )
    },
}
# The authored declarations are terse directions; the runtime table stores an exact
# operation, case label, both HTTP statuses, and the reason for each exception.
_STATUS_BY_DIRECTION = {
    "rust_accept_python_reject": (200, 400),
    "rust_reject_python_accept": (400, 200),
    "status:400->422": (400, 422),
}
_EXPECTED_STATUS_DIFFERENCES: dict[tuple[str, str], tuple[int, int, str]] = {}
for (_operation, _label, _direction), _cause in _EXPECTED_DIRECTIONS.items():
    _rust_status, _python_status = _STATUS_BY_DIRECTION[_direction]
    _EXPECTED_STATUS_DIFFERENCES[(_operation, _label)] = (
        _rust_status, _python_status, _cause
    )
OPAQUE_JSON_INTEGER_CAUSE = "permanent: integer outside i64/u64 inside opaque JsonValue is carried as f64"
REQUIRED_PREDICTED_DIFFERENCE_CAUSES = frozenset({OPAQUE_JSON_INTEGER_CAUSE})


_MISSING = object()


def opaque_integer_difference(
    case: dict[str, Any], rust_row: dict[str, Any], python_row: dict[str, Any],
    operation: dict[str, Any], document: dict[str, Any],
) -> str | None:
    label = case.get("label", "")
    if (
        not rust_row.get("accepted") or not python_row.get("accepted")
        or rust_row.get("status") != 200 or python_row.get("status") != 200
        or ":lax:" not in label
        or body_schema_pointer(document, operation, case) != "#/components/schemas/JsonValue"
    ):
        return None
    try:
        integer = int(label.rsplit(":lax:", 1)[1])
    except ValueError:
        return None
    if -(1 << 63) <= integer <= (1 << 64) - 1:
        return None
    rust_value = rust_row.get("value", {}).get("body")
    python_value = python_row.get("value", {}).get("body")
    for segment in label.split(":lax:", 1)[0].split("/"):
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
    if type(rust_value) is float and type(python_value) is int and python_value == integer and rust_value == float(integer):
        return OPAQUE_JSON_INTEGER_CAUSE
    return None


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
                if not current_case.get().get("bodyPresent", True) or ("raw" in current_case.get() and not current_case.get()["raw"]): received.pop("body", None)
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


def schema_value_equal(left: Any, right: Any, schema: Any, document: dict[str, Any]) -> bool:
    """Compare echoed values with JSON Schema numeric semantics, strictly otherwise."""
    if not isinstance(schema, dict):
        return strict_equal(left, right)
    reference = schema.get("$ref")
    if isinstance(reference, str) and reference.startswith("#/components/schemas/"):
        target = document.get("components", {}).get("schemas", {}).get(reference.rsplit("/", 1)[-1])
        if target is not None and schema_value_equal(left, right, target, document):
            return True
    for keyword in ("anyOf", "oneOf"):
        branches = schema.get(keyword)
        if isinstance(branches, list):
            return any(schema_value_equal(left, right, branch, document) for branch in branches)
    kind = schema.get("type")
    if kind == "number":
        return (
            type(left) in (int, float) and type(right) in (int, float)
            and left == right
        )
    if kind == "integer":
        def is_integral(value: Any) -> bool:
            return type(value) is int or type(value) is float and value.is_integer()

        return (
            type(left) in (int, float) and type(right) in (int, float)
            and is_integral(left) and is_integral(right) and left == right
        )
    if kind == "array" and type(left) is list and type(right) is list:
        item_schema = schema.get("items")
        return len(left) == len(right) and all(
            schema_value_equal(a, b, item_schema, document)
            for a, b in zip(left, right, strict=True)
        )
    if kind == "object" and type(left) is dict and type(right) is dict:
        if left.keys() != right.keys():
            return False
        properties = schema.get("properties", {})
        additional = schema.get("additionalProperties")
        return all(
            schema_value_equal(
                left[key], right[key],
                properties.get(key, additional) if isinstance(properties, dict) else additional,
                document,
            )
            for key in left
        )
    return strict_equal(left, right)


def observation_values_equal(
    rust: dict[str, Any], python: dict[str, Any], operation: dict[str, Any],
    document: dict[str, Any],
) -> bool:
    rust_value, python_value = rust.get("value"), python.get("value")
    if not isinstance(rust_value, dict) or not isinstance(python_value, dict):
        return strict_equal(rust_value, python_value)
    if rust_value.keys() != python_value.keys():
        return False
    parameters = operation.get("parameters", [])
    for section, wire_section in (("query", "query"), ("headers", "header")):
        left, right = rust_value.get(section, {}), python_value.get(section, {})
        if type(left) is not dict or type(right) is not dict or left.keys() != right.keys():
            return False
        for name in left:
            parameter = next((p for p in parameters if p.get("in") == wire_section and p.get("name", "").lower() == name.lower()), None)
            if parameter is None or not schema_value_equal(left[name], right[name], parameter.get("schema"), document):
                return False
    body_schema = operation.get("requestBody", {}).get("content", {}).get("application/json", {}).get("schema")
    for section in rust_value:
        if section not in {"query", "headers"} and not schema_value_equal(rust_value[section], python_value[section], body_schema, document):
            return False
    return True


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
    """Predict the one permanent JsonValue numeric representation difference."""
    integer = _case_integer(case)
    if integer is None or -(1 << 63) <= integer <= (1 << 64) - 1:
        return None
    if body_schema_pointer(document, operation, case) == "#/components/schemas/JsonValue":
        return (200, 200, OPAQUE_JSON_INTEGER_CAUSE)
    return None


def _normalize_allowed_integer_leaf(
    case: dict[str, Any], operation: dict[str, Any], document: dict[str, Any],
    rust_value: Any, python_value: Any, cause: str,
) -> Any | None:
    """Normalize only the documented opaque JsonValue integer precision leaf."""
    if cause != OPAQUE_JSON_INTEGER_CAUSE or body_schema_pointer(document, operation, case) != "#/components/schemas/JsonValue":
        return None
    integer = _case_integer(case)
    if integer is None or -(1 << 63) <= integer <= (1 << 64) - 1:
        return None
    if type(python_value) is not dict or type(rust_value) is not dict:
        return None
    path = case["label"].split(":lax:", 1)[0].split("/")
    py_node: Any = python_value
    rust_node: Any = rust_value
    for part in path[:-1]:
        if type(py_node) is dict and type(rust_node) is dict:
            py_node = py_node.get(part, _MISSING)
            rust_node = rust_node.get(part, _MISSING)
        elif type(py_node) is list and type(rust_node) is list and part.isdecimal() and int(part) < len(py_node) and int(part) < len(rust_node):
            py_node = py_node[int(part)]
            rust_node = rust_node[int(part)]
        else:
            return None
    leaf = path[-1]
    if type(py_node) is dict and type(rust_node) is dict:
        python_leaf = py_node.get(leaf, _MISSING)
        rust_leaf = rust_node.get(leaf, _MISSING)
    elif type(py_node) is list and type(rust_node) is list and leaf.isdecimal() and int(leaf) < len(py_node) and int(leaf) < len(rust_node):
        python_leaf = py_node[int(leaf)]
        rust_leaf = rust_node[int(leaf)]
    else:
        return None
    if not (type(python_leaf) is int and python_leaf == integer and type(rust_leaf) is float and rust_leaf == float(integer)):
        return None
    normalized = copy.deepcopy(python_value)
    node: Any = normalized
    for part in path[:-1]:
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


def classify(
    cases: list[dict[str, Any]], rust: list[dict[str, Any]],
    python: list[dict[str, Any]], document: dict[str, Any],
) -> dict[str, Any]:
    if not (len(cases) == len(rust) == len(python)):
        raise AssertionError(f"generated corpus lengths differ: cases={len(cases)}, Rust={len(rust)}, Python={len(python)}")
    allowances = dict(_EXPECTED_STATUS_DIFFERENCES)
    for key, expected in allowances.items():
        if not key[0] or not key[1] or "*" in key[0] or "*" in key[1] or len(expected) != 3 or type(expected[0]) is not int or type(expected[1]) is not int or not isinstance(expected[2], str) or not expected[2].strip():
            raise AssertionError(f"invalid expected-difference entry {key}: {expected!r}")
    totals: dict[str, Any] = {
        "cases": len(cases), "accepted": 0, "rejected": 0, "mismatches": 0,
        "mismatches_by_cause": {}, "mismatch_direction": {},
        "predicted_cases_by_cause": {OPAQUE_JSON_INTEGER_CAUSE: 0},
        "agreement_on_refusal_by_cause": {OPAQUE_JSON_INTEGER_CAUSE: 0},
        "agreement_on_refusal_cases": [], "opaque_json_precision_fields": [],
        "expected_difference_cases": [],
    }
    exact_predicted_differences = {OPAQUE_JSON_INTEGER_CAUSE: 0}
    for case, rust_row, python_row in zip(cases, rust, python, strict=True):
        operation = document["paths"][case["path"]][case["method"].lower()]
        operation_id = operation["operationId"]
        key = (operation_id, case["label"])
        if "exception" in rust_row or "exception" in python_row:
            raise AssertionError(f"exception operation={operation_id} case={case['label']}: Rust={rust_row} Python={python_row}")
        for side, row in (("Rust", rust_row), ("Python", python_row)):
            if row["status"] >= 500:
                raise AssertionError(f"{side} returned {row['status']} operation={operation_id} case={case['label']}")
        statuses = (rust_row["status"], python_row["status"])
        if rust_row["accepted"]:
            totals["accepted"] += 1
        else:
            totals["rejected"] += 1
        if not rust_row["accepted"] and not located_envelope(rust_row):
            raise AssertionError(f"Rust refusal lacks its located error envelope operation={operation_id} case={case['label']}: {rust_row}")
        if not python_row["accepted"] and not located_envelope(python_row):
            raise AssertionError(f"Python refusal lacks its located error envelope operation={operation_id} case={case['label']}: {python_row}")

        predicted = _predicted_difference(case, operation, document)
        expected = allowances.get(key) or predicted
        if predicted is not None:
            cause = predicted[2]
            totals["predicted_cases_by_cause"][cause] += 1
            if statuses == (400, 400) and not rust_row["accepted"] and not python_row["accepted"]:
                totals["agreement_on_refusal_by_cause"][cause] += 1
                totals["agreement_on_refusal_cases"].append({
                    "operation": operation_id, "pointer": body_schema_pointer(document, operation, case),
                    "case": case["label"], "rust_status": statuses[0],
                    "python_status": statuses[1], "cause": cause,
                })
                continue
            if statuses != predicted[:2]:
                raise AssertionError(f"expected class {cause!r} predicts statuses {predicted[:2]} or agreement-on-refusal (400, 400), operation={operation_id} case={case['label']!r}, observed {statuses}")
            normalized_body = _normalize_allowed_integer_leaf(
                case, operation, document, rust_row.get("value", {}).get("body"),
                python_row.get("value", {}).get("body"), cause,
            )
            if normalized_body is None:
                raise AssertionError(f"opaque JsonValue precision predicate did not match the exact leaf: {operation_id} {case['label']}")
            normalized_python = copy.deepcopy(python_row.get("value"))
            normalized_python["body"] = normalized_body
            if not strict_equal(rust_row.get("value"), normalized_python):
                raise AssertionError(f"opaque JsonValue allowance has an unrelated value mismatch after normalization: {operation_id} {case['label']}: Rust={rust_row.get('value')!r}, Python={python_row.get('value')!r}")
            exact_predicted_differences[cause] += 1
            totals["opaque_json_precision_fields"].append({
                "operation": operation_id, "pointer": body_schema_pointer(document, operation, case),
                "case": case["label"], "cause": cause,
            })
            totals["mismatches"] += 1
            totals["mismatches_by_cause"][cause] = totals["mismatches_by_cause"].get(cause, 0) + 1
            totals["mismatch_direction"]["accepted_value_mismatch"] = totals["mismatch_direction"].get("accepted_value_mismatch", 0) + 1
            totals["expected_difference_cases"].append({
                "operation": operation_id, "pointer": body_schema_pointer(document, operation, case),
                "case": case["label"], "rust_status": statuses[0],
                "python_status": statuses[1], "cause": cause,
            })
            continue

        if expected is None:
            if statuses[0] != statuses[1] or rust_row["accepted"] != python_row["accepted"]:
                raise AssertionError(f"unclassified differential operation={operation_id} case={case['label']!r}: Rust status={statuses[0]} accepted={rust_row['accepted']}; Python status={statuses[1]} accepted={python_row['accepted']}")
            if rust_row["accepted"] and not observation_values_equal(rust_row, python_row, operation, document):
                raise AssertionError(f"unclassified typed-value mismatch operation={operation_id} case={case['label']!r}: Rust={rust_row.get('value')!r} Python={python_row.get('value')!r}")
            continue
        if statuses != expected[:2]:
            raise AssertionError(f"expected permanent rule {expected[2]!r} requires statuses {expected[:2]}, operation={operation_id} case={case['label']!r}, observed {statuses}")
        if rust_row["accepted"] == python_row["accepted"]:
            raise AssertionError(f"expected permanent rule did not differ in acceptance: {operation_id} {case['label']}")
        allowances.pop(key)
        totals["mismatches"] += 1
        cause = expected[2]
        totals["mismatches_by_cause"][cause] = totals["mismatches_by_cause"].get(cause, 0) + 1
        direction = "rust_accept_python_reject" if rust_row["accepted"] else "rust_reject_python_accept"
        totals["mismatch_direction"][direction] = totals["mismatch_direction"].get(direction, 0) + 1
        totals["expected_difference_cases"].append({
            "operation": operation_id, "pointer": case_pointer(case), "case": case["label"],
            "rust_status": statuses[0], "python_status": statuses[1], "cause": cause,
        })
    if allowances:
        raise AssertionError(f"expected differential entries were unused: {sorted(allowances.items())}")
    missing_classes = [cause for cause, count in totals["predicted_cases_by_cause"].items() if count and exact_predicted_differences.get(cause, 0) == 0]
    if missing_classes:
        raise AssertionError(f"predicted differential classes had no case with their exact expected difference: {missing_classes}")
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
