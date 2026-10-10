"""OpenAPI schema coverage checks for generation."""
from __future__ import annotations

import unittest
from typing import cast

from codex_sync_entities import _DTO_MODELS
from studio_api.models import JsonValue
from studio_api.generate_types import render
from studio_api.schema import (
    entity_schema_names,
    normalize_json_value_schema,
    remove_orphan_fastapi_validation_schemas,
    JSON_VALUE_SCHEMA,
    validate_error_responses,
    validate_contract_schemas,
    openapi_document,
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
    return document


class SchemaContractTests(unittest.TestCase):
    def test_accepts_referenced_and_explicit_success_schema(self) -> None:
        document = sample_document({"$ref": "#/components/schemas/AgentEntityDto"})
        validate_contract_schemas(document)
        validate_error_responses(document)

    def test_rejects_missing_common_error_model(self) -> None:
        document = sample_document({"type": "string"})
        responses = _responses(document)
        del responses["400"]
        with self.assertRaisesRegex(ValueError, "400 must use ErrorResponse"):
            validate_error_responses(document)

    def test_rejects_fastapi_default_422_validation_schema(self) -> None:
        document = sample_document({"type": "string"})
        responses = _responses(document)
        responses["422"] = {
            "content": {
                "application/json": {
                    "schema": {"$ref": "#/components/schemas/HTTPValidationError"}
                }
            }
        }
        with self.assertRaisesRegex(ValueError, "default FastAPI 422"):
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
                    {"anyOf": [{"type": "integer"}, {"type": "null"}]},
                    {"type": "integer"},
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
                "prefixItems": [{"type": "string"}, {"type": "integer"}],
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
        self.assertEqual(len(variants), 14)
        discriminator = payload["discriminator"]
        assert isinstance(discriminator, dict)
        self.assertEqual(discriminator["propertyName"], "collection")
        mapping = discriminator["mapping"]
        assert isinstance(mapping, dict)
        self.assertEqual(len(mapping), 14)

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
                "rows": {"type": "integer", "default": 24},
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
