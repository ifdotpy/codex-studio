#!/usr/bin/env python3
"""Reset-credit HTTP contract on either backend using only provider replay."""

from __future__ import annotations

import os
from pathlib import Path
import sys
import unittest
import uuid

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from test_isolation import isolate_supervisor_environment

isolate_supervisor_environment()

from http_backend_harness import TimedHTTPContractTest, start_contract_backend


class ResetCreditHTTPContract(TimedHTTPContractTest, unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        if not os.environ.get("TMPDIR"):
            raise RuntimeError("The server runner must provide a memory-backed TMPDIR")
        cls.backend = start_contract_backend(cls, "reset-credit-redemption.json")

    @classmethod
    def headers(cls) -> dict[str, str]:
        return {
            "Origin": cls.backend.origin or "",
            "X-Canvas-Token": cls.backend.session["token"],
            "X-Studio-API-Schema": cls.backend.schema_hash,
        }

    def test_exact_request_replay_uses_one_fake_provider_redemption(self) -> None:
        request_id = str(uuid.uuid4())
        body = {
            "account_key": "default",
            "account_id": "fixture-account",
            "credit_id": "fixture-credit",
            "request_id": request_id,
        }
        first = self.backend.request("POST", "/api/limits/reset", body=body, headers=self.headers())
        self.assertEqual(first.status, 200, first.body[:1000])
        self.assertEqual(first.json()["outcome"], "reset")
        self.assertEqual(first.json()["request_id"], request_id)
        self.assertEqual(first.json()["credit_id"], "fixture-credit")
        self.assertEqual(first.json()["account_id"], "fixture-account")

        replay = self.backend.request("POST", "/api/limits/reset", body=body, headers=self.headers())
        self.assertEqual(replay.status, 200, replay.body[:1000])
        replay_json = replay.json()
        first_json = first.json()
        for field in ("outcome", "request_id", "credit_id", "account_id", "account_key"):
            self.assertEqual(replay_json[field], first_json[field])
        self.assertEqual(replay_json["limits"]["data"]["accountId"], "fixture-account")
        consume = [
            frame for frame in self.backend.received_provider_frames()
            if frame.get("method") == "account/rateLimitResetCredit/consume"
        ]
        self.assertEqual(len(consume), 1)
        self.assertEqual(consume[0]["params"]["creditId"], "fixture-credit")
        uuid.UUID(consume[0]["params"]["idempotencyKey"])


if __name__ == "__main__":
    unittest.main()
