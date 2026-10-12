"""Report request validation rules missing from the exported OpenAPI schemas."""
from __future__ import annotations

import ast
import json
import math
from collections.abc import Iterator, Mapping, Sequence
from pathlib import Path
from typing import Annotated, Literal, get_args, get_origin, get_type_hints

from fastapi.routing import APIRoute
from starlette.routing import BaseRoute
from pydantic import BaseModel, RootModel, TypeAdapter
from pydantic.fields import FieldInfo

from studio_api.app import create_app
from studio_api.context import ApiContext
from studio_api.schema import openapi_document


# A JSON Schema cannot express the UTF-8 byte count of a string. Keep this
# validator in Python until the API uses a byte-oriented field representation.
SCHEMA_EXCEPTIONS = {
    ("studio_api.agents.models.ConversationRequest", "validate_excluded_account_keys"): (
        "The Pydantic field validator rejects duplicate array members before dispatch. "
        "Pydantic does not infer JSON Schema uniqueItems from a list validator, so the "
        "field publishes uniqueItems alongside this named runtime validator."
    ),
    ("studio_api.accounts.models.AccountNameRequest", "require_unicode_scalar_label"): (
        "JSON Schema string constraints do not portably reject lone UTF-16 surrogate escapes. "
        "The smallest declarative form would be a standard Unicode-scalar format assertion."
    ),
    ("studio_api.io.models.TerminalInput", "validate_utf8_size"): (
        "UTF-8 byte length is not a JSON Schema string constraint; maxLength "
        "counts Unicode characters. The smallest schema form is maxLength."
    ),
    ("studio_api.voice.models.UnknownVoiceActionBody", "extra_policy"): (
        "This compatibility catch-all route is excluded from OpenAPI and always "
        "returns an unknown-action error. It has no accepted request contract."
    ),
    ("studio_api.sync.models.DraftPushRequest", "validate_identities"): (
        "The payload is JSON serialized inside a string, so its parsed identity must "
        "match the row key. The smallest declarative form is to send the payload as "
        "an object and constrain its identity fields alongside the row id."
    ),
    ("studio_api.sync.models.DraftPushRow", "validate_payload"): (
        "The request embeds DraftPayload as serialized JSON text. The smallest "
        "declarative form is a typed object payload instead of a JSON string."
    ),
    ("studio_api.sync.models.DraftPushRow", "validate_assumed_payload"): (
        "The request embeds arbitrary JSON as serialized text. The smallest "
        "declarative form is a JSON value payload instead of a JSON string."
    ),
    ("studio_api.sync.preferences.UiPreferencesDto", "validate_fields"): (
        "Preference keys select different value schemas and scopes, a cross-field "
        "condition. The smallest declarative form is a discriminated preference "
        "entry union with typed key and value fields."
    ),
    ("studio_api.sync.preferences.PreferencePushRequest", "validate_scope"): (
        "Whether a key belongs to a scope is cross-field. The smallest declarative "
        "form is a discriminated union with scope-specific key patterns."
    ),
    ("studio_api.sync.preferences.PreferencePushRequest", "validate_fields"): (
        "Preference map keys choose different field and value rules. The smallest "
        "declarative form is a typed preference-entry union with key and value "
        "properties instead of a dynamically keyed map."
    ),
    ("studio_api.work.models.QueueCancelBody", "has_message_identity"): (
        "At least one of id and message_id is required. The smallest declarative "
        "form is anyOf with each property required."
    ),
    ("studio_api.work.models.QueueEditBody", "has_message_identity"): (
        "At least one of id and message_id is required. The smallest declarative "
        "form is anyOf with each property required."
    ),
    ("studio_api.work.models.QueueFirstBody", "has_message_identity"): (
        "Identity is required and send_now also requires request_id. The smallest "
        "declarative form is if/then plus anyOf required properties."
    ),
    ("studio_api.agents.models.RenameRequest", "smart_rename_identity"): (
        "A generated name requires request_id when name is absent. The smallest "
        "declarative form is dependentRequired."
    ),
    ("studio_api.voice.models.VoiceRecordRequest", "validate_record_payload"): (
        "Payload and text requirements depend on kind and nested payload values. "
        "The smallest declarative form is a kind-discriminated record union with "
        "typed payload models."
    ),
}

# Handler coverage is manual: this script does not detect an unlisted handler
# check. For each refresh, grep router modules and helpers for HTTPException,
# explicit 4xx status/status_code values, ValueError/TypeError raises and
# catches mapped to 4xx, Request query/header/body reads, and request-dependent
# return branches. Then classify each site as schema-covered, state/business/
# authorization behavior, or one of the entries below. Python's AST cannot
# reliably connect a helper's conditional response to its caller's route.
HANDLER_EXCEPTIONS = (
    {
        "kind": "handler_validation",
        "model": "POST /api/action",
        "name": "durable request ID for native actions",
        "operations": "POST /api/action",
        "exception": (
            "Review and compact require request_id, while the safety-action variant does not. "
            "This cross-field rule is schema-expressible as an action-discriminated union or if/then "
            "requiring request_id for review and compact."
        ),
    },
    {
        "kind": "handler_validation",
        "model": "GET /api/sync/stream",
        "name": "protocol version negotiation",
        "operations": "GET /api/sync/stream",
        "exception": (
            "The optional protocol query and X-Codex-Sync-Protocol header are intentionally plain strings. "
            "The handler accepts protocol 3 (or no version on the legacy path) and returns status 426 "
            "with supportedVersions for any other value; version negotiation is an HTTP behavior whose "
            "status and response body must remain explicit."
        ),
    },
    {
        "kind": "handler_validation",
        "model": "GET /api/workspace/tasks",
        "name": "task-feed JSON cursor strings",
        "operations": "GET /api/workspace/tasks",
        "exception": (
            "cursor and before are JSON-encoded strings parsed by the handler into structured cursors. "
            "Replacing them with typed query objects would change the established query wire format."
        ),
    },
    {
        "kind": "handler_validation",
        "model": "GET /api/sync/stream",
        "name": "resources JSON query payload",
        "operations": "GET /api/sync/stream",
        "exception": (
            "The resources parameter contains JSON encoded inside a string; the handler parses it, "
            "validates ResourceRef entries, bounds its UTF-8 size and enforces a non-empty maximum "
            "item count. The smallest schema form is a typed repeated query array with minItems/maxItems, "
            "which requires changing the current JSON-string wire format."
        ),
    },
    {
        "kind": "handler_validation",
        "model": "GET /api/messages/receipts",
        "name": "ids JSON query payload",
        "operations": "GET /api/messages/receipts",
        "exception": (
            "The ids parameter contains a JSON encoded string that is parsed as a list of receipt IDs. "
            "The smallest schema form is a repeated query array of receipt IDs, which requires changing "
            "the current JSON-string wire format."
        ),
    },
    {
        "kind": "handler_validation",
        "model": "GET /api/monitor/log",
        "name": "Range header satisfiability",
        "operations": "GET /api/monitor/log",
        "exception": (
            "Range is an optional plain string; malformed, unsatisfiable, or out-of-file byte ranges "
            "receive 416 based on HTTP byte-range semantics and the current log size. The status is "
            "documented on this operation."
        ),
    },
    {
        "kind": "handler_validation",
        "model": "POST /api/federation/v1/{pair,status,message,pull}",
        "name": "signed request framing",
        "operations": "POST /api/federation/v1/pair, /status, /message, /pull",
        "exception": (
            "The raw signed body must have Content-Type application/json, Content-Length 1..262144, and "
            "a body byte count matching that length before signature verification. A JSON Schema cannot "
            "constrain serialized request byte length or match it to Content-Length; the smallest "
            "declarative form is an HTTP request-body byte limit plus typed JSON content."
        ),
    },
)


def _model_types(annotation: object, seen: set[type[BaseModel]]) -> set[type[BaseModel]]:
    if isinstance(annotation, type) and issubclass(annotation, BaseModel):
        if issubclass(annotation, RootModel):
            return set()
        if annotation in seen:
            return set()
        seen.add(annotation)
        models = {annotation}
        for field in annotation.model_fields.values():
            models.update(_model_types(field.annotation, seen))
        return models
    result: set[type[BaseModel]] = set()
    for argument in get_args(annotation):
        result.update(_model_types(argument, seen))
    return result


def _contains_numeric(annotation: object) -> bool:
    return annotation in (int, float) or any(
        argument in (int, float) or _contains_numeric(argument)
        for argument in get_args(annotation)
    )


def _has_integer_literal(annotation: object) -> bool:
    if get_origin(annotation) is Literal and any(
        isinstance(value, int) and not isinstance(value, bool)
        for value in get_args(annotation)
    ):
        return True
    return any(_has_integer_literal(argument) for argument in get_args(annotation))


def _schema_contains_key(schema: object, key: str) -> bool:
    if isinstance(schema, dict):
        return key in schema or any(
            _schema_contains_key(value, key) for value in schema.values()
        )
    if isinstance(schema, list):
        return any(_schema_contains_key(value, key) for value in schema)
    return False


def _schema_contains_type(schema: object, *types: str) -> bool:
    if isinstance(schema, dict):
        return schema.get("type") in types or any(
            _schema_contains_type(value, *types) for value in schema.values()
        )
    if isinstance(schema, list):
        return any(_schema_contains_type(value, *types) for value in schema)
    return False


def _router_routes(routes: Sequence[BaseRoute]) -> Iterator[APIRoute]:
    for route in routes:
        if isinstance(route, APIRoute):
            yield route
        else:
            nested = getattr(route, "original_router", None)
            nested_routes = getattr(nested, "routes", None)
            if isinstance(nested_routes, Sequence):
                yield from _router_routes(nested_routes)


def request_models() -> dict[type[BaseModel], set[str]]:
    """Find request models from actual endpoint dependencies and parameters."""
    app = create_app(ApiContext.for_schema())
    result: dict[type[BaseModel], set[str]] = {}

    def visit(dependant: object, operation: str, visited: set[int]) -> None:
        if id(dependant) in visited:
            return
        visited.add(id(dependant))
        for name in ("body_params", "query_params", "path_params", "header_params", "cookie_params"):
            for parameter in getattr(dependant, name, ()):
                annotation = parameter.field_info.annotation
                for model in _model_types(annotation, set()):
                    result.setdefault(model, set()).add(operation)
        for child in getattr(dependant, "dependencies", ()):
            visit(child, operation, visited)

    for route in _router_routes(app.routes):
        for method in sorted(route.methods or ()):
            operation = f"{method} {route.path}"
            visit(route.dependant, operation, set())
            if method.upper() == "POST":
                from studio_api.federation.router import SIGNED_REQUEST_OPERATION_MODELS

                signed_model = SIGNED_REQUEST_OPERATION_MODELS.get(route.path)
                if signed_model is not None:
                    result.setdefault(signed_model, set()).add(operation)
    return result


def _validator_findings(model: type[BaseModel]) -> list[dict[str, str]]:
    findings: list[dict[str, str]] = []
    decorators = model.__pydantic_decorators__
    for kind, validators in (
        ("field_validator", decorators.field_validators),
        ("model_validator", decorators.model_validators),
    ):
        for name in validators:
            key = (f"{model.__module__}.{model.__qualname__}", name)
            reason = SCHEMA_EXCEPTIONS.get(key)
            finding = {
                "kind": kind,
                "model": key[0],
                "name": name,
            }
            if reason is not None:
                finding["exception"] = reason
            findings.append(finding)
    return findings


_CONSTRAINT_KEYS = (
    "minLength", "maxLength", "pattern", "format", "minimum", "maximum",
    "exclusiveMinimum", "exclusiveMaximum", "minItems", "maxItems",
    "uniqueItems", "multipleOf", "enum", "const",
)

_JSON_SCHEMA_EXTRA_VALIDATION_KEYS = frozenset(
    {
        "pattern", "minimum", "maximum", "exclusiveMinimum", "exclusiveMaximum",
        "minLength", "maxLength", "minItems", "maxItems", "minProperties",
        "maxProperties", "minContains", "maxContains", "multipleOf", "uniqueItems",
        "additionalProperties", "unevaluatedProperties", "enum", "const", "format",
        "required",
    }
)


def _json_schema_extra_findings(
    source_root: Path | None = None,
) -> tuple[list[dict[str, str]], dict[str, int]]:
    """Reject validation constraints injected through schema-only metadata.

    Constraint metadata must come from the same Pydantic type that validates
    requests and responses. Schema-only additions are inventoried here so a
    future json_schema_extra cannot silently tighten the generated contract.
    """
    root = source_root or Path(__file__).resolve().parent
    findings: list[dict[str, str]] = []
    uses = 0
    validation_uses = 0
    for source_path in root.rglob("*.py"):
        relative_path = source_path.relative_to(root)
        if "tests" in relative_path.parts or "benchmarks" in relative_path.parts:
            continue
        try:
            tree = ast.parse(source_path.read_text(encoding="utf-8"), filename=str(relative_path))
        except (OSError, SyntaxError) as error:
            findings.append({"kind": "unreadable_schema_extra_source", "source": str(relative_path),
                             "detail": type(error).__name__})
            continue
        for node in ast.walk(tree):
            if not isinstance(node, ast.Call):
                continue
            for keyword in node.keywords:
                if keyword.arg != "json_schema_extra":
                    continue
                uses += 1
                if not isinstance(keyword.value, ast.Dict):
                    findings.append({"kind": "uninspectable_schema_extra", "source": str(relative_path),
                                     "line": str(node.lineno)})
                    continue
                keys = {
                    key.value
                    for key in keyword.value.keys
                    if isinstance(key, ast.Constant) and isinstance(key.value, str)
                }
                validation_keys = keys & _JSON_SCHEMA_EXTRA_VALIDATION_KEYS
                if validation_keys:
                    validation_uses += 1
                    # The array's Pydantic field validator enforces uniqueness
                    # before returning the list. Keep this schema hint paired to
                    # that named validator, and let the HTTP/unit regression
                    # prove the runtime behavior.
                    validator_paired = (
                        relative_path.as_posix() == "agents/models.py"
                        and validation_keys == {"uniqueItems"}
                        and any(
                            isinstance(candidate, (ast.FunctionDef, ast.AsyncFunctionDef))
                            and candidate.name == "validate_excluded_account_keys"
                            and any(
                                isinstance(decorator, ast.Call)
                                and isinstance(decorator.func, ast.Name)
                                and decorator.func.id == "field_validator"
                                and any(
                                    isinstance(argument, ast.Constant)
                                    and argument.value == "excluded_account_keys"
                                    for argument in decorator.args
                                )
                                for decorator in candidate.decorator_list
                            )
                            for candidate in ast.walk(tree)
                        )
                    )
                    if validator_paired:
                        continue
                    findings.append({
                        "kind": "schema_only_validation_constraint",
                        "source": str(relative_path),
                        "line": str(node.lineno),
                        "keywords": ", ".join(sorted(validation_keys)),
                    })
    return findings, {"uses": uses, "validationUses": validation_uses}


def _resolve_local_schema(schema: object, root: object, *, expected: bool) -> object:
    if not isinstance(schema, dict) or not isinstance(root, dict):
        return schema
    reference = schema.get("$ref")
    if not isinstance(reference, str):
        return schema
    prefix = "#/$defs/" if expected else "#/components/schemas/"
    key = reference.removeprefix(prefix) if reference.startswith(prefix) else None
    definitions = root.get("$defs") if expected else root.get("components", {}).get("schemas")
    return definitions.get(key, schema) if key and isinstance(definitions, dict) else schema


def _schema_constraints(schema: object, root: object, *, expected: bool) -> dict[str, set[str]]:
    """Collect constraints through nullable unions, refs, and container items."""
    found: dict[str, set[str]] = {}
    visited: set[str] = set()

    def canonical(value: object) -> object:
        if isinstance(value, bool):
            return value
        if isinstance(value, (int, float)):
            return float(value)
        if isinstance(value, list):
            return [canonical(item) for item in value]
        if isinstance(value, dict):
            return {key: canonical(item) for key, item in value.items()}
        return value

    def visit(value: object) -> None:
        if isinstance(value, dict):
            reference = value.get("$ref")
            if isinstance(reference, str):
                if reference in visited:
                    return
                visited.add(reference)
                value = _resolve_local_schema(value, root, expected=expected)
                if not isinstance(value, dict):
                    return
            for key in _CONSTRAINT_KEYS:
                if key in value:
                    found.setdefault(key, set()).add(
                        json.dumps(canonical(value[key]), sort_keys=True)
                    )
            for child in value.values():
                visit(child)
        elif isinstance(value, list):
            for child in value:
                visit(child)

    visit(schema)
    return found


def _contradictory_union_findings(value: object, location: str = "openapi") -> list[dict[str, str]]:
    """Find a declared JSON type that every explicit union branch excludes."""
    findings: list[dict[str, str]] = []
    if isinstance(value, dict):
        declared = value.get("type")
        declared_types = (
            {item for item in declared if isinstance(item, str)}
            if isinstance(declared, list)
            else {declared} if isinstance(declared, str) else set()
        )
        if declared_types:
            for keyword in ("anyOf", "oneOf"):
                branches = value.get(keyword)
                if not isinstance(branches, list) or not branches:
                    continue
                branch_type_sets: list[set[str]] = []
                for branch in branches:
                    branch_type = branch.get("type") if isinstance(branch, dict) else None
                    if branch_type is None:
                        branch_type_sets = []
                        break
                    branch_types = (
                        {item for item in branch_type if isinstance(item, str)}
                        if isinstance(branch_type, list)
                        else {branch_type} if isinstance(branch_type, str) else set()
                    )
                    if not branch_types:
                        branch_type_sets = []
                        break
                    branch_type_sets.append(branch_types)
                if branch_type_sets and all(not (declared_types & branch_types) for branch_types in branch_type_sets):
                    findings.append({
                        "kind": "type_excluded_by_union",
                        "location": location,
                        "keyword": keyword,
                    })
        for key, child in value.items():
            findings.extend(_contradictory_union_findings(child, f"{location}/{key}"))
    elif isinstance(value, list):
        for index, child in enumerate(value):
            findings.extend(_contradictory_union_findings(child, f"{location}/{index}"))
    return findings


def _unbounded_integer_findings(value: object, location: str = "openapi") -> list[dict[str, str]]:
    """Ensure every explicit integer schema branch has both finite bounds."""
    findings: list[dict[str, str]] = []
    if isinstance(value, dict):
        def bounded_number(bound: object) -> bool:
            return (
                isinstance(bound, (int, float))
                and not isinstance(bound, bool)
                and math.isfinite(bound)
                and float(bound).is_integer()
            )

        if value.get("type") == "integer" and not (
            bounded_number(value.get("minimum"))
            and bounded_number(value.get("maximum"))
        ):
            findings.append({"kind": "unbounded_integer_schema", "location": location})
        elif value.get("type") == "integer":
            minimum = value.get("minimum")
            maximum = value.get("maximum")
            if (
                isinstance(minimum, (int, float))
                and not isinstance(minimum, bool)
                and minimum < -9_007_199_254_740_991
            ) or (
                isinstance(maximum, (int, float))
                and not isinstance(maximum, bool)
                and maximum > 9_007_199_254_740_991
            ):
                findings.append({"kind": "integer_outside_safe_range", "location": location})
        for key, child in value.items():
            findings.extend(_unbounded_integer_findings(child, f"{location}/{key}"))
    elif isinstance(value, list):
        for index, child in enumerate(value):
            findings.extend(_unbounded_integer_findings(child, f"{location}/{index}"))
    return findings


def _constraint_findings(
    model: type[BaseModel], component: object, operations: set[str], document: Mapping[str, object]
) -> list[dict[str, str]]:
    """Find field constraints or discriminators lost by the OpenAPI exporter."""
    exported_properties: dict[str, object] = {}
    if isinstance(component, dict):
        raw_properties = component.get("properties", {})
        if isinstance(raw_properties, dict):
            exported_properties.update(raw_properties)
    else:
        paths = document.get("paths")
        if not isinstance(paths, dict):
            return []
        for operation in operations:
            method, _, path = operation.partition(" ")
            path_item = paths.get(path)
            operation_item = path_item.get(method.lower()) if isinstance(path_item, dict) else None
            parameters = operation_item.get("parameters", ()) if isinstance(operation_item, dict) else ()
            for parameter in parameters if isinstance(parameters, list) else ():
                if isinstance(parameter, dict) and parameter.get("in") == "query":
                    name = parameter.get("name")
                    schema = parameter.get("schema")
                    if isinstance(name, str) and isinstance(schema, dict):
                        exported_properties[name] = schema
        if not exported_properties:
            if all(
                path not in paths or not isinstance(paths.get(path), dict)
                or method.lower() not in paths[path]
                for method, _, path in (item.partition(" ") for item in operations)
            ):
                return []
            return [{"kind": "missing_component", "model": model.__name__,
                     "operations": ", ".join(sorted(operations))}]
    try:
        expected = model.model_json_schema()
    except (TypeError, ValueError):
        return [{"kind": "unreadable_model_schema", "model": model.__name__,
                 "operations": ", ".join(sorted(operations))}]
    findings: list[dict[str, str]] = []
    expected_properties = expected.get("properties", {})
    if not isinstance(expected_properties, dict):
        return findings
    for field_name, field in model.model_fields.items():
        wire_name = field.alias or field_name
        expected_field = _resolve_local_schema(
            expected_properties.get(wire_name), expected, expected=True
        )
        actual_field = _resolve_local_schema(
            exported_properties.get(wire_name), document, expected=False
        )
        expected_constraints = _schema_constraints(expected_field, expected, expected=True)
        actual_constraints = _schema_constraints(actual_field, document, expected=False)
        for key, constraint_values in expected_constraints.items():
            if actual_constraints.get(key) != constraint_values:
                findings.append({"kind": "lost_constraint", "model": model.__name__,
                                 "field": field_name, "constraint": key,
                                 "operations": ", ".join(sorted(operations))})
    expected_discriminator = expected.get("discriminator")
    if expected_discriminator is not None and (not isinstance(component, dict) or component.get("discriminator") != expected_discriminator):
        findings.append({"kind": "lost_discriminator", "model": model.__name__,
                         "operations": ", ".join(sorted(operations))})
    if isinstance(component, dict):
        if model.model_config.get("extra") == "forbid" and component.get("additionalProperties") is not False:
            findings.append({"kind": "lost_extra_constraint", "model": model.__name__,
                             "operations": ", ".join(sorted(operations))})
        expected_required = set(expected.get("required", ()))
        actual_required = set(component.get("required", ()))
        for field_name in sorted(expected_required - actual_required):
            findings.append({"kind": "lost_required", "model": model.__name__,
                             "field": field_name,
                             "operations": ", ".join(sorted(operations))})
    return findings


def request_schema_audit() -> dict[str, object]:
    """Return unrepresented request rules and the documented exceptions."""
    document = openapi_document()
    models = request_models()
    components = document.get("components", {})
    schemas = components.get("schemas", {}) if isinstance(components, dict) else {}
    findings: list[dict[str, str]] = []
    findings.extend(_contradictory_union_findings(document))
    unbounded_integer_findings = _unbounded_integer_findings(document)
    findings.extend(unbounded_integer_findings)
    schema_extra_findings, schema_extra_counts = _json_schema_extra_findings()
    findings.extend(schema_extra_findings)
    exceptions: list[dict[str, str]] = []
    exceptions.extend(dict(item) for item in HANDLER_EXCEPTIONS)
    app = create_app(ApiContext.for_schema())
    routes = list(_router_routes(app.routes))
    walked_operations: set[str] = set()
    signed_request_model_operations: set[str] = set()
    for route in routes:
        endpoint_hints = get_type_hints(route.endpoint, include_extras=True)
        endpoint_models = set().union(
            *(_model_types(annotation, set()) for annotation in endpoint_hints.values())
        ) if endpoint_hints else set()

        def has_query_parser(
            annotation: object,
            metadata: Sequence[object] = (),
        ) -> bool:
            annotated_metadata = get_args(annotation)[1:] if get_origin(annotation) is Annotated else ()
            if any(
                getattr(getattr(item, "func", None), "__name__", None) == "_parse_safe_query_integer"
                for item in (*annotated_metadata, *metadata)
            ):
                return True
            return any(has_query_parser(argument) for argument in get_args(annotation))

        def check_query_parameters(dependant: object, visited: set[int]) -> None:
            if id(dependant) in visited:
                return
            visited.add(id(dependant))
            for parameter in getattr(dependant, "query_params", ()):
                annotation = endpoint_hints.get(parameter.name)
                validator_metadata: Sequence[object] = ()
                if annotation is None:
                    for model in endpoint_models:
                        matching_field = next((
                            field for name, field in model.model_fields.items()
                            if (field.alias or name) == parameter.alias
                        ), None)
                        if matching_field is not None:
                            annotation = matching_field.annotation
                            validator_metadata = matching_field.metadata
                            break
                if not _contains_numeric(annotation):
                    continue
                if not has_query_parser(annotation, validator_metadata):
                    findings.append({
                        "kind": "query_integer_without_safe_alias",
                        "model": route.path,
                        "field": parameter.alias,
                        "operations": ", ".join(
                            f"{method.upper()} {route.path}"
                            for method in sorted(route.methods or {"GET"})
                        ),
                    })
            for child in getattr(dependant, "dependencies", ()):
                check_query_parameters(child, visited)

        check_query_parameters(route.dependant, set())
        if route.include_in_schema:
            walked_operations.update(
                f"{method.upper()} {route.path}"
                for method in route.methods or ()
            )
        if route.path in {
            "/api/federation/v1/pair",
            "/api/federation/v1/status",
            "/api/federation/v1/message",
            "/api/federation/v1/pull",
        }:
            from studio_api.federation.router import SIGNED_REQUEST_OPERATION_MODELS

            operation = f"POST {route.path}"
            model = SIGNED_REQUEST_OPERATION_MODELS.get(route.path)
            expected_model_name = {
                "/api/federation/v1/pair": "PairRequest",
                "/api/federation/v1/status": "StatusRequest",
                "/api/federation/v1/message": "MessageRequest",
                "/api/federation/v1/pull": "PullRequest",
            }[route.path]
            if model is None or model.__name__ != expected_model_name:
                findings.append({
                    "kind": "request_model_not_enforced",
                    "operation": operation,
                    "expectedModel": expected_model_name,
                })
            else:
                signed_request_model_operations.add(operation)
        for parameter in route.dependant.body_params:
            annotations = (parameter.field_info.annotation, *get_args(parameter.field_info.annotation))
            discriminator = next(
                (item.discriminator for item in annotations
                 if isinstance(item, FieldInfo) and item.discriminator is not None),
                None,
            )
            if discriminator is None:
                continue
            method = next(iter(route.methods or ("POST",))).lower()
            paths = document.get("paths")
            path_item: object = paths.get(route.path, {}) if isinstance(paths, dict) else {}
            operation = path_item.get(method, {}) if isinstance(path_item, dict) else {}
            request_body = operation.get("requestBody", {}) if isinstance(operation, dict) else {}
            content = request_body.get("content", {}) if isinstance(request_body, dict) else {}
            json_content = content.get("application/json", {}) if isinstance(content, dict) else {}
            body_schema = json_content.get("schema", {}) if isinstance(json_content, dict) else {}
            if not isinstance(body_schema, dict) or "discriminator" not in body_schema:
                findings.append({"kind": "lost_discriminator", "model": route.path,
                                 "field": str(discriminator),
                                 "operations": f"{method.upper()} {route.path}"})
    for model, operations in sorted(models.items(), key=lambda item: item[0].__name__):
        model_name = f"{model.__module__}.{model.__qualname__}"
        config = model.model_config
        component = schemas.get(model.__name__) if isinstance(schemas, dict) else None
        findings.extend(_constraint_findings(model, component, operations, document))
        for finding in _validator_findings(model):
            finding["operations"] = ", ".join(sorted(operations))
            if "exception" in finding:
                exceptions.append(finding)
            else:
                findings.append(finding)
        if config.get("strict") is not True:
            findings.append({"kind": "lax_model", "model": model_name,
                             "operations": ", ".join(sorted(operations))})
        if config.get("extra") != "forbid":
            schema = component
            additional = schema.get("additionalProperties") if isinstance(schema, dict) else None
            modeled_extra = config.get("extra") == "allow" and (
                additional is True or (additional is not None and additional is not False)
            )
            if not modeled_extra:
                key = (model_name, "extra_policy")
                finding = {"kind": "extra_policy", "model": model_name,
                           "policy": str(config.get("extra")),
                           "operations": ", ".join(sorted(operations))}
                reason = SCHEMA_EXCEPTIONS.get(key)
                if reason is not None:
                    finding["exception"] = reason
                    exceptions.append(finding)
                else:
                    findings.append(finding)
        for field_name, field in model.model_fields.items():
            if _has_integer_literal(field.annotation):
                try:
                    TypeAdapter(Annotated[field.annotation, *field.metadata]).validate_python(True)
                except (TypeError, ValueError):
                    pass
                else:
                    findings.append({
                        "kind": "integer_literal_accepts_boolean",
                        "model": model_name,
                        "field": field_name,
                        "operations": ", ".join(sorted(operations)),
                    })
            if field.alias and field.alias != field_name:
                # A normal Pydantic wire alias is schema-expressible. Flag it
                # only if the generated contract failed to publish that name.
                properties = component.get("properties", {}) if isinstance(component, dict) else {}
                if not isinstance(properties, dict) or field.alias not in properties:
                    findings.append({"kind": "alias", "model": model_name,
                                     "field": field_name, "alias": field.alias,
                                     "operations": ", ".join(sorted(operations))})
            if any(getattr(metadata, "strict", None) is False for metadata in field.metadata):
                exported_field = None
                if isinstance(component, dict):
                    properties = component.get("properties", {})
                    if isinstance(properties, dict):
                        exported_field = _resolve_local_schema(
                            properties.get(field.alias or field_name), document,
                            expected=False,
                        )
                else:
                    for operation in operations:
                        method, _, path = operation.partition(" ")
                        paths = document.get("paths", {})
                        path_item = paths.get(path, {}) if isinstance(paths, dict) else {}
                        operation_item = path_item.get(method.lower(), {}) if isinstance(path_item, dict) else {}
                        parameters = operation_item.get("parameters", ()) if isinstance(operation_item, dict) else ()
                        for parameter in parameters if isinstance(parameters, list) else ():
                            if isinstance(parameter, dict) and parameter.get("name") == (field.alias or field_name):
                                exported_field = parameter.get("schema")
                                break
                query_parameters = all(
                    operation.partition(" ")[0].upper() in {"GET", "DELETE"}
                    for operation in operations
                )
                if query_parameters and _contains_numeric(field.annotation) and _schema_contains_type(
                    exported_field, "integer", "number"
                ):
                    # Query values arrive as text on HTTP. FastAPI/Pydantic must
                    # parse that scalar while OpenAPI exposes its numeric type.
                    continue
                key = (model_name, field_name)
                reason = SCHEMA_EXCEPTIONS.get(key)
                finding = {"kind": "lax_field", "model": model_name,
                           "field": field_name,
                           "operations": ", ".join(sorted(operations))}
                if reason is not None:
                    finding["exception"] = reason
                    exceptions.append(finding)
                else:
                    findings.append(finding)
        if config.get("populate_by_name") and any(
            field.alias and field.alias != name for name, field in model.model_fields.items()
        ):
            findings.append({"kind": "populate_by_name", "model": model_name,
                             "operations": ", ".join(sorted(operations))})

    # Keep the document in this audit's execution path so an exporter failure
    # cannot accidentally turn a missing schema into an empty work list.
    if not document.get("paths"):
        findings.append({"kind": "missing_openapi_paths"})
    documented_operations: set[str] = set()
    paths_value = document.get("paths")
    if isinstance(paths_value, dict):
        for path, path_item_value in paths_value.items():
            if not isinstance(path, str) or not isinstance(path_item_value, dict):
                continue
            for method in path_item_value:
                if isinstance(method, str) and method.lower() in {
                    "get", "put", "post", "delete", "options", "head", "patch", "trace",
                }:
                    documented_operations.add(f"{method.upper()} {path}")
    if walked_operations != documented_operations:
        findings.append({
            "kind": "operation_coverage_mismatch",
            "walked": str(len(walked_operations)),
            "documented": str(len(documented_operations)),
            "missingFromWalker": ", ".join(sorted(documented_operations - walked_operations)),
            "missingFromDocument": ", ".join(sorted(walked_operations - documented_operations)),
        })
    return {
        "findings": findings,
        "exceptions": exceptions,
        "jsonSchemaExtra": schema_extra_counts,
        "operationCoverage": {
            "walked": len(walked_operations),
            "documented": len(documented_operations),
        },
        "signedRequestModelCoverage": {
            "enforcedOperations": len(signed_request_model_operations),
            "expectedOperations": 4,
        },
        "unboundedIntegerSchemas": len(unbounded_integer_findings),
    }


def main() -> int:
    report = request_schema_audit()
    print(json.dumps(report, ensure_ascii=False, indent=2, sort_keys=True))
    return int(bool(report["findings"]))


if __name__ == "__main__":
    raise SystemExit(main())
