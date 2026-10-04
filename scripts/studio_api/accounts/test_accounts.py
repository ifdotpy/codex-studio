"""Contract checks for the accounts API module."""

from __future__ import annotations

import unittest
from uuid import uuid4

from pydantic import ValidationError

from .models import (
    Account,
    AccountsResponse,
    ModelCatalogResponse,
    ProjectWriteRequest,
    ResetRequest,
    UsageLimitsResponse,
)


class AccountsModelTests(unittest.TestCase):
    def test_account_contract_never_accepts_credential_material(self) -> None:
        good = Account.model_validate({
            "id": "default", "home": "/profiles/default", "label": "Codex",
            "source": "Codex CLI", "status": "ready", "accountId": "acct-1",
        })
        self.assertEqual(good.accountId, "acct-1")
        with self.assertRaises(ValidationError):
            Account.model_validate({
                "id": "default", "home": "/profiles/default", "label": "Codex",
                "source": "Codex CLI", "status": "ready", "accessToken": "secret",
            })

    def test_account_snapshot_has_explicit_collections(self) -> None:
        result = AccountsResponse.model_validate({
            "accounts": [], "archivedAccounts": [], "defaultAccountKey": "default",
            "logins": [], "supportsDisconnect": True, "supportsDelete": True,
        })
        self.assertEqual(result.defaultAccountKey, "default")
        with self.assertRaises(ValidationError):
            AccountsResponse.model_validate({"accounts": [], "defaultAccountKey": "default", "token": "secret"})

    def test_project_updates_keep_explicit_nulls(self) -> None:
        request = ProjectWriteRequest.model_validate({
            "action": "set_worker_base", "path": "/project", "base_ref": None,
            "expected_revision": 0,
        })
        self.assertIn("base_ref", request.model_dump(mode="json", exclude_unset=True))
        with self.assertRaises(ValidationError):
            ProjectWriteRequest.model_validate({"action": "unknown", "path": "/project"})

    def test_reset_requires_durable_uuid_before_dispatch(self) -> None:
        request = ResetRequest.model_validate({
            "account_id": "account-1", "credit_id": "credit-1", "request_id": str(uuid4()),
        })
        self.assertEqual(request.request_id, str(request.request_id))
        with self.assertRaises(ValidationError):
            ResetRequest.model_validate({
                "account_id": "account-1", "credit_id": "credit-1", "request_id": "bad-id",
            })

    def test_provider_catalog_names_are_open_strings(self) -> None:
        catalog = ModelCatalogResponse.model_validate({"data": [{"model": "vendor/new-model", "tier": "preview"}]})
        self.assertIsNotNone(catalog.data)
        self.assertEqual(catalog.data[0].model, "vendor/new-model")

    def test_usage_payload_is_explicitly_provider_owned(self) -> None:
        limits = UsageLimitsResponse.model_validate({
            "accountKey": "default", "data": {"accountId": "acct-1", "newField": [True, 1]},
            "at": 10.5, "error": None,
        })
        self.assertIsInstance(limits.data, dict)


if __name__ == "__main__":
    unittest.main()
