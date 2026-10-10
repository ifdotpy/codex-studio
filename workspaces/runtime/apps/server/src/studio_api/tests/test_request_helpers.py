"""Tests for shared HTTP request conversion helpers."""

from __future__ import annotations

import unittest

from starlette.requests import Request
from pydantic import Field

from studio_api.models import ContractModel
from studio_api.request_helpers import body_data, first_nonempty_query


class _AliasedBody(ContractModel):
    request_id: str = Field(alias="requestId")
    optional: str | None = None


class RequestHelperTests(unittest.TestCase):
    def test_body_keeps_default_aliases_and_explicit_nulls(self) -> None:
        body = _AliasedBody.model_validate({"requestId": "stable", "optional": None})

        self.assertEqual(body_data(body), {"request_id": "stable", "optional": None})
        self.assertEqual(
            body.model_dump(mode="json", by_alias=True, exclude_unset=True),
            {"requestId": "stable", "optional": None},
        )
        self.assertNotIn("optional", body_data(_AliasedBody.model_validate({"requestId": "stable"})))

    def test_first_nonempty_query_preserves_order_and_default(self) -> None:
        request = Request({"type": "http", "query_string": b"value=&value=first&value=second"})

        self.assertEqual(first_nonempty_query(request, "value"), "first")
        self.assertEqual(first_nonempty_query(request, "missing", "fallback"), "fallback")
