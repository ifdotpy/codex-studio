"""OpenAPI schema coverage checks for generation."""
from __future__ import annotations

import ast
import unittest
import json
import hashlib
from pathlib import Path
from tempfile import TemporaryDirectory
from typing import cast

from codex_layout import REPOSITORY_ROOT
from codex_agent_modes import (
    DEFAULT_MAX_TEAM_AGENTS,
    DEFAULT_SUBAGENT_CONCURRENCY,
    MAX_SUBAGENT_CONCURRENCY,
    MAX_TEAM_AGENTS,
    MIN_SUBAGENT_CONCURRENCY,
    MIN_TEAM_AGENTS,
)
from pydantic import TypeAdapter, ValidationError
from codex_sync_entities import _DTO_MODELS
from studio_api.models import (
    MAX_INT64,
    MAX_SAFE_INTEGER,
    MIN_INT64,
    MIN_SAFE_INTEGER,
    JsonValue,
    SafeInt,
    SafeIntHeader,
    SafeQueryInt,
    SignedSafeInt,
    Int64,
    UInt64,
)
from studio_api.work.models import (
    PersistedToolRequest,
    _record_response_model,
)
from codex_records import SafeNumber, WorktreeCleanupRecord
import codex_records as record_contracts
from studio_api.io.models import TerminalRecord
from studio_api.work.models import RuleRecord, WorkItem
from studio_api.generate_state_schemas import render_rust_agent_limits
from studio_api.generate_types import render, render_schema_identity
from studio_api.persisted_account_transfer import PersistedAccountTransferRecord
from studio_api.schema import (
    canonical_openapi_json,
    entity_schema_names,
    bound_integer_schemas,
    normalize_json_value_schema,
    remove_orphan_fastapi_validation_schemas,
    JSON_VALUE_SCHEMA,
    normalize_optional_parameter_schemas,
    validate_error_responses,
    validate_contract_schemas,
    api_schema_hash,
    openapi_document,
)
from studio_api.request_schema_audit import (
    _contradictory_union_findings,
    _json_schema_extra_findings,
    request_schema_audit,
)
from studio_api.schema_tests.constraint_diff import (
    _canonical,
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
    def test_agent_capacity_contracts_derive_from_runtime_limits(self) -> None:
        document = openapi_document()
        components = document["components"]
        assert isinstance(components, dict)
        schemas = components["schemas"]
        assert isinstance(schemas, dict)
        expected = {
            "concurrency": (MIN_SUBAGENT_CONCURRENCY, DEFAULT_SUBAGENT_CONCURRENCY, MAX_SUBAGENT_CONCURRENCY),
            "maxAgents": (MIN_TEAM_AGENTS, DEFAULT_MAX_TEAM_AGENTS, MAX_TEAM_AGENTS),
        }
        for model_name in ("CreateAgentRequest", "ConfigureRequest"):
            model = schemas[model_name]
            assert isinstance(model, dict)
            properties = model["properties"]
            assert isinstance(properties, dict)
            for field, (minimum, default, maximum) in expected.items():
                schema = properties[field]
                assert isinstance(schema, dict)
                self.assertEqual(schema["minimum"], minimum)
                self.assertEqual(schema["default"], default)
                self.assertEqual(schema["maximum"], maximum)
        conversation = schemas["ConversationRequest"]
        assert isinstance(conversation, dict)
        conversation_properties = conversation["properties"]
        assert isinstance(conversation_properties, dict)
        concurrency = conversation_properties["subagent_concurrency"]
        assert isinstance(concurrency, dict)
        self.assertEqual(concurrency["minimum"], MIN_SUBAGENT_CONCURRENCY)
        self.assertEqual(concurrency["default"], DEFAULT_SUBAGENT_CONCURRENCY)
        self.assertEqual(concurrency["maximum"], MAX_SUBAGENT_CONCURRENCY)

        persisted_schema = PersistedAccountTransferRecord.model_json_schema()
        self.assertEqual(persisted_schema["properties"]["members"]["maxProperties"], MAX_TEAM_AGENTS)
        generated_identity = render_schema_identity(api_schema_hash(document), document)
        for name, value in (
            ("MIN_SUBAGENT_CONCURRENCY", MIN_SUBAGENT_CONCURRENCY),
            ("DEFAULT_SUBAGENT_CONCURRENCY", DEFAULT_SUBAGENT_CONCURRENCY),
            ("MAX_SUBAGENT_CONCURRENCY", MAX_SUBAGENT_CONCURRENCY),
            ("MIN_TEAM_AGENTS", MIN_TEAM_AGENTS),
            ("DEFAULT_MAX_TEAM_AGENTS", DEFAULT_MAX_TEAM_AGENTS),
            ("MAX_TEAM_AGENTS", MAX_TEAM_AGENTS),
        ):
            self.assertIn(f"export const {name} = {value};", generated_identity)
        generated_api = REPOSITORY_ROOT / "workspaces/client/apps/web/src/generated/apiSchema.ts"
        self.assertEqual(generated_api.read_text(encoding="utf-8"), generated_identity)
        generated_rust = REPOSITORY_ROOT / "workspaces/runtime/packages/runtime-schema/src/agent_limits_generated.rs"
        self.assertEqual(generated_rust.read_text(encoding="utf-8"), render_rust_agent_limits())

    def test_openapi_defaults_ignore_unrelated_same_named_models(self) -> None:
        from pydantic import BaseModel

        before = canonical_openapi_json(openapi_document())
        type(
            "ConfigureRequest",
            (BaseModel,),
            {"__annotations__": {"unrelated": str}, "unrelated": "value"},
        )
        after = canonical_openapi_json(openapi_document())
        self.assertEqual(after, before)

    def test_openapi_documents_explicit_null_defaults(self) -> None:
        document = openapi_document()
        components = document["components"]
        assert isinstance(components, dict)
        schemas = components["schemas"]
        assert isinstance(schemas, dict)
        configure = schemas["ConfigureRequest"]
        assert isinstance(configure, dict)
        properties = configure["properties"]
        assert isinstance(properties, dict)
        max_agents = properties["maxAgents"]
        assert isinstance(max_agents, dict)
        self.assertIn("default", max_agents)
        self.assertEqual(max_agents["default"], 64)
        token_budget = properties["tokenBudget"]
        assert isinstance(token_budget, dict)
        self.assertIn("default", token_budget)
        self.assertIsNone(token_budget["default"])

    def test_constraint_diff_ignores_documentation_defaults(self) -> None:
        self.assertEqual(
            _canonical({"type": "string", "default": None}),
            _canonical({"type": "string"}),
        )

    def test_record_integer_domains_are_explicit_on_typed_dict_fields(self) -> None:
        source = Path(record_contracts.__file__).read_text(encoding="utf-8")
        tree = ast.parse(source)
        classes = {
            node.name: node for node in tree.body if isinstance(node, ast.ClassDef)
        }
        typed_dicts: set[str] = set()
        while True:
            discovered = {
                name for name, node in classes.items()
                if any(
                    isinstance(base, ast.Name)
                    and (base.id == "TypedDict" or base.id in typed_dicts)
                    for base in node.bases
                )
            }
            if discovered <= typed_dicts:
                break
            typed_dicts.update(discovered)

        checked_fields = 0
        for name in typed_dicts:
            for statement in classes[name].body:
                if not isinstance(statement, ast.AnnAssign):
                    continue
                checked_fields += 1
                bare_integer_nodes = [
                    child for child in ast.walk(statement.annotation)
                    if isinstance(child, ast.Name) and child.id == "int"
                ]
                with self.subTest(record=name, field=getattr(statement.target, "id", "?")):
                    self.assertEqual(
                        bare_integer_nodes,
                        [],
                        "record fields must state SafeInt, SignedSafeInt, UInt64, or Int64 explicitly",
                    )
        self.assertGreater(checked_fields, 500)

    def test_integer_alias_runtime_and_exported_bounds_agree(self) -> None:
        cases = (
            (SafeInt, 0, MAX_SAFE_INTEGER, (-1, MAX_SAFE_INTEGER + 1)),
            (SignedSafeInt, MIN_SAFE_INTEGER, MAX_SAFE_INTEGER,
             (MIN_SAFE_INTEGER - 1, MAX_SAFE_INTEGER + 1)),
            (UInt64, 0, MAX_INT64, (-1, MAX_INT64 + 1)),
            (Int64, MIN_INT64, MAX_INT64, (MIN_INT64 - 1, MAX_INT64 + 1)),
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

        number_adapter = TypeAdapter(SafeNumber)
        number_schema = number_adapter.json_schema()
        self.assertEqual(number_schema["type"], "number")
        self.assertEqual(number_schema["minimum"], MIN_SAFE_INTEGER)
        self.assertEqual(number_schema["maximum"], MAX_SAFE_INTEGER)
        for number_value in (MIN_SAFE_INTEGER, 0, 1.5, MAX_SAFE_INTEGER):
            number_adapter.validate_python(number_value)
        for invalid_number in (MIN_SAFE_INTEGER - 1, MAX_SAFE_INTEGER + 1):
            with self.assertRaises(ValidationError):
                number_adapter.validate_python(invalid_number)

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

    def test_large_response_producers_fit_their_runtime_and_documented_domains(self) -> None:
        from codex_agent_management import _file_identity

        with TemporaryDirectory() as temp:
            root = Path(temp)
            git_file = root / ".git"
            git_file.write_text("gitdir: .git/worktrees/example\n", encoding="utf-8")
            git_identity = _file_identity(git_file)
            root_identity = _file_identity(root)
            self.assertEqual(git_identity[4], git_file.stat().st_mtime_ns)
            WorktreeCleanupResponse = _record_response_model(
                "WorktreeCleanupRecordResponse", WorktreeCleanupRecord
            )
            cleanup = WorktreeCleanupResponse.model_validate({
                    "root": str(root), "repo": str(root), "relative": ".",
                    "identity": [], "gitFile": git_identity, "rootFile": root_identity,
            })
            self.assertEqual(cleanup.gitFile, git_identity)

        monotonic_ns = MAX_SAFE_INTEGER + 1
        request = PersistedToolRequest.model_validate({
            "id": "tool-request-1", "stage": "queued",
            "timing": {"reservationBeganAt": monotonic_ns},
        })
        self.assertEqual(request.timing.reservationBeganAt, monotonic_ns)

        document = json.loads(
            (REPOSITORY_ROOT / "workspaces/runtime/packages/runtime-http/openapi.json")
            .read_text(encoding="utf-8")
        )
        schemas = document["components"]["schemas"]
        cleanup_file_integer = schemas["WorktreeCleanupRecordResponse"]["properties"]["gitFile"]["items"]
        timing_integer = schemas["ToolRequestTimingRecordResponse"]["properties"]["reservationBeganAt"]
        self.assertEqual(cleanup_file_integer["x-integer-domain"], "signed64")
        self.assertEqual(cleanup_file_integer["minimum"], MIN_INT64)
        self.assertEqual(cleanup_file_integer["maximum"], MAX_INT64)
        self.assertEqual(timing_integer["x-integer-domain"], "unsigned64")
        self.assertEqual(timing_integer["minimum"], 0)
        self.assertEqual(timing_integer["maximum"], MAX_INT64)

        signal_returncode = -15
        cleanup_error = WorktreeCleanupResponse.model_validate({
            "root": str(root), "repo": str(root), "relative": ".", "identity": [],
            "error": {"message": "terminated", "at": 1.0, "by": "test",
                      "returncode": signal_returncode},
        })
        self.assertEqual(cleanup_error.error.returncode, signal_returncode)
        returncode_schema = schemas["WorktreeCleanupErrorRecordResponse"]["properties"]["returncode"]
        self.assertEqual(returncode_schema["type"], "integer")
        self.assertLessEqual(returncode_schema["minimum"], signal_returncode)
        self.assertGreaterEqual(returncode_schema["maximum"], signal_returncode)

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
            ("/components/schemas/ConfigureRequest/properties/concurrency", "maximum"):
                ("512.0", "2048.0", "Raises the approved subagent concurrency ceiling to support 2048 configured slots."),
            ("/components/schemas/ConfigureRequest/properties/maxAgents", "maximum"):
                ("1024.0", "2050.0", "Allows the lead, 2048 workers, and one queued worker."),
            ("/components/schemas/ConversationRequest/properties/subagent_concurrency", "maximum"):
                ("512.0", "2048.0", "Raises the approved subagent concurrency ceiling to support 2048 configured slots."),
            ("/components/schemas/CreateAgentRequest/properties/concurrency", "maximum"):
                ("512.0", "2048.0", "Raises the approved subagent concurrency ceiling to support 2048 configured slots."),
            ("/components/schemas/CreateAgentRequest/properties/maxAgents", "maximum"):
                ("1024.0", "2050.0", "Allows the lead, 2048 workers, and one queued worker."),
            ("/paths/~1api~1agents/post/requestBody/content/application~1json/schema/properties/concurrency", "maximum"):
                ("512.0", "2048.0", "Raises the approved subagent concurrency ceiling to support 2048 configured slots."),
            ("/paths/~1api~1agents/post/requestBody/content/application~1json/schema/properties/maxAgents", "maximum"):
                ("1024.0", "2050.0", "Allows the lead, 2048 workers, and one queued worker."),
            ("/paths/~1api~1configure/post/requestBody/content/application~1json/schema/properties/concurrency", "maximum"):
                ("512.0", "2048.0", "Raises the approved subagent concurrency ceiling to support 2048 configured slots."),
            ("/paths/~1api~1configure/post/requestBody/content/application~1json/schema/properties/maxAgents", "maximum"):
                ("1024.0", "2050.0", "Allows the lead, 2048 workers, and one queued worker."),
            ("/paths/~1api~1conversation/post/requestBody/content/application~1json/schema/properties/subagent_concurrency", "maximum"):
                ("512.0", "2048.0", "Raises the approved subagent concurrency ceiling to support 2048 configured slots."),
            ("/components/schemas/DraftDocumentInput/properties/deleted", "property"):
                ("absent from closed object", "accepted property", "Adds the deleted state to the draft input DTO."),
            ("/components/schemas/DraftPushRequest/properties/rows/items/properties/newDocumentState/properties/deleted", "property"):
                ("absent from closed object", "accepted property", "Adds the deleted state to the nested draft push DTO."),
            ("/components/schemas/DraftPushRow/properties/newDocumentState/properties/deleted", "property"):
                ("absent from closed object", "accepted property", "Adds the deleted state to the draft push row DTO."),
            ("/components/schemas/ErrorResponse/properties/location", "property"):
                ("absent from closed object", "accepted property", "Adds the optional error location field."),
            ("/components/schemas/ErrorResponse/properties/location", "@nullable"):
                ("None", "True", "Allows a null error location."),
            ("/components/schemas/ErrorResponse/properties/location", "@nullableBranch"):
                ("None", "['{\"items\":{\"type\":\"string\"}}']", "Adds the nullable branch for the error location field."),
            ("/components/schemas/RuleList/properties/rules/items/properties/fingerprint", "@nullableBranch"):
                ("sha256:56c216b5c10586d052770d6813aecaff1352ecf11d836081bf2f7426c961cff6", "sha256:0da4a39db36966f70408d5e4a554f4051ea5f0de4ad7aff40b512e27109f4a1c", "Publishes the recursively typed JSON value for a rule fingerprint; the runtime already accepts nested JSON values."),
            ("/components/schemas/RuleRecordResponse/properties/fingerprint", "@nullableBranch"):
                ("sha256:56c216b5c10586d052770d6813aecaff1352ecf11d836081bf2f7426c961cff6", "sha256:0da4a39db36966f70408d5e4a554f4051ea5f0de4ad7aff40b512e27109f4a1c", "Publishes the recursively typed JSON value for a rule fingerprint; the runtime already accepts nested JSON values."),
            ("/paths/~1api~1sync~1drafts/post/requestBody/content/application~1json/schema/properties/rows/items/properties/newDocumentState/properties/deleted", "property"):
                ("absent from closed object", "accepted property", "Publishes the deleted state in the sync drafts request schema."),
            ("/paths/~1api~1analytics/get/operationParameters/query/from_time", "parameter"):
                ("'absent'", "'present'", "Documents the client's from_time filter, which the insights model maps to its legacy 'from' option."),
            ("/paths/~1api~1analytics/get/operationParameters/query/to_time", "parameter"):
                ("'absent'", "'present'", "Documents the client's to_time filter, which the insights model maps to its legacy 'to' option."),
            ("/paths/~1api~1monitor~1log/get/operationParameters/header/Range", "parameter"):
                ("'absent'", "'present'", "Documents the standard optional HTTP Range header already interpreted by the handler."),
            ("/paths/~1api~1sync~1stream/get/operationParameters/header/Last-Event-ID", "parameter"):
                ("'absent'", "'present'", "Documents the optional event cursor header now parsed by the shared safe-integer type."),
            ("/paths/~1api~1sync~1stream/get/operationParameters/header/X-Codex-Sync-Protocol", "parameter"):
                ("'absent'", "'present'", "Documents the optional protocol negotiation header already read by the stream handler."),
            ("/paths/~1api~1sync~1stream/get/operationParameters/query/apiSchema", "parameter"):
                ("'absent'", "'present'", "Documents the optional API schema query used by the existing client compatibility handshake."),
            ("/components/schemas/SupervisorHandle/properties/jobIdentity", "property"):
                ("absent from closed object", "accepted property", "Documents the optional nullable job identity already emitted by supervisor handles."),
            ("/components/schemas/SupervisorHandle/properties/jobIdentity", "@nullable"):
                ("None", "True", "Supervisor handles may emit a null job identity."),
            ("/components/schemas/SupervisorHandle/properties/jobIdentity", "@nullableBranch"):
                ("None", "['{}']", "Adds the nullable branch for the existing supervisor job identity."),
            ("/components/schemas/SupervisorHealth/properties/handles/items/properties/jobIdentity", "property"):
                ("absent from closed object", "accepted property", "Publishes the optional nullable job identity on supervisor health handles."),
            ("/components/schemas/SupervisorHealth/properties/handles/items/properties/jobIdentity", "@nullable"):
                ("None", "True", "Supervisor health handles may emit a null job identity."),
            ("/components/schemas/SupervisorHealth/properties/handles/items/properties/jobIdentity", "@nullableBranch"):
                ("None", "['{}']", "Adds the nullable branch for the existing supervisor health job identity."),
        }
        def accepted_fragment(pointer: str, value: str) -> str:
            if pointer in {
                "/components/schemas/RuleList/properties/rules/items/properties/fingerprint",
                "/components/schemas/RuleRecordResponse/properties/fingerprint",
            }:
                return "sha256:" + hashlib.sha256(value.encode()).hexdigest()
            return value

        self.assertEqual(
            {
                (
                    item["pointer"], item["constraint"],
                    accepted_fragment(item["pointer"], item["baseline"]),
                    accepted_fragment(item["pointer"], item["candidate"]),
                )
                for item in loosenings
            },
            {
                (path, constraint, before, after)
                for (path, constraint), (before, after, _reason) in accepted_loosenings.items()
            },
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

    def test_constraint_diff_treats_bounded_number_as_tighter_than_integer_number_union(self) -> None:
        old: dict[str, dict[str, JsonValue]] = {
            "/value": {
                "@schemaPresent": True,
                "@anyOf": ['{"type":"integer"}', '{"type":"number"}'],
            }
        }
        new: dict[str, dict[str, JsonValue]] = {
            "/value": {
                "@schemaPresent": True,
                "type": "number",
                "minimum": MIN_SAFE_INTEGER,
                "maximum": MAX_SAFE_INTEGER,
            }
        }
        loosenings, _ = constraint_changes(old, new)
        self.assertEqual(loosenings, [])

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

    def test_constraint_diff_detects_nested_union_and_map_loosenings(self) -> None:
        baseline_doc: dict[str, JsonValue] = {
            "paths": {"/fixture": {"get": {"responses": {"200": {
                "description": "ok",
                "content": {"application/json": {"schema": {
                    "$ref": "#/components/schemas/ContractFixture",
                }}},
            }}}}},
            "components": {"schemas": {
                "ContractFixture": {
                    "type": "object",
                    "properties": {
                        "nativeStatus": {"anyOf": [
                            {"$ref": "#/components/schemas/NativeStatus"},
                            {"$ref": "#/components/schemas/NativeStatusValue"},
                            {"type": "null"},
                        ]},
                        "requiredUnion": {"anyOf": [
                            {"type": "object", "required": ["id"], "properties": {
                                "id": {"type": "string"},
                            }},
                            {"type": "string"},
                        ]},
                        "arrayUnion": {"anyOf": [
                            {"type": "array", "items": {
                                "type": "integer", "minimum": 0, "maximum": 5,
                            }},
                            {"type": "string"},
                        ]},
                        "mapUnion": {"anyOf": [
                            {"type": "object", "properties": {
                                "values": {"type": "object", "additionalProperties": {
                                    "type": "object", "additionalProperties": False,
                                }},
                            }},
                            {"type": "string"},
                        ]},
                    },
                },
                "NativeStatus": {"type": "object", "properties": {
                    "phase": {"type": "string"},
                }},
                "NativeStatusValue": {"type": "string", "enum": ["idle", "retrying"]},
            }},
        }
        baseline = constraint_snapshot(baseline_doc)

        mutations: list[tuple[str, dict[str, JsonValue], str]] = []
        enum_widened = json.loads(json.dumps(baseline_doc))
        enum_widened["components"]["schemas"]["NativeStatusValue"]["enum"].append("new")
        mutations.append((
            "enum added in nativeStatus union",
            enum_widened,
            "/components/schemas/ContractFixture/properties/nativeStatus",
        ))

        required_removed = json.loads(json.dumps(baseline_doc))
        required_removed["components"]["schemas"]["ContractFixture"]["properties"][
            "requiredUnion"
        ]["anyOf"][0]["required"].clear()
        mutations.append((
            "required member removed in union branch",
            required_removed,
            "/components/schemas/ContractFixture/properties/requiredUnion",
        ))

        array_widened = json.loads(json.dumps(baseline_doc))
        item = array_widened["components"]["schemas"]["ContractFixture"]["properties"][
            "arrayUnion"
        ]["anyOf"][0]["items"]
        item["type"] = "number"
        item.pop("minimum")
        item.pop("maximum")
        mutations.append((
            "array item widened in union branch",
            array_widened,
            "/components/schemas/ContractFixture/properties/arrayUnion",
        ))

        map_widened = json.loads(json.dumps(baseline_doc))
        map_widened["components"]["schemas"]["ContractFixture"]["properties"][
            "mapUnion"
        ]["anyOf"][0]["properties"]["values"]["additionalProperties"].pop(
            "additionalProperties"
        )
        mutations.append((
            "additionalProperties removed in map value",
            map_widened,
            "/components/schemas/ContractFixture/properties/mapUnion",
        ))

        # The checker must descend through all applicator keywords and report
        # constraints below array items and schema-valued map values.
        for applicator in ("anyOf", "oneOf", "allOf"):
            item_doc: dict[str, JsonValue] = {
                "paths": {"/item": {"get": {"responses": {"200": {
                    "content": {"application/json": {"schema": {
                        "$ref": "#/components/schemas/Container",
                    }}},
                }}}}},
                "components": {"schemas": {"Container": {
                    applicator: [
                        {"type": "array", "items": {
                            "type": "integer", "minimum": 0, "maximum": 7,
                        }},
                        {"type": "string"},
                    ],
                }}},
            }
            relaxed_item = json.loads(json.dumps(item_doc))
            relaxed_item["components"]["schemas"]["Container"][applicator][0][
                "items"
            ].pop("maximum")
            item_path = f"/components/schemas/Container"
            item_changes, _ = constraint_changes(
                constraint_snapshot(item_doc), constraint_snapshot(relaxed_item)
            )
            with self.subTest(applicator=applicator, nested="array-items"):
                self.assertIn(
                    (item_path, f"@{applicator}"),
                    {(change["pointer"], change["constraint"]) for change in item_changes},
                )

        map_bound_doc: dict[str, JsonValue] = {
            "paths": {"/map": {"get": {"responses": {"200": {
                "content": {"application/json": {"schema": {
                    "$ref": "#/components/schemas/Container",
                }}},
            }}}}},
            "components": {"schemas": {"Container": {"anyOf": [
                {"type": "object", "additionalProperties": {
                    "type": "integer", "minimum": 0, "maximum": 7,
                }},
                {"type": "string"},
            ]}}},
        }
        relaxed_map_bound = json.loads(json.dumps(map_bound_doc))
        relaxed_map_bound["components"]["schemas"]["Container"]["anyOf"][0][
            "additionalProperties"
        ].pop("maximum")
        map_bound_changes, _ = constraint_changes(
            constraint_snapshot(map_bound_doc), constraint_snapshot(relaxed_map_bound)
        )
        self.assertIn(
            ("/components/schemas/Container", "@anyOf"),
            {(change["pointer"], change["constraint"]) for change in map_bound_changes},
        )

        for label, mutated_doc, expected_pointer in mutations:
            with self.subTest(label=label):
                loosenings, _ = constraint_changes(baseline, constraint_snapshot(mutated_doc))
                self.assertIn(
                    (expected_pointer, "@anyOf"),
                    {(item["pointer"], item["constraint"]) for item in loosenings},
                )

    def test_accepted_union_change_does_not_cover_native_status_enum_widening(self) -> None:
        baseline_path = Path(__file__).with_name("openapi_constraint_baseline.json")
        manifest = json.loads(baseline_path.read_text(encoding="utf-8"))
        baseline_document = manifest["document"]
        candidate_document = json.loads(
            (REPOSITORY_ROOT / manifest["sourcePath"]).read_text(encoding="utf-8")
        )
        baseline_document = json.loads(json.dumps(baseline_document))
        candidate_document["components"]["schemas"]["AgentNativeStatusValue"][
            "enum"
        ].append("unreviewed-status")

        loosenings, _ = constraint_changes(
            constraint_snapshot(baseline_document),
            constraint_snapshot(candidate_document),
        )
        enum_changes = [
            change for change in loosenings
            if change["pointer"] == "/components/schemas/AgentNativeStatusValue"
            and change["constraint"] == "enum"
        ]
        self.assertEqual(len(enum_changes), 1)
        self.assertIn("unreviewed-status", enum_changes[0]["candidate"])

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
        self.assertEqual(report["jsonSchemaExtra"], {"uses": 5, "validationUses": 1})
        self.assertEqual(
            report["signedRequestModelRegistration"],
            {"registeredOperations": 4, "expectedOperations": 4},
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
