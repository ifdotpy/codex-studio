"""OpenAPI schema coverage checks for generation."""
from __future__ import annotations

import unittest
from typing import cast

from studio_api.models import JsonValue
from studio_api.generate_types import render
from studio_api.schema import (
    entity_schema_names,
    require_json_value_schema,
    validate_error_responses,
    validate_success_responses,
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
    return {
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
                "JsonValue": {"anyOf": [{"type": "string"}, {"type": "null"}]},
                "ErrorResponse": {
                    "type": "object",
                    "properties": {"error": {"type": "string"}},
                    "required": ["error"],
                    "additionalProperties": False,
                },
                "AgentEntityDto": {"type": "object", "properties": {"id": {"type": "string"}}},
                "TaskEntityDto": {"type": "object", "properties": {"id": {"type": "string"}}},
            }
        },
    }


class SchemaContractTests(unittest.TestCase):
    def test_accepts_referenced_and_explicit_success_schema(self) -> None:
        document = sample_document({"$ref": "#/components/schemas/AgentEntityDto"})
        validate_success_responses(document)
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
        with self.assertRaisesRegex(ValueError, "opaque JSON response schema"):
            validate_success_responses(document)

    def test_rejects_unconstrained_object_success_schema(self) -> None:
        document = sample_document({"type": "object", "additionalProperties": True})
        with self.assertRaisesRegex(ValueError, "opaque JSON response schema"):
            validate_success_responses(document)

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


if __name__ == "__main__":
    unittest.main()
