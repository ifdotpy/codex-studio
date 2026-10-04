"""OpenAPI schema coverage checks for generation."""
from __future__ import annotations

import unittest
from typing import cast

from studio_api.models import JsonValue
from studio_api.generate_types import render
from studio_api.schema import (
    entity_schema_names,
    normalize_json_value_schema,
    require_json_value_schema,
    validate_error_responses,
    validate_contract_schemas,
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

    def test_entity_exports_are_named_and_sorted(self) -> None:
        names = entity_schema_names(sample_document({"type": "string"}))
        self.assertEqual(names, ["AgentEntityDto", "TaskEntityDto"])

    def test_shared_json_value_schema_is_required(self) -> None:
        require_json_value_schema(sample_document({"type": "string"}))
        document = sample_document({"type": "string"})
        document["components"] = {"schemas": {}}
        with self.assertRaisesRegex(ValueError, "JsonValue"):
            require_json_value_schema(document)

    def test_openapi_typescript_emits_paths_components_and_entity_aliases(self) -> None:
        document = sample_document({"$ref": "#/components/schemas/AgentEntityDto"})
        generated = render(document)
        self.assertIn('export interface paths', generated)
        self.assertIn('export interface components', generated)
        self.assertIn('export type AgentEntityDto = components["schemas"]["AgentEntityDto"];', generated)
        self.assertIn('export type TaskEntityDto = components["schemas"]["TaskEntityDto"];', generated)
        self.assertIn('Status: "idle" | "running";', generated)


if __name__ == "__main__":
    unittest.main()
