"""Compare the OpenAPI constraints used by the schema contract tests.

This is a deliberately small checker, not a JSON Schema subsumption engine. It
handles local references and common structural constraints; remote references,
dynamic references, and full interactions among arbitrary JSON Schema keywords
remain outside its scope. Unclassified keyword changes are reported for review.
"""
from __future__ import annotations

import json
from collections.abc import Mapping

from studio_api.models import JsonValue


CONSTRAINT_KEYS = (
    "minimum", "maximum", "exclusiveMinimum", "exclusiveMaximum",
    "minLength", "maxLength", "pattern", "enum", "const", "format",
    "minItems", "maxItems", "uniqueItems", "required", "additionalProperties",
    "minProperties", "maxProperties", "multipleOf", "type", "minContains",
    "maxContains", "dependentRequired", "nullable",
)
METHODS = {"get", "put", "post", "delete", "options", "head", "patch", "trace"}


def pointer(base: str, key: str | int) -> str:
    return f"{base}/{str(key).replace('~', '~0').replace('/', '~1')}"


def resolve_ref(document: JsonValue, value: JsonValue) -> JsonValue:
    """Inline a local ref at one node; do not normalize or drop its keywords."""
    if not isinstance(value, dict):
        return value
    ref = value.get("$ref")
    if not isinstance(ref, str) or not ref.startswith("#/"):
        return value
    target: JsonValue = document
    for part in ref[2:].split("/"):
        if not isinstance(target, dict):
            return value
        target = target.get(part.replace("~1", "/").replace("~0", "~"))
    if not isinstance(target, dict):
        return value
    return {**target, **{key: child for key, child in value.items() if key != "$ref"}}


def _resolve_schema_content(
    document: JsonValue, value: JsonValue, ref_stack: frozenset[str] = frozenset()
) -> JsonValue:
    """Expand local schema refs recursively while leaving cycles as refs."""
    if isinstance(value, list):
        return [_resolve_schema_content(document, child, ref_stack) for child in value]
    if not isinstance(value, dict):
        return value
    ref = value.get("$ref")
    if isinstance(ref, str) and ref.startswith("#/") and ref not in ref_stack:
        expanded = resolve_ref(document, value)
        if expanded is not value:
            return _resolve_schema_content(document, expanded, ref_stack | {ref})
    return {
        key: _resolve_schema_content(document, child, ref_stack)
        for key, child in value.items()
    }


def _reachable_schema_names(document: JsonValue) -> set[str]:
    """Return schema components reachable from an operation's wire contract."""
    reachable: set[str] = set()
    visited_refs: set[str] = set()

    def visit(value: JsonValue) -> None:
        if isinstance(value, dict):
            ref = value.get("$ref")
            if isinstance(ref, str) and ref.startswith("#/components/"):
                parts = ref[2:].split("/")
                if len(parts) >= 3 and parts[1] == "schemas":
                    name = parts[2].replace("~1", "/").replace("~0", "~")
                    reachable.add(name)
                if ref not in visited_refs:
                    visited_refs.add(ref)
                    target: JsonValue = document
                    for part in ref[2:].split("/"):
                        if not isinstance(target, dict):
                            break
                        target = target.get(part.replace("~1", "/").replace("~0", "~"))
                    else:
                        visit(target)
            for key, child in value.items():
                if key != "$ref":
                    visit(child)
        elif isinstance(value, list):
            for child in value:
                visit(child)

    if isinstance(document, dict):
        visit(document.get("paths"))
    return reachable


def _canonical(value: JsonValue) -> str:
    return json.dumps(
        _without_defaults(value), sort_keys=True, separators=(",", ":"), ensure_ascii=False
    )


def _without_defaults(value: JsonValue) -> JsonValue:
    """Remove documentation-only default values from nested union schemas."""
    if isinstance(value, list):
        return [_without_defaults(child) for child in value]
    if isinstance(value, dict):
        return {
            key: _without_defaults(child)
            for key, child in value.items()
            if key != "default"
        }
    return value


def constraint_snapshot(document: JsonValue) -> dict[str, dict[str, JsonValue]]:
    """Index schema constraints by pointer, resolving refs and preserving unions.

    Union alternatives are stored as complete canonical branch schemas at the
    parent pointer. This avoids treating branch reordering or removal as a
    shift in array indexes, while retaining every keyword on each branch.
    """
    snapshot: dict[str, dict[str, JsonValue]] = {}

    def visit_schema(
        value: JsonValue,
        path: str,
        ref_stack: frozenset[str] = frozenset(),
        *,
        expand_refs: bool = True,
    ) -> None:
        if not isinstance(value, dict):
            return
        ref = value.get("$ref")
        if isinstance(ref, str) and ref in ref_stack:
            return
        schema = resolve_ref(document, value) if expand_refs else {
            key: child for key, child in value.items() if key != "$ref"
        }
        if not isinstance(schema, dict):
            return
        # Retain schema-node presence even when the node is unconstrained ({})
        # so it is not confused with a property absent from the parent object.
        constraints: dict[str, JsonValue] = {"@schemaPresent": True}
        for key in CONSTRAINT_KEYS:
            if key in schema and not (key == "additionalProperties" and isinstance(schema[key], dict)):
                constraints[key] = schema[key]
        nullable_union = schema.get("anyOf")
        if isinstance(nullable_union, list):
            null_branches = [branch for branch in nullable_union
                             if isinstance(branch, dict) and branch.get("type") == "null"]
            concrete = [branch for branch in nullable_union if branch not in null_branches]
            concrete_types = {
                branch.get("type") for branch in concrete if isinstance(branch, dict)
            }
            if (
                null_branches
                and len(concrete) > 1
                and concrete_types == {"integer", "number"}
            ):
                # Integer is already included by JSON Schema's number type.
                # Compare the numeric branches as a nullable numeric union.
                constraints["@nullable"] = True
                constraints["@nullableBranch"] = [
                    _canonical(branch) for branch in concrete
                ]
                nullable_union = None
            if null_branches and len(concrete) == 1 and isinstance(concrete[0], dict):
                concrete_schema = resolve_ref(document, concrete[0])
                if isinstance(concrete_schema, dict):
                    for key in CONSTRAINT_KEYS:
                        if key in concrete_schema and key not in constraints:
                            constraints[key] = concrete_schema[key]
                    # Preserve the whole concrete branch, excluding keywords
                    # already represented at this node. This retains nested
                    # validation constraints without mistaking ref siblings
                    # duplicated at the root for semantic changes.
                    branch_detail = dict(concrete[0])
                    for key in CONSTRAINT_KEYS:
                        if key in branch_detail and branch_detail[key] == constraints.get(key):
                            branch_detail.pop(key)
                    nullable_detail: list[JsonValue] = [_canonical(branch_detail)]
                    constraints["@nullableBranch"] = nullable_detail
                    constraints["@nullable"] = True
                nullable_union = None
        if schema.get("nullable") is True:
            constraints["@nullable"] = True
        for keyword in ("anyOf", "oneOf", "allOf"):
            if keyword == "anyOf" and nullable_union is None:
                continue
            branches = schema.get(keyword)
            if isinstance(branches, list):
                union_branches: list[JsonValue] = []
                for branch in branches:
                    resolved_branch = (
                        _resolve_schema_content(document, branch) if expand_refs else branch
                    )
                    union_branches.append(_canonical(resolved_branch))
                union_branches.sort(key=str)
                constraints[f"@{keyword}"] = union_branches
        if constraints:
            snapshot[path] = constraints

        if isinstance(ref, str) and ref.startswith("#/"):
            ref_stack = ref_stack | {ref}
        for keyword in ("properties", "patternProperties", "$defs", "dependentSchemas"):
            children = schema.get(keyword)
            if isinstance(children, dict):
                for name, child in children.items():
                    visit_schema(child, pointer(pointer(path, keyword), name), ref_stack,
                                 expand_refs=expand_refs)
        for keyword in ("items", "contains", "propertyNames", "not", "if", "then", "else"):
            child = schema.get(keyword)
            if isinstance(child, dict):
                visit_schema(child, pointer(path, keyword), ref_stack, expand_refs=expand_refs)
        additional = schema.get("additionalProperties")
        if isinstance(additional, dict):
            visit_schema(additional, pointer(path, "additionalProperties"), ref_stack,
                         expand_refs=expand_refs)

    def media(
        value: JsonValue,
        path: str,
        *,
        expand_refs: bool = True,
        response_schema: bool = False,
    ) -> None:
        entry = resolve_ref(document, value)
        if not isinstance(entry, dict):
            return
        content = entry.get("content")
        if isinstance(content, dict):
            for media_type, media_value in content.items():
                if isinstance(media_value, dict):
                    schema = media_value.get("schema")
                    schema_path = pointer(pointer(path, "content"), media_type) + "/schema"
                    if (
                        response_schema
                        and isinstance(schema, dict)
                        and isinstance(schema.get("$ref"), str)
                    ):
                        snapshot[schema_path] = {
                            "@responseRef": schema["$ref"],
                            "@responseSchema": _canonical(
                                _resolve_schema_content(document, schema)
                            ),
                        }
                        continue
                    visit_schema(
                        schema,
                        schema_path,
                        expand_refs=expand_refs and not response_schema,
                    )

    if not isinstance(document, dict):
        return snapshot
    components = document.get("components")
    if isinstance(components, dict):
        schemas = components.get("schemas")
        if isinstance(schemas, dict):
            reachable_schemas = _reachable_schema_names(document)
            for name, schema in schemas.items():
                if name not in reachable_schemas:
                    continue
                visit_schema(schema, f"/components/schemas/{name}")
        parameters = components.get("parameters")
        if isinstance(parameters, dict):
            for name, value in parameters.items():
                parameter = resolve_ref(document, value)
                if isinstance(parameter, dict):
                    visit_schema(parameter.get("schema"), f"/components/parameters/{name}/schema")
        # Response DTOs are registered under components.schemas. The per-status
        # response wrappers repeat those same inline schemas for every route;
        # comparing them would count one shared ErrorResponse field hundreds of
        # times. Request bodies are indexed at their operation sites below.

    paths = document.get("paths")
    if isinstance(paths, dict):
        for path, value in paths.items():
            path_item = resolve_ref(document, value)
            if not isinstance(path_item, dict):
                continue
            for method, operation_value in path_item.items():
                if method not in METHODS:
                    continue
                operation = resolve_ref(document, operation_value)
                if not isinstance(operation, dict):
                    continue
                base = pointer(pointer("/paths", path), method)
                for scope, entries in (("pathParameters", path_item.get("parameters")),
                                       ("operationParameters", operation.get("parameters"))):
                    if isinstance(entries, list):
                        for value in entries:
                            parameter = resolve_ref(document, value)
                            if isinstance(parameter, dict):
                                location = parameter.get("in")
                                parameter_name = parameter.get("name")
                                if isinstance(location, str) and isinstance(parameter_name, str):
                                    parameter_path = pointer(
                                        pointer(pointer(base, scope), location), parameter_name
                                    )
                                    snapshot[parameter_path] = {
                                        "@parameterPresent": True,
                                        "required": parameter.get("required") is True,
                                    }
                                    visit_schema(
                                        parameter.get("schema"),
                                        pointer(parameter_path, "schema"),
                                    )
                request_body = operation.get("requestBody")
                if isinstance(request_body, dict):
                    resolved_request_body = resolve_ref(document, request_body)
                    if isinstance(resolved_request_body, dict):
                        body_path = pointer(base, "requestBody")
                        snapshot[body_path] = {
                            "required": resolved_request_body.get("required") is True
                        }
                        media(resolved_request_body, body_path)
                responses = operation.get("responses")
                if isinstance(responses, dict):
                    for status, response in responses.items():
                        media(response, pointer(pointer(base, "responses"), status),
                              response_schema=True)
    return snapshot


def constraint_changes(
    baseline: Mapping[str, Mapping[str, JsonValue]],
    candidate: Mapping[str, Mapping[str, JsonValue]],
) -> tuple[list[dict[str, str]], list[dict[str, str]]]:
    """Return all loosenings and tightenings between snapshots."""
    loosenings: list[dict[str, str]] = []
    tightenings: list[dict[str, str]] = []

    def record(target: list[dict[str, str]], path: str, key: str, old: object, new: object) -> None:
        target.append({"pointer": path, "constraint": key, "baseline": repr(old), "candidate": repr(new)})

    def branch_direction(old_value: JsonValue, new_value: JsonValue) -> str | None:
        if old_value == new_value:
            return None
        if not isinstance(old_value, dict) or not isinstance(new_value, dict):
            return "loosening"
        if not old_value:
            # An unconstrained schema accepts every JSON value. Replacing it
            # with a recursive JsonValue schema can only narrow that set.
            return "tightening" if new_value else None
        if not new_value:
            return "loosening"
        old_type, new_type = old_value.get("type"), new_value.get("type")
        old_union = old_value.get("anyOf")
        if old_type is None and new_type == "number" and isinstance(old_union, list):
            old_branches = [branch for branch in old_union if isinstance(branch, dict)]
            if {branch.get("type") for branch in old_branches} == {"integer", "number"}:
                new_branch: dict[str, JsonValue] = {
                    key: new_value[key]
                    for key in ("type", "minimum", "maximum", "exclusiveMinimum", "exclusiveMaximum")
                    if key in new_value
                }
                return union_direction(
                    [_canonical(branch) for branch in old_branches],
                    [_canonical(new_branch)],
                )
        if old_type != new_type:
            if old_type == "integer" and new_type == "number":
                return "loosening"
            if old_type == "number" and new_type == "integer":
                return "tightening"
            return "loosening"
        directions: list[str] = []
        for key in set(old_value) | set(new_value):
            before, after = old_value.get(key), new_value.get(key)
            if before == after:
                continue
            if key.startswith("x-"):
                # OpenAPI extensions describe tooling metadata, not validation.
                continue
            if key in {"title", "description", "default", "examples", "deprecated", "readOnly", "writeOnly"}:
                continue
            if key in {"minimum", "exclusiveMinimum", "minLength", "minItems", "minProperties", "minContains"}:
                if after is None:
                    directions.append("loosening")
                elif before is None:
                    directions.append("tightening")
                elif isinstance(before, (int, float)) and isinstance(after, (int, float)) and after > before:
                    directions.append("tightening")
                else:
                    directions.append("loosening")
            elif key in {"maximum", "exclusiveMaximum", "maxLength", "maxItems", "maxProperties"}:
                if after is None:
                    directions.append("loosening")
                elif before is None:
                    directions.append("tightening")
                elif isinstance(before, (int, float)) and isinstance(after, (int, float)) and after < before:
                    directions.append("tightening")
                else:
                    directions.append("loosening")
            elif key == "pattern":
                if before is None and after is not None:
                    directions.append("tightening")
                elif after is None:
                    directions.append("loosening")
                else:
                    directions.append("loosening")
            elif key == "enum" and isinstance(before, list) and isinstance(after, list):
                directions.append("tightening" if set(after) < set(before) else "loosening")
            elif key == "required" and isinstance(before, list) and isinstance(after, list):
                directions.append("tightening" if set(before) < set(after) else "loosening")
            elif key == "additionalProperties":
                if before is False and after is not False:
                    directions.append("loosening")
                elif after is False and before is not False:
                    directions.append("tightening")
                elif isinstance(before, dict) and isinstance(after, dict):
                    nested = branch_direction(before, after)
                    if nested:
                        directions.append(nested)
            elif key == "uniqueItems":
                directions.append("tightening" if after is True else "loosening")
            elif key in {"properties", "patternProperties"} and isinstance(before, dict) and isinstance(after, dict):
                for name in set(before) & set(after):
                    nested = branch_direction(before[name], after[name])
                    if nested:
                        directions.append(nested)
            elif key in {"items", "contains", "propertyNames", "not", "if", "then", "else"}:
                if isinstance(before, dict) and isinstance(after, dict):
                    nested = branch_direction(before, after)
                    if nested:
                        directions.append(nested)
            elif key in {"anyOf", "oneOf", "allOf"}:
                if before is None:
                    directions.append("tightening")
                elif after is None:
                    directions.append("loosening")
                elif isinstance(before, list) and isinstance(after, list):
                    nested = union_direction(
                        [_canonical(branch) for branch in before],
                        [_canonical(branch) for branch in after],
                    )
                    if nested:
                        directions.append(nested)
            else:
                directions.append("loosening")
        if "loosening" in directions:
            return "loosening"
        return "tightening" if directions else None

    def union_direction(old: list[str], new: list[str]) -> str | None:
        old_branches = [json.loads(branch) for branch in old]
        new_branches = [json.loads(branch) for branch in new]
        unmatched_new = list(new_branches)
        directions: list[str] = []
        for old_branch in old_branches:
            match_index = next(
                (index for index, new_branch in enumerate(unmatched_new)
                 if isinstance(old_branch, dict) and isinstance(new_branch, dict)
                 and old_branch.get("type") == new_branch.get("type")),
                None,
            )
            if match_index is not None:
                new_branch = unmatched_new.pop(match_index)
                direction = branch_direction(old_branch, new_branch)
                if direction:
                    directions.append(direction)
                continue
            old_type = old_branch.get("type") if isinstance(old_branch, dict) else None
            if old_type == "integer" and any(
                isinstance(branch, dict) and branch.get("type") == "number"
                for branch in unmatched_new
            ):
                # JSON Schema number already includes every integer, so this
                # removed branch changes representation but not the union.
                continue
            else:
                directions.append("tightening")
        if unmatched_new:
            directions.append("loosening")
        if "loosening" in directions:
            return "loosening"
        return "tightening" if directions else None

    def classify(key: str, old: JsonValue | None, new: JsonValue | None) -> str | None:
        if old == new:
            return None
        if key == "@nullable":
            return "tightening" if old is True and new is not True else "loosening"
        if key == "@nullableBranch":
            if old is None:
                return "loosening"
            if new is None:
                return "tightening"
        if key == "@responseSchema" and isinstance(old, str) and isinstance(new, str):
            return branch_direction(json.loads(old), json.loads(new))
        if key == "required" and isinstance(old, bool) and isinstance(new, bool):
            return "loosening" if old and not new else "tightening" if new and not old else None
        if key == "@parameterPresent":
            return "loosening" if old is True and new is not True else "tightening"
        if old is None:
            return "tightening"
        if new is None:
            return "loosening"
        if key in {"minimum", "exclusiveMinimum", "minLength", "minItems", "minProperties", "minContains"}:
            if isinstance(old, (int, float)) and isinstance(new, (int, float)):
                return "loosening" if new < old else "tightening"
        if key in {"maximum", "exclusiveMaximum", "maxLength", "maxItems", "maxProperties", "maxContains"}:
            if isinstance(old, (int, float)) and isinstance(new, (int, float)):
                return "loosening" if new > old else "tightening"
        if key == "enum" and isinstance(old, list) and isinstance(new, list):
            return "loosening" if set(old) < set(new) else "tightening" if set(new) < set(old) else None
        if key == "required" and isinstance(old, list) and isinstance(new, list):
            return "loosening" if not set(old) <= set(new) else "tightening" if not set(new) <= set(old) else None
        if key == "additionalProperties":
            if isinstance(old, dict) and isinstance(new, dict):
                return None
            if old is False and new is not False:
                return "loosening"
            if new is False and old is not False:
                return "tightening"
        if key == "uniqueItems":
            if old is True and new is not True:
                return "loosening"
            if new is True and old is not True:
                return "tightening"
        if key.startswith("@"):
            if isinstance(old, list) and isinstance(new, list):
                if all(isinstance(branch, str) for branch in old + new):
                    return union_direction(
                        [branch for branch in old if isinstance(branch, str)],
                        [branch for branch in new if isinstance(branch, str)],
                    )
            return "loosening"
        if key == "type":
            old_types = set(old) if isinstance(old, list) else {old}
            new_types = set(new) if isinstance(new, list) else {new}
            if old_types == {"string"} and new_types <= {"integer", "number"}:
                return "tightening"
            if old_types == {"array"} and new_types == {"string"}:
                return "tightening"
            return "loosening" if old_types < new_types else "tightening" if new_types < old_types else "loosening"
        # A changed opaque validation keyword is a potential loosening and must
        # be reviewed explicitly rather than disappearing from the report.
        return "loosening"

    def closed_object_parent(path: str, snapshot: Mapping[str, Mapping[str, JsonValue]]) -> str | None:
        parent, separator, name = path.rpartition("/properties/")
        if not separator or not name or "/" in name:
            return None
        return parent if snapshot.get(parent, {}).get("additionalProperties") is False else None

    def excluded_by_parent_type_change(path: str) -> bool:
        tokens = path.split("/")
        if "items" in tokens:
            parent = "/".join(tokens[:tokens.index("items")])
            old_types = baseline.get(parent, {}).get("type")
            new_types = candidate.get(parent, {}).get("type")
            old_set = set(old_types) if isinstance(old_types, list) else {old_types}
            new_set = set(new_types) if isinstance(new_types, list) else {new_types}
            if "array" in old_set and "array" not in new_set:
                return True
        return False

    for path in sorted(set(baseline) | set(candidate)):
        old_schema = baseline.get(path, {})
        new_schema = candidate.get(path, {})
        if path.endswith("/schema") and (
            "/operationParameters/" in path or "/pathParameters/" in path
        ):
            parameter_path = path[:-len("/schema")]
            if baseline.get(parameter_path, {}).get("@parameterPresent") != candidate.get(
                parameter_path, {}
            ).get("@parameterPresent"):
                continue
        old_parameter = old_schema.get("@parameterPresent") is True
        new_parameter = new_schema.get("@parameterPresent") is True
        if old_parameter != new_parameter:
            required = old_schema.get("required") is True if old_parameter else new_schema.get("required") is True
            parameter_direction = (
                "loosening" if required else "tightening"
            ) if old_parameter else (
                "tightening" if required else "loosening"
            )
            record(
                loosenings if parameter_direction == "loosening" else tightenings,
                path,
                "parameter",
                "present" if old_parameter else "absent",
                "present" if new_parameter else "absent",
            )
        removed_closed_property = False
        if not old_schema and new_schema:
            # Adding an optional property to a closed object expands its
            # accepted wire shape. Keep that as a loosening in the report.
            if closed_object_parent(path, baseline) is not None:
                loosenings.append({
                    "pointer": path,
                    "constraint": "property",
                    "baseline": "absent from closed object",
                    "candidate": "accepted property",
                })
        elif old_schema and not new_schema:
            # Removing a property from a closed object removes a formerly
            # accepted wire shape; a missing child schema is not a relaxation.
            if closed_object_parent(path, candidate) is not None:
                removed_closed_property = True
                tightenings.append({
                    "pointer": path,
                    "constraint": "property",
                    "baseline": "accepted property",
                    "candidate": "absent from closed object",
                })
            elif excluded_by_parent_type_change(path):
                removed_closed_property = True
                tightenings.append({
                    "pointer": path,
                    "constraint": "schema branch",
                    "baseline": "accepted under parent type",
                    "candidate": "excluded by parent type",
                })
        if removed_closed_property:
            continue
        for key in sorted(
            (set(old_schema) | set(new_schema))
            - {"@schemaPresent", "@parameterPresent", "@responseRef"}
        ):
            old = old_schema.get(key)
            new = new_schema.get(key)
            if key == "@responseSchema" and old_schema.get("@responseRef") == new_schema.get("@responseRef"):
                continue
            if key == "required" and (old_parameter != new_parameter):
                continue
            if key == "enum" and isinstance(old, list) and isinstance(new, list):
                added = set(new) - set(old)
                removed = set(old) - set(new)
                if added:
                    record(loosenings, path, key, sorted(old, key=repr), sorted(new, key=repr))
                if removed:
                    record(tightenings, path, key, sorted(old, key=repr), sorted(new, key=repr))
                continue
            constraint_direction = None
            if (
                key in {"@anyOf", "@nullableBranch"}
                and isinstance(old, list)
                and all(isinstance(branch, str) for branch in old)
            ):
                old_branches = [json.loads(branch) for branch in old if isinstance(branch, str)]
                old_types = {
                    branch.get("type") for branch in old_branches
                    if isinstance(branch, dict)
                }
                candidate_is_numeric = new_schema.get("type") == "number"
                branch_count_matches = (
                    new is None if key == "@anyOf"
                    else new == ["{}"]
                )
                if (
                    old_types == {"integer", "number"}
                    and candidate_is_numeric
                    and branch_count_matches
                ):
                    new_branch: dict[str, JsonValue] = {
                        key: new_schema[key]
                        for key in ("type", "minimum", "maximum", "exclusiveMinimum", "exclusiveMaximum")
                        if key in new_schema
                    }
                    constraint_direction = union_direction(
                        [_canonical(branch) for branch in old_branches],
                        [_canonical(new_branch)],
                    )
            if constraint_direction is None:
                constraint_direction = classify(key, old, new)
            if constraint_direction == "loosening":
                record(loosenings, path, key, old, new)
            elif constraint_direction == "tightening":
                record(tightenings, path, key, old, new)
    return loosenings, tightenings


def constraint_loosening_findings(
    baseline: Mapping[str, Mapping[str, JsonValue]], candidate: Mapping[str, Mapping[str, JsonValue]]
) -> list[dict[str, str]]:
    """Compatibility wrapper returning baseline-to-candidate loosenings."""
    return constraint_changes(baseline, candidate)[0]
