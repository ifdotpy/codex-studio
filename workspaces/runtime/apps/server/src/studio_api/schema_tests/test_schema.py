"""OpenAPI schema coverage checks for generation."""
from __future__ import annotations

import unittest
import json
from pathlib import Path
from tempfile import TemporaryDirectory
from typing import cast

from codex_layout import REPOSITORY_ROOT
from pydantic import TypeAdapter, ValidationError
from codex_sync_entities import _DTO_MODELS
from studio_api.models import (
    MAX_SAFE_INTEGER,
    MIN_SAFE_INTEGER,
    JsonValue,
    SafeInt,
    SafeIntHeader,
    SafeQueryInt,
    SignedSafeInt,
)
from studio_api.io.models import TerminalRecord
from studio_api.work.models import RuleRecord, WorkItem
from studio_api.generate_types import render
from studio_api.schema import (
    entity_schema_names,
    bound_integer_schemas,
    normalize_json_value_schema,
    remove_orphan_fastapi_validation_schemas,
    JSON_VALUE_SCHEMA,
    normalize_optional_parameter_schemas,
    validate_error_responses,
    validate_contract_schemas,
    openapi_document,
)
from studio_api.request_schema_audit import (
    _contradictory_union_findings,
    _json_schema_extra_findings,
    request_schema_audit,
)
from studio_api.schema_tests.constraint_diff import (
    constraint_changes,
    constraint_loosening_findings,
    constraint_snapshot,
)


def _responses(document: dict[str, JsonValue]) -> dict[str, JsonValue]:
    paths = document["paths"]
    assert isinstance(paths, dict)
    path_item = paths["/api/example"]
    assert isinstance(path_item, dict)
    operation = path_item["get"]
    assert isinstance(operation, dict)
    return cast(dict[str, JsonValue], operation["responses"])


def sample_document(response_schema: dict[str, JsonValue]) -> dict[str, JsonValue]:
    document: dict[str, JsonValue] = {
        "openapi": "3.1.0",
        "paths": {
            "/api/example": {
                "get": {
                    "responses": {
                        "200": {
                            "content": {"application/json": {"schema": response_schema}},
                        },
                        "400": {
                            "content": {
                                "application/json": {
                                    "schema": {"$ref": "#/components/schemas/ErrorResponse"}
                                }
                            }
                        },
                    }
                }
            }
        },
        "components": {
            "schemas": {
                "JsonValue": {},
                "ErrorResponse": {
                    "type": "object",
                    "properties": {"error": {"type": "string"}},
                    "required": ["error"],
                    "additionalProperties": False,
                },
                "AgentEntityDto": {
                    "type": "object",
                    "properties": {
                        "id": {"type": "string"},
                        "status": {"$ref": "#/components/schemas/Status"},
                    },
                },
                "Status": {"type": "string", "enum": ["idle", "running"]},
                "TaskEntityDto": {"type": "object", "properties": {"id": {"type": "string"}}},
            }
        },
    }
    normalize_json_value_schema(document)
    bound_integer_schemas(document)
    return document


class SchemaContractTests(unittest.TestCase):
    def test_integer_alias_runtime_and_exported_bounds_agree(self) -> None:
        cases = (
            (SafeInt, 0, MAX_SAFE_INTEGER, (-1, MAX_SAFE_INTEGER + 1)),
            (SignedSafeInt, MIN_SAFE_INTEGER, MAX_SAFE_INTEGER,
             (MIN_SAFE_INTEGER - 1, MAX_SAFE_INTEGER + 1)),
            (SafeQueryInt, 0, MAX_SAFE_INTEGER, ("+1", " 1", "1.0", "")),
            (SafeIntHeader, 0, MAX_SAFE_INTEGER, ("+1", " 1", "1.0", "")),
        )
        for alias, minimum, maximum, invalid_values in cases:
            with self.subTest(alias=alias):
                adapter = TypeAdapter(alias)
                document: dict[str, JsonValue] = {
                    "components": {"schemas": {"value": adapter.json_schema()}}
                }
                bound_integer_schemas(document)
                components = document["components"]
                assert isinstance(components, dict)
                schemas = components["schemas"]
                assert isinstance(schemas, dict)
                schema = schemas["value"]
                assert isinstance(schema, dict)
                self.assertEqual(schema["minimum"], minimum)
                self.assertEqual(schema["maximum"], maximum)
                for accepted_value in (minimum, maximum):
                    accepted = (
                        str(accepted_value)
                        if alias in (SafeQueryInt, SafeIntHeader)
                        else accepted_value
                    )
                    adapter.validate_python(accepted)
                for invalid_value in invalid_values:
                    with self.assertRaises(ValidationError):
                        adapter.validate_python(invalid_value)

    def test_persisted_rule_and_negative_exit_code_fit_bounded_response_models(self) -> None:
        rule = RuleRecord.model_validate(
            {"id": "rule-1", "kind": "once", "stallWakeGeneration": -1}
        )
        self.assertEqual(rule.stallWakeGeneration, -1)
        terminal = TerminalRecord.model_validate(
            {"id": "terminal-1", "agent": "agent-1", "title": "shell",
             "cwd": "/tmp", "status": "exited", "created": 1, "exitCode": -9}
        )
        self.assertEqual(terminal.exitCode, -9)

        document = json.loads(
            (REPOSITORY_ROOT / "workspaces/runtime/packages/runtime-http/openapi.json")
            .read_text(encoding="utf-8")
        )
        components = document["components"]["schemas"]
        rule_integer = components["RuleRecordResponse"]["properties"]["stallWakeGeneration"]
        self.assertLessEqual(rule_integer["minimum"], rule.stallWakeGeneration)
        self.assertGreaterEqual(rule_integer["maximum"], rule.stallWakeGeneration)
        exit_schema = components["TerminalRecord"]["properties"]["exitCode"]
        exit_integer = next(branch for branch in exit_schema["anyOf"] if branch.get("type") == "integer")
        self.assertLessEqual(exit_integer["minimum"], terminal.exitCode)
        self.assertGreaterEqual(exit_integer["maximum"], terminal.exitCode)

    def test_work_archive_producer_integer_fields_use_safe_runtime_aliases(self) -> None:
        adapter = TypeAdapter(WorkItem)
        item = {
            "id": "work-1", "rootId": "root-1", "title": "Archive",
            "status": "ready", "created": 1, "updated": 2, "version": 1,
            "archiveIntent": {
                "id": "archive-1", "status": "pending", "attempts": MAX_SAFE_INTEGER,
                "schedulerVersion": MAX_SAFE_INTEGER,
            },
        }
        result = adapter.validate_python(item)
        assert result.archiveIntent is not None
        self.assertEqual(result.archiveIntent["attempts"], MAX_SAFE_INTEGER)
        for field in ("attempts", "schedulerVersion"):
            with self.subTest(field=field):
                invalid = json.loads(json.dumps(item))
                invalid["archiveIntent"][field] = MAX_SAFE_INTEGER + 1
                with self.assertRaises(ValidationError):
                    adapter.validate_python(invalid)

    def test_generated_document_does_not_loosen_main_constraint_baseline(self) -> None:
        baseline_path = Path(__file__).with_name("openapi_constraint_baseline.json")
        manifest = json.loads(baseline_path.read_text(encoding="utf-8"))
        self.assertEqual(manifest["sourceCommit"], "2602cb4e")
        self.assertEqual(manifest["sourcePath"], "workspaces/runtime/packages/runtime-http/openapi.json")
        baseline_document = manifest["document"]
        self.assertIsInstance(baseline_document, dict)
        baseline = constraint_snapshot(baseline_document)
        committed_document = json.loads(
            (REPOSITORY_ROOT / manifest["sourcePath"]).read_text(encoding="utf-8")
        )
        candidate = constraint_snapshot(committed_document)
        loosenings, _tightenings = constraint_changes(baseline, candidate)
        accepted_loosenings = {
            ("/components/schemas/DraftDocumentInput/properties/deleted", "property"):
                "Adds the deleted state to the draft input DTO.",
            ("/components/schemas/DraftPushRequest/properties/rows/items/properties/newDocumentState/properties/deleted", "property"):
                "Adds the deleted state to the nested draft push DTO.",
            ("/components/schemas/DraftPushRow/properties/newDocumentState/properties/deleted", "property"):
                "Adds the deleted state to the draft push row DTO.",
            ("/components/schemas/ErrorResponse/properties/location", "property"):
                "Adds the optional error location field.",
            ("/components/schemas/ErrorResponse/properties/location", "@nullable"):
                "Allows a null error location.",
            ("/components/schemas/ErrorResponse/properties/location", "@nullableBranch"):
                "Adds the nullable branch for the error location field.",
            ("/paths/~1api~1sync~1drafts/post/requestBody/content/application~1json/schema/properties/rows/items/properties/newDocumentState/properties/deleted", "property"):
                "Publishes the deleted state in the sync drafts request schema.",
            ("/paths/~1api~1analytics/get/operationParameters/query/from_time", "parameter"):
                "Documents the client's from_time filter, which the insights model maps to its legacy 'from' option.",
            ("/paths/~1api~1analytics/get/operationParameters/query/to_time", "parameter"):
                "Documents the client's to_time filter, which the insights model maps to its legacy 'to' option.",
            ("/paths/~1api~1monitor~1log/get/operationParameters/header/Range", "parameter"):
                "Documents the standard optional HTTP Range header already interpreted by the handler.",
            ("/paths/~1api~1sync~1stream/get/operationParameters/header/Last-Event-ID", "parameter"):
                "Documents the optional event cursor header now parsed by the shared safe-integer type.",
            ("/paths/~1api~1sync~1stream/get/operationParameters/header/X-Codex-Sync-Protocol", "parameter"):
                "Documents the optional protocol negotiation header already read by the stream handler.",
            ("/paths/~1api~1sync~1stream/get/operationParameters/query/apiSchema", "parameter"):
                "Documents the optional API schema query used by the existing client compatibility handshake.",
            ("/components/schemas/AgentNativeStatus/properties/error", "@anyOf"):
                "Clarifies opaque JSON error extensions; the integer code branch is now bounded by SafeInt.",
            ("/components/schemas/AgentResponse/properties/error", "@anyOf"):
                "Clarifies opaque JSON error extensions; the integer code branch is now bounded by SafeInt.",
            ("/components/schemas/BranchResponse/properties/error", "@anyOf"):
                "Clarifies opaque JSON error extensions; the integer code branch is now bounded by SafeInt.",
            ("/components/schemas/AgentResponse/properties/nativeStatus", "@anyOf"):
                "Clarifies the native status union; its integer code branch is bounded by SafeInt.",
            ("/components/schemas/BranchResponse/properties/nativeStatus", "@anyOf"):
                "Clarifies the native status union; its integer code branch is bounded by SafeInt.",
            ("/components/schemas/DesktopResponse/properties/supervisor", "@anyOf"):
                "Clarifies supervisor diagnostics; typed counters and identifiers are bounded by their runtime aliases.",
            ("/components/schemas/DiagnosticsResponse/properties/supervisor/properties/health", "@anyOf"):
                "Clarifies supervisor health diagnostics; typed counters and identifiers are bounded by their runtime aliases.",
            ("/components/schemas/SupervisorDiagnostics/properties/health", "@anyOf"):
                "Clarifies supervisor health diagnostics; typed counters and identifiers are bounded by their runtime aliases.",
        }
        self.assertEqual(
            {(item["pointer"], item["constraint"]) for item in loosenings},
            set(accepted_loosenings),
        )

    def test_constraint_diff_detects_five_removed_constraints(self) -> None:
        baseline_doc: dict[str, JsonValue] = {
            "paths": {
                "/example": {
                    "post": {
                        "parameters": [
                            {"name": "q", "in": "query", "schema": {"type": "string", "pattern": "^[0-9]+$"}},
                            {"name": "X-Mode", "in": "header", "schema": {"type": "string", "enum": ["a", "b"]}},
                        ],
                        "requestBody": {"content": {"application/json": {"schema": {
                            "type": "object", "properties": {"count": {"type": "integer", "maximum": 8}},
                        }}}},
                        "responses": {"200": {"description": "ok", "content": {
                            "application/json": {
                                "schema": {"$ref": "#/components/schemas/Nested"}
                            }
                        }}},
                    }
                }
            },
            "components": {"schemas": {"Nested": {
                "type": "object", "required": ["id", "kind"], "additionalProperties": False,
                "properties": {"id": {"type": "string"}, "kind": {"type": "string"}},
            }}},
        }
        candidate_doc = json.loads(json.dumps(baseline_doc))
        paths = candidate_doc["paths"]["/example"]["post"]
        paths["requestBody"]["content"]["application/json"]["schema"]["properties"]["count"].pop("maximum")
        paths["parameters"][0]["schema"].pop("pattern")
        paths["parameters"][1]["schema"]["enum"].remove("b")
        candidate_doc["components"]["schemas"]["Nested"]["required"].remove("kind")
        candidate_doc["components"]["schemas"]["Nested"].pop("additionalProperties")
        baseline = constraint_snapshot(baseline_doc)
        candidate = constraint_snapshot(candidate_doc)
        loosenings, tightenings = constraint_changes(baseline, candidate)
        self.assertEqual(
            {(item["constraint"], item["pointer"]) for item in loosenings},
            {
                ("maximum", "/paths/~1example/post/requestBody/content/application~1json/schema/properties/count"),
                ("pattern", "/paths/~1example/post/operationParameters/query/q/schema"),
                ("required", "/components/schemas/Nested"),
                ("additionalProperties", "/components/schemas/Nested"),
            },
        )
        self.assertEqual(
            {(item["constraint"], item["pointer"]) for item in tightenings},
            {("enum", "/paths/~1example/post/operationParameters/header/X-Mode/schema")},
        )

    def test_constraint_diff_ignores_unreachable_components(self) -> None:
        baseline_doc: dict[str, JsonValue] = {
            "paths": {
                "/example": {
                    "get": {"responses": {"200": {"content": {
                        "application/json": {"schema": {"type": "string"}}
                    }}}}
                }
            },
            "components": {"schemas": {
                "ValidationError": {"type": "object", "properties": {
                    "loc": {"type": "array", "items": {"type": "integer"}}
                }}
            }},
        }
        candidate_doc = json.loads(json.dumps(baseline_doc))
        candidate_doc["components"]["schemas"].pop("ValidationError")
        loosenings, tightenings = constraint_changes(
            constraint_snapshot(baseline_doc), constraint_snapshot(candidate_doc)
        )
        self.assertEqual(loosenings, [])
        self.assertEqual(tightenings, [])

    def test_constraint_diff_detects_operation_and_union_loosenings(self) -> None:
        baseline_doc: dict[str, JsonValue] = {
            "paths": {
                "/example": {
                    "get": {
                        "parameters": [{
                            "in": "query", "name": "agent", "required": True,
                            "schema": {"type": "string"},
                        }],
                        "responses": {
                            "200": {"description": "ok", "content": {
                                "application/json": {"schema": {
                                    "$ref": "#/components/schemas/SessionResponse",
                                }},
                            }},
                            "201": {"description": "keep schemas reachable", "content": {
                                "application/json": {"schema": {
                                    "type": "object", "properties": {
                                        "branch": {"$ref": "#/components/schemas/Branch"},
                                        "nested": {"$ref": "#/components/schemas/NestedBranch"},
                                        "enum": {"$ref": "#/components/schemas/EnumValue"},
                                        "session": {"$ref": "#/components/schemas/SessionResponse"},
                                    },
                                }},
                            }},
                        },
                    },
                    "post": {
                        "requestBody": {"required": True, "content": {
                            "application/json": {"schema": {"type": "string"}},
                        }},
                        "responses": {},
                    },
                },
            },
            "components": {"schemas": {
                "SessionResponse": {
                    "type": "object", "required": ["token"],
                    "properties": {"token": {"type": "string"}},
                    "additionalProperties": False,
                },
                "JsonValue": {},
                "Branch": {"anyOf": [
                    {"type": "integer", "minimum": 1},
                    {"type": "string"},
                ]},
                "NestedBranch": {"anyOf": [
                    {"type": "object", "properties": {
                        "item": {"type": "integer", "maximum": 3},
                    }},
                    {"type": "string"},
                ]},
                "EnumValue": {"type": "string", "enum": ["a", "b"]},
            }},
        }

        actual: dict[str, tuple[set[tuple[str, str]], set[tuple[str, str]]]] = {}
        expected: dict[str, tuple[set[tuple[str, str]], set[tuple[str, str]]]] = {}

        def record_mutation(
            label: str,
            candidate_doc: dict[str, JsonValue],
            expected_loosenings: set[tuple[str, str]],
            expected_tightenings: set[tuple[str, str]] = set(),
        ) -> None:
            loosenings, tightenings = constraint_changes(
                constraint_snapshot(baseline_doc), constraint_snapshot(candidate_doc)
            )
            actual[label] = (
                {(item["constraint"], item["pointer"]) for item in loosenings},
                {(item["constraint"], item["pointer"]) for item in tightenings},
            )
            expected[label] = (expected_loosenings, expected_tightenings)

        optional_parameter = json.loads(json.dumps(baseline_doc))
        optional_parameter["paths"]["/example"]["get"]["parameters"][0]["required"] = False
        record_mutation("required query parameter made optional", optional_parameter, {
            ("required", "/paths/~1example/get/operationParameters/query/agent"),
        })

        replacement_response = json.loads(json.dumps(baseline_doc))
        replacement_response["paths"]["/example"]["get"]["responses"]["200"]["content"][
            "application/json"
        ]["schema"] = {"$ref": "#/components/schemas/JsonValue"}
        record_mutation("response reference replaced by looser schema", replacement_response, {
            ("@responseSchema", "/paths/~1example/get/responses/200/content/application~1json/schema"),
        })

        replaced_enum = json.loads(json.dumps(baseline_doc))
        replaced_enum["components"]["schemas"]["EnumValue"]["enum"] = ["a", "c"]
        record_mutation("enum value replaced", replaced_enum, {
            ("enum", "/components/schemas/EnumValue"),
        }, {
            ("enum", "/components/schemas/EnumValue"),
        })

        relaxed_minimum = json.loads(json.dumps(baseline_doc))
        relaxed_minimum["components"]["schemas"]["Branch"]["anyOf"][0].pop("minimum")
        record_mutation("minimum removed in union branch", relaxed_minimum, {
            ("@anyOf", "/components/schemas/Branch"),
        })

        relaxed_maximum = json.loads(json.dumps(baseline_doc))
        relaxed_maximum["components"]["schemas"]["NestedBranch"]["anyOf"][0][
            "properties"
        ]["item"].pop("maximum")
        record_mutation("nested maximum removed in union branch", relaxed_maximum, {
            ("@anyOf", "/components/schemas/NestedBranch"),
        })

        optional_body = json.loads(json.dumps(baseline_doc))
        optional_body["paths"]["/example"]["post"]["requestBody"]["required"] = False
        record_mutation("request body made optional", optional_body, {
            ("required", "/paths/~1example/post/requestBody"),
        })

        renamed_parameter = json.loads(json.dumps(baseline_doc))
        renamed_parameter["paths"]["/example"]["get"]["parameters"][0]["name"] = "subject"
        record_mutation("parameter identity changed", renamed_parameter, {
            ("parameter", "/paths/~1example/get/operationParameters/query/agent"),
        }, {
            ("parameter", "/paths/~1example/get/operationParameters/query/subject"),
        })
        self.assertEqual(actual, expected, msg=repr(actual))

    def test_constraint_diff_resolves_recursive_response_refs_cycle_safely(self) -> None:
        document: dict[str, JsonValue] = {
            "paths": {"/recursive": {"get": {"responses": {"200": {
                "description": "ok",
                "content": {"application/json": {"schema": {
                    "$ref": "#/components/schemas/Recursive",
                }}},
            }}}}},
            "components": {"schemas": {"Recursive": {
                "type": "object",
                "properties": {"next": {"$ref": "#/components/schemas/Recursive"}},
            }}},
        }
        snapshot = constraint_snapshot(document)
        self.assertEqual(constraint_changes(snapshot, snapshot), ([], []))

    def test_optional_query_and_header_parameters_are_absent_not_nullable(self) -> None:
        document: dict[str, JsonValue] = {
            "paths": {
                "/example": {
                    "get": {
                        "parameters": [
                            {"in": "query", "name": "flag", "required": False,
                             "schema": {"anyOf": [{"const": "1", "type": "string"}, {"type": "null"}]}},
                            {"in": "header", "name": "X-Flag", "required": False,
                             "schema": {"anyOf": [{"type": "integer"}, {"type": "null"}]}},
                        ],
                        "responses": {},
                    }
                }
            }
        }
        normalize_optional_parameter_schemas(document)
        paths = document["paths"]
        assert isinstance(paths, dict)
        path = paths["/example"]
        assert isinstance(path, dict)
        operation = path["get"]
        assert isinstance(operation, dict)
        parameters = operation["parameters"]
        assert isinstance(parameters, list)
        first_parameter, second_parameter = parameters
        assert isinstance(first_parameter, dict)
        assert isinstance(second_parameter, dict)
        self.assertEqual(first_parameter["schema"], {"const": "1", "type": "string"})
        self.assertEqual(second_parameter["schema"], {"type": "integer"})

    def test_generated_document_bounds_every_integer_schema(self) -> None:
        document = openapi_document()

        def visit(value: JsonValue, pointer: str = "") -> None:
            if isinstance(value, dict):
                if value.get("type") == "integer":
                    self.assertIn("minimum", value, pointer)
                    self.assertIn("maximum", value, pointer)
                for key, child in value.items():
                    visit(child, f"{pointer}/{key}")
            elif isinstance(value, list):
                for index, child in enumerate(value):
                    visit(child, f"{pointer}/{index}")

        visit(document)

    def test_integer_schema_bounder_preserves_domains_and_bounds_consts(self) -> None:
        document: dict[str, JsonValue] = {
            "anyOf": [
                {"type": "integer", "x-safe-integer": "unsigned"},
                {"type": "integer", "minimum": 4, "maximum": 7},
                {"type": "integer", "const": 1},
            ]
        }
        bound_integer_schemas(document)
        variants = document["anyOf"]
        assert isinstance(variants, list)
        self.assertEqual(variants[0], {
            "type": "integer", "minimum": 0, "maximum": MAX_SAFE_INTEGER,
        })
        self.assertEqual(variants[1], {"type": "integer", "minimum": 4, "maximum": 7})
        self.assertEqual(variants[2], {"type": "integer", "const": 1, "minimum": 1, "maximum": 1})

    def test_schema_audit_rejects_types_excluded_by_union_branches(self) -> None:
        findings = _contradictory_union_findings({
            "parameters": [{
                "name": "Last-Event-ID",
                "schema": {"type": "integer", "anyOf": [{"type": "null"}, {"type": "string"}]},
            }],
        })
        self.assertEqual(findings, [{
            "kind": "type_excluded_by_union",
            "location": "openapi/parameters/0/schema",
            "keyword": "anyOf",
        }])

    def test_request_schema_audit_has_only_documented_exceptions(self) -> None:
        report = request_schema_audit()
        self.assertEqual(report["findings"], [])
        self.assertEqual(report["jsonSchemaExtra"], {"uses": 3, "validationUses": 1})
        self.assertEqual(
            report["signedRequestModelCoverage"],
            {"enforcedOperations": 4, "expectedOperations": 4},
        )
        exceptions = report["exceptions"]
        self.assertTrue(exceptions)
        self.assertIsInstance(exceptions, list)
        assert isinstance(exceptions, list)
        self.assertTrue(all(isinstance(item, dict) and "exception" in item for item in exceptions))

    def test_schema_extra_audit_rejects_schema_only_validation_keywords(self) -> None:
        with TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "route.py").write_text(
                'query = Query(json_schema_extra={"pattern": "^[0-9]+$"})\n',
                encoding="utf-8",
            )
            findings, counts = _json_schema_extra_findings(root)
        self.assertEqual(counts, {"uses": 1, "validationUses": 1})
        self.assertEqual(findings[0]["kind"], "schema_only_validation_constraint")
        self.assertEqual(findings[0]["keywords"], "pattern")

    def test_accepts_referenced_and_explicit_success_schema(self) -> None:
        document = sample_document({"$ref": "#/components/schemas/AgentEntityDto"})
        validate_contract_schemas(document)
        validate_error_responses(document)

    def test_accepts_shared_error_response_component_reference(self) -> None:
        document = sample_document({"type": "object"})
        responses = _responses(document)
        responses["400"] = {"$ref": "#/components/responses/ApiError400"}
        components = document["components"]
        assert isinstance(components, dict)
        components["responses"] = {
            "ApiError400": {
                "description": "Invalid request",
                "content": {
                    "application/json": {
                        "schema": {"$ref": "#/components/schemas/ErrorResponse"}
                    }
                },
            }
        }
        validate_error_responses(document)

    def test_rejects_missing_common_error_model(self) -> None:
        document = sample_document({"type": "string"})
        responses = _responses(document)
        del responses["400"]
        with self.assertRaisesRegex(ValueError, "400 must use ErrorResponse"):
            validate_error_responses(document)

    def test_rejects_422_validation_response(self) -> None:
        document = sample_document({"type": "string"})
        responses = _responses(document)
        responses["422"] = {
            "content": {
                "application/json": {
                    "schema": {"$ref": "#/components/schemas/HTTPValidationError"}
                }
            }
        }
        with self.assertRaisesRegex(ValueError, "request validation must use status 400"):
            validate_error_responses(document)

    def test_rejects_success_with_missing_schema(self) -> None:
        document = sample_document({})
        with self.assertRaisesRegex(ValueError, "no type or validation shape"):
            validate_contract_schemas(document)

    def test_rejects_unconstrained_object_success_schema(self) -> None:
        document = sample_document({"type": "object", "additionalProperties": True})
        with self.assertRaisesRegex(ValueError, "unconstrained additionalProperties"):
            validate_contract_schemas(document)

    def test_rejects_array_with_untyped_items(self) -> None:
        document = sample_document({"type": "array", "items": {}})
        with self.assertRaisesRegex(ValueError, "items: schema has no type or validation shape"):
            validate_contract_schemas(document)

    def test_accepts_tuple_schema_with_typed_prefix_items(self) -> None:
        document = sample_document(
            {
                "type": "array",
                "prefixItems": [
                    {"type": "string"},
                    {"anyOf": [
                        {"type": "integer", "minimum": 0, "maximum": MAX_SAFE_INTEGER},
                        {"type": "null"},
                    ]},
                    {"type": "integer", "minimum": 0, "maximum": MAX_SAFE_INTEGER},
                ],
                "minItems": 3,
                "maxItems": 3,
            }
        )
        validate_contract_schemas(document)

    def test_accepts_prefix_items_when_max_items_closes_tail(self) -> None:
        document = sample_document(
            {
                "type": "array",
                "prefixItems": [
                    {"type": "string"},
                    {"type": "integer", "minimum": 0, "maximum": MAX_SAFE_INTEGER},
                ],
                "maxItems": 2,
            }
        )
        validate_contract_schemas(document)

    def test_accepts_false_items_schema_as_closed_array_tail(self) -> None:
        document = sample_document(
            {"type": "array", "prefixItems": [{"type": "string"}], "items": False}
        )
        validate_contract_schemas(document)

    def test_rejects_prefix_items_with_untyped_tail(self) -> None:
        document = sample_document(
            {"type": "array", "prefixItems": [{"type": "string"}]}
        )
        with self.assertRaisesRegex(ValueError, "prefixItems permits an untyped array tail"):
            validate_contract_schemas(document)

    def test_rejects_boolean_max_items_as_tuple_tail_bound(self) -> None:
        document = sample_document(
            {
                "type": "array",
                "prefixItems": [{"type": "string"}],
                "maxItems": True,
            }
        )
        with self.assertRaisesRegex(ValueError, "prefixItems permits an untyped array tail"):
            validate_contract_schemas(document)

    def test_rejects_true_items_schema(self) -> None:
        document = sample_document({"type": "array", "items": True})
        with self.assertRaisesRegex(ValueError, "unconstrained item schema"):
            validate_contract_schemas(document)

    def test_rejects_tuple_schema_with_untyped_prefix_item(self) -> None:
        document = sample_document(
            {"type": "array", "prefixItems": [{"type": "string"}, {}]}
        )
        with self.assertRaisesRegex(ValueError, r"prefixItems\[1\].*no type or validation shape"):
            validate_contract_schemas(document)

    def test_rejects_untyped_one_of_branch(self) -> None:
        document = sample_document(
            {"oneOf": [{"type": "string"}, {}]}
        )
        with self.assertRaisesRegex(ValueError, r"oneOf\[1\].*no type or validation shape"):
            validate_contract_schemas(document)

    def test_rejects_integer_schema_without_bounds(self) -> None:
        document = sample_document({"type": "integer"})
        with self.assertRaisesRegex(ValueError, "integer schema has no minimum"):
            validate_contract_schemas(document)

    def test_rejects_query_parameter_without_schema(self) -> None:
        document = sample_document({"type": "string"})
        path_item = document["paths"]
        assert isinstance(path_item, dict)
        operation = path_item["/api/example"]
        assert isinstance(operation, dict)
        get_operation = operation["get"]
        assert isinstance(get_operation, dict)
        get_operation["parameters"] = [{"name": "limit", "in": "query", "required": True}]
        with self.assertRaisesRegex(ValueError, "has no schema"):
            validate_contract_schemas(document)

    def test_rejects_untyped_request_body_schema(self) -> None:
        document = sample_document({"type": "string"})
        path_item = document["paths"]
        assert isinstance(path_item, dict)
        operation = path_item["/api/example"]
        assert isinstance(operation, dict)
        get_operation = operation["get"]
        assert isinstance(get_operation, dict)
        get_operation["requestBody"] = {
            "content": {"application/json": {"schema": {"type": "array", "items": {}}}}
        }
        with self.assertRaisesRegex(ValueError, "requestBody.*items: schema has no type"):
            validate_contract_schemas(document)

    def test_allows_finite_recursive_json_value_schema(self) -> None:
        document = sample_document({"$ref": "#/components/schemas/JsonValue"})
        validate_contract_schemas(document)

    def test_rejects_pure_reference_cycles(self) -> None:
        document = sample_document({"type": "string"})
        components = document["components"]
        assert isinstance(components, dict)
        schemas = components["schemas"]
        assert isinstance(schemas, dict)
        schemas["AliasA"] = {"$ref": "#/components/schemas/AliasB"}
        schemas["AliasB"] = {"$ref": "#/components/schemas/AliasA"}
        with self.assertRaisesRegex(ValueError, "reference cycle has no concrete schema"):
            validate_contract_schemas(document)

    def test_drops_unreferenced_fastapi_validation_components(self) -> None:
        document = sample_document({"type": "string"})
        components = document["components"]
        assert isinstance(components, dict)
        schemas = components["schemas"]
        assert isinstance(schemas, dict)
        schemas["HTTPValidationError"] = {
            "type": "object",
            "properties": {
                "detail": {
                    "type": "array",
                    "items": {"$ref": "#/components/schemas/ValidationError"},
                }
            },
        }
        schemas["ValidationError"] = {
            "type": "object",
            "properties": {"ctx": {"type": "object", "additionalProperties": True}},
        }

        remove_orphan_fastapi_validation_schemas(document)
        validate_contract_schemas(document)

        self.assertNotIn("HTTPValidationError", schemas)
        self.assertNotIn("ValidationError", schemas)

    def test_keeps_referenced_fastapi_validation_component_for_strict_check(self) -> None:
        document = sample_document(
            {"$ref": "#/components/schemas/HTTPValidationError"}
        )
        components = document["components"]
        assert isinstance(components, dict)
        schemas = components["schemas"]
        assert isinstance(schemas, dict)
        schemas["HTTPValidationError"] = {
            "type": "object",
            "properties": {
                "detail": {
                    "type": "array",
                    "items": {"$ref": "#/components/schemas/ValidationError"},
                }
            },
        }
        schemas["ValidationError"] = {
            "type": "object",
            "properties": {"ctx": {"type": "object", "additionalProperties": True}},
        }

        remove_orphan_fastapi_validation_schemas(document)
        with self.assertRaisesRegex(ValueError, "unconstrained additionalProperties"):
            validate_contract_schemas(document)

    def test_entity_exports_are_named_and_sorted(self) -> None:
        names = entity_schema_names(sample_document({"type": "string"}))
        self.assertEqual(names, ["AgentEntityDto", "TaskEntityDto"])

    def test_normalization_installs_required_recursive_json_value_schema(self) -> None:
        document = sample_document({"type": "string"})
        components = document["components"]
        assert isinstance(components, dict)
        schemas = components["schemas"]
        assert isinstance(schemas, dict)
        self.assertEqual(schemas["JsonValue"], JSON_VALUE_SCHEMA)

    def test_json_value_normalization_requires_component(self) -> None:
        document = sample_document({"type": "string"})
        document["components"] = {"schemas": {}}
        with self.assertRaisesRegex(ValueError, "JsonValue"):
            normalize_json_value_schema(document)

    def test_openapi_typescript_emits_paths_components_and_entity_aliases(self) -> None:
        document = sample_document({"$ref": "#/components/schemas/AgentEntityDto"})
        generated = render(document)
        self.assertIn('export interface paths', generated)
        self.assertIn('export interface components', generated)
        self.assertIn('JsonValue: JsonValue;', generated)
        self.assertIn("export type JsonValue =", generated)
        self.assertIn("| JsonValue[]", generated)
        self.assertIn("{ [key: string]: JsonValue }", generated)
        self.assertIn('export type AgentEntityDto = components["schemas"]["AgentEntityDto"];', generated)
        self.assertIn('export type TaskEntityDto = components["schemas"]["TaskEntityDto"];', generated)
        self.assertIn('Status: "idle" | "running";', generated)

    def test_sync_pull_registers_entity_payload_union_and_all_dtos(self) -> None:
        document = openapi_document()
        components = document["components"]
        assert isinstance(components, dict)
        schemas = components["schemas"]
        assert isinstance(schemas, dict)
        entity_names = [name for name in schemas if name.endswith("EntityDto")]
        self.assertEqual(len(entity_names), 14)
        payload = schemas["SyncEntityPayload"]
        assert isinstance(payload, dict)
        variants = payload["oneOf"]
        assert isinstance(variants, list)
        self.assertEqual(len(variants), 15)
        discriminator = payload["discriminator"]
        assert isinstance(discriminator, dict)
        self.assertEqual(discriminator["propertyName"], "collection")
        mapping = discriminator["mapping"]
        assert isinstance(mapping, dict)
        self.assertEqual(len(mapping), 15)

        for collection, dto_model in _DTO_MODELS.items():
            with self.subTest(collection=collection):
                payload_ref = mapping[collection]
                assert isinstance(payload_ref, str)
                payload_name = payload_ref.rsplit("/", 1)[-1]
                payload_member = schemas[payload_name]
                assert isinstance(payload_member, dict)
                properties = payload_member["properties"]
                assert isinstance(properties, dict)
                value_schema = properties["value"]
                assert isinstance(value_schema, dict)
                self.assertEqual(
                    value_schema["$ref"],
                    f"#/components/schemas/{dto_model.__name__}",
                )

    def test_openapi_413_descriptions_are_explicit(self) -> None:
        document = openapi_document()
        paths = document["paths"]
        assert isinstance(paths, dict)
        affected: list[str] = []
        for path, path_item in paths.items():
            if not isinstance(path_item, dict):
                continue
            for method, operation in path_item.items():
                if not isinstance(operation, dict):
                    continue
                responses = operation.get("responses")
                if not isinstance(responses, dict) or "413" not in responses:
                    continue
                response = responses["413"]
                assert isinstance(response, dict)
                affected.append(f"{method.upper()} {path}")
                self.assertIn(
                    response.get("description"),
                    {"Content Too Large", "Invalid request size"},
                    f"{method.upper()} {path} must use an explicit HTTP 413 description",
                )
        self.assertTrue(affected, "OpenAPI must include at least one HTTP 413 response")

    def test_openapi_numeric_union_order_is_stable(self) -> None:
        document = openapi_document()
        paths = document["paths"]
        components = document["components"]
        assert isinstance(paths, dict)
        assert isinstance(components, dict)
        schemas = components["schemas"]
        assert isinstance(schemas, dict)

        def assert_numeric_unions(value: JsonValue, location: str) -> None:
            if isinstance(value, dict):
                variants = value.get("anyOf")
                if (
                    isinstance(variants, list)
                    and {
                        variant.get("type")
                        for variant in variants
                        if isinstance(variant, dict)
                        and variant.get("type") in {"integer", "number"}
                    }
                    == {"integer", "number"}
                ):
                    numeric_types = [
                        variant["type"]
                        for variant in variants
                        if isinstance(variant, dict)
                        and variant.get("type") in {"integer", "number"}
                    ]
                    self.assertEqual(
                        numeric_types,
                        ["integer", "number"],
                        location,
                    )
                for key, child in value.items():
                    assert_numeric_unions(child, f"{location}.{key}")
            elif isinstance(value, list):
                for index, child in enumerate(value):
                    assert_numeric_unions(child, f"{location}[{index}]")

        assert_numeric_unions(paths, "paths")
        assert_numeric_unions(schemas, "components.schemas")

    def test_generator_exports_the_named_sync_entity_payload_union(self) -> None:
        document = sample_document({"type": "string"})
        components = document["components"]
        assert isinstance(components, dict)
        schemas = components["schemas"]
        assert isinstance(schemas, dict)
        schemas["SyncEntityPayload"] = {
            "oneOf": [
                {"$ref": "#/components/schemas/AgentEntityDto"},
                {"$ref": "#/components/schemas/TaskEntityDto"},
            ]
        }
        generated = render(document)
        self.assertIn(
            'export type SyncEntityPayload = components["schemas"]["SyncEntityPayload"];',
            generated,
        )

    def test_generator_preserves_required_and_optional_defaulted_properties(self) -> None:
        document = sample_document({"$ref": "#/components/schemas/ResponseContract"})
        components = document["components"]
        assert isinstance(components, dict)
        schemas = components["schemas"]
        assert isinstance(schemas, dict)
        schemas["MonitorInput"] = {
            "type": "object",
            "properties": {
                "id": {"type": "string"},
                "text": {"type": "string", "default": ""},
                "rows": {
                    "type": "integer", "minimum": 0,
                    "maximum": MAX_SAFE_INTEGER, "default": 24,
                },
            },
            "required": ["id"],
        }
        schemas["ResponseContract"] = {
            "type": "object",
            "properties": {
                "id": {"type": "string"},
                "status": {"type": "string", "default": "idle"},
            },
            "required": ["id", "status"],
        }
        path_item = document["paths"]
        assert isinstance(path_item, dict)
        operation = path_item["/api/example"]
        assert isinstance(operation, dict)
        get_operation = operation["get"]
        assert isinstance(get_operation, dict)
        get_operation["requestBody"] = {
            "content": {
                "application/json": {"schema": {"$ref": "#/components/schemas/MonitorInput"}}
            }
        }

        generated = render(document)

        monitor_input = generated.split("MonitorInput: {", 1)[1].split("\n    };", 1)[0]
        response_contract = generated.split("ResponseContract: {", 1)[1].split("\n    };", 1)[0]
        self.assertIn("id: string", monitor_input)
        self.assertIn("text?: string", monitor_input)
        self.assertIn("rows?: number", monitor_input)
        self.assertIn("id: string", response_contract)
        self.assertIn("status: string", response_contract)


if __name__ == "__main__":
    unittest.main()
