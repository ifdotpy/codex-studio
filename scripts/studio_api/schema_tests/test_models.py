"""Behavioral tests for the shared FastAPI contract models."""
from __future__ import annotations

import unittest
from enum import StrEnum
from typing import Annotated

from fastapi import FastAPI
from fastapi.testclient import TestClient
from pydantic import Field, ValidationError

from studio_api.models import ContractModel, ContractStrEnum, ErrorResponse, JsonValue, ResponseModel


class Action(ContractStrEnum):
    START = "start"
    STOP = "stop"


class Body(ContractModel):
    action: Action
    count: Annotated[int, Field(gt=0)] = 1


class ExtensiblePayload(ContractModel):
    value: JsonValue


class ModelContractTests(unittest.TestCase):
    def setUp(self) -> None:
        app = FastAPI()

        @app.post("/action", response_model=Body)
        def action(body: Body) -> Body:
            return body

        self.client = TestClient(app)

    def test_fastapi_json_enum_accepts_exact_wire_string(self) -> None:
        response = self.client.post("/action", json={"action": "start"})
        self.assertEqual(response.status_code, 200)
        self.assertEqual(response.json(), {"action": "start", "count": 1})

    def test_fastapi_json_enum_rejects_invalid_and_coerced_values(self) -> None:
        for value in ("launch", 1, True):
            with self.subTest(value=value):
                response = self.client.post("/action", json={"action": value})
                self.assertEqual(response.status_code, 422)

    def test_enum_rejects_non_string_python_coercions(self) -> None:
        for value in (b"start", 1, True):
            with self.subTest(value=value):
                with self.assertRaises(ValidationError):
                    Body.model_validate({"action": value})

    def test_models_reject_unknown_fields_and_integer_coercion(self) -> None:
        with self.assertRaises(ValidationError) as captured:
            Body.model_validate({"action": Action.START, "count": "2", "extra": True})
        error_types = {issue["type"] for issue in captured.exception.errors()}
        self.assertIn("extra_forbidden", error_types)
        self.assertIn("int_type", error_types)

    def test_json_value_accepts_json_values_and_rejects_non_finite_numbers(self) -> None:
        accepted = ExtensiblePayload.model_validate(
            {"value": {"items": [True, 3, 1.5, None, "text"]}}
        )
        self.assertEqual(accepted.value, {"items": [True, 3, 1.5, None, "text"]})
        for value in (float("nan"), float("inf"), {"nested": [float("-inf")]}):
            with self.subTest(value=value):
                with self.assertRaises(ValidationError):
                    ExtensiblePayload.model_validate({"value": value})

    def test_response_dump_preserves_omitted_versus_explicit_null(self) -> None:
        self.assertEqual(ResponseModel().wire_dump(), {})
        response = ResponseModel.model_validate({"_syncEntities": None})
        self.assertEqual(response.wire_dump(), {"_syncEntities": None})

    def test_sync_envelope_preserves_existing_wire_shape(self) -> None:
        response = ResponseModel.model_validate(
            {
                "_syncEntities": [
                    {
                        "id": "entity:task:one",
                        "seq": 9,
                        "payload": '{"collection":"task","id":"one","value":{}}',
                        "_deleted": False,
                    }
                ]
            }
        )
        self.assertEqual(
            response.wire_dump(),
            {
                "_syncEntities": [
                    {
                        "id": "entity:task:one",
                        "seq": 9,
                        "payload": '{"collection":"task","id":"one","value":{}}',
                        "_deleted": False,
                    }
                ]
            },
        )

    def test_error_response_keeps_documented_metadata(self) -> None:
        error = ErrorResponse.model_validate(
            {
                "error": "Invalid field",
                "outcome": "not_applied",
                "code": "invalid",
                "details": {"field": "id"},
            }
        )
        self.assertEqual(
            error.wire_dump(),
            {
                "error": "Invalid field",
                "outcome": "not_applied",
                "code": "invalid",
                "details": {"field": "id"},
            },
        )


if __name__ == "__main__":
    unittest.main()
