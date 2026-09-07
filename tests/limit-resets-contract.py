#!/usr/bin/env python3
"""Exact reset credits, account identity and uncertain RPC retries. No live credit use."""

import copy
from concurrent.futures import ThreadPoolExecutor
import importlib.util
import json
from pathlib import Path
import unittest
from unittest.mock import patch
import threading
import urllib.error
import urllib.request
import uuid

spec = importlib.util.spec_from_file_location(
    "workspace_fixture", Path(__file__).with_name("workspace-contract.py")
)
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)
from codex_limit_resets import consume_reset


def credit(key="credit-A", **changes):
    return {
        "id": key,
        "status": "available",
        "resetType": "codexRateLimits",
        "grantedAt": 1,
        **changes,
    }


class ResetServer(f.WorkspaceServer):
    def __init__(self, *args):
        super().__init__(*args)
        self.snapshot = {
            "accountId": "account-A",
            "rateLimits": {"primary": {"usedPercent": 100}},
            "rateLimitResetCredits": {
                "availableCount": 2,
                "credits": [credit(), credit("credit-B")],
            },
        }
        self.outcomes = []
        self.consume_calls = []
        self.read_count = 0
        self.fail_read_at = None

    def call(self, method, params, timeout=60):
        if method == "account/rateLimits/read":
            self.read_count += 1
            if self.fail_read_at == self.read_count:
                raise RuntimeError("Fixture usage unavailable")
            return copy.deepcopy(self.snapshot)
        if method == "account/rateLimitResetCredit/consume":
            self.consume_calls.append(copy.deepcopy(params))
            outcome = self.outcomes.pop(0) if self.outcomes else "reset"
            # A timeout can occur after the backend spent the credit.
            if outcome in {"reset", "alreadyRedeemed"} or isinstance(
                outcome, Exception
            ):
                for row in self.snapshot["rateLimitResetCredits"]["credits"]:
                    if row["id"] == params["creditId"]:
                        row["status"] = "redeemed"
            if isinstance(outcome, Exception):
                raise outcome
            return {"outcome": outcome}
        return super().call(method, params, timeout)


class ResetContracts(unittest.TestCase):
    def setUp(self):
        f.WorkspaceContract.setUp(self)
        self.runtime.close()
        self.runtime = f.ControlledRuntime(self.state, ResetServer)
        self.server = self.runtime.connect()

    tearDown = f.WorkspaceContract.tearDown

    def reset(self, **data):
        return consume_reset(
            self.runtime, {"credit_id": "credit-A", "account_id": "account-A", **data}
        )

    def test_source_update_keeps_older_runtime_reset_compatible(self):
        with patch.object(self.runtime, "limit_refresh_lock", None):
            self.assertEqual(self.reset()["outcome"], "reset")
        self.assertEqual(len(self.server.consume_calls), 1)

    def test_exact_credit_and_completed_retry_do_not_spend_another_credit(self):
        request = str(uuid.uuid4())
        first = self.reset(request_id=request)
        second = self.reset(request_id=request)
        third = self.reset(request_id=str(uuid.uuid4()))
        self.assertEqual(
            [first["outcome"], second["outcome"], third["outcome"]], ["reset"] * 3
        )
        self.assertEqual(len(self.server.consume_calls), 1)
        self.assertEqual(self.server.consume_calls[0]["creditId"], "credit-A")
        uuid.UUID(self.server.consume_calls[0]["idempotencyKey"])
        self.assertEqual(
            first["limits"]["data"]["rateLimitResetCredits"]["credits"][1]["status"],
            "available",
        )

    def test_stale_account_missing_identity_and_unknown_credit_never_consume(self):
        for data in (
            {"account_id": "account-B"},
            {"account_id": None},
            {"credit_id": "absent"},
            {"credit_id": None},
        ):
            with self.assertRaises(ValueError):
                self.reset(**data)
        self.assertEqual(self.server.consume_calls, [])

    def test_count_without_details_expired_redeeming_and_unknown_type_are_not_selected(
        self,
    ):
        for rows in (
            None,
            [],
            [credit(status="redeeming")],
            [credit(expiresAt=1)],
            [credit(resetType="unknown")],
        ):
            self.server.snapshot["rateLimitResetCredits"]["credits"] = rows
            with self.assertRaises(ValueError):
                self.reset()
        self.assertEqual(self.server.consume_calls, [])

    def test_outcomes_remain_distinct_and_nonspending_attempt_can_be_retried_later(
        self,
    ):
        for outcome in ("nothingToReset", "noCredit", "alreadyRedeemed"):
            self.server.outcomes = [outcome]
            request = str(uuid.uuid4())
            result = self.reset(request_id=request)
            self.assertEqual(result["outcome"], outcome)
            count = len(self.server.consume_calls)
            self.assertEqual(self.reset(request_id=request)["outcome"], outcome)
            self.assertEqual(len(self.server.consume_calls), count)
        self.assertEqual(len(self.server.consume_calls), 3)

    def test_uncertain_retry_uses_same_key_after_restart_even_if_credit_is_redeemed(
        self,
    ):
        self.server.outcomes = [RuntimeError("Fixture timeout after commit")]
        first = self.reset()
        self.assertEqual(first["outcome"], "uncertain")
        original = self.server.consume_calls[0]
        snapshot = copy.deepcopy(self.server.snapshot)
        self.runtime.close()
        self.runtime = f.ControlledRuntime(self.state, ResetServer)
        self.server = self.runtime.connect()
        self.server.snapshot = snapshot
        self.server.outcomes = ["alreadyRedeemed"]
        second = self.reset(request_id=first["request_id"])
        self.assertEqual(second["outcome"], "alreadyRedeemed")
        self.assertEqual(self.server.consume_calls, [original])

    def test_pending_crash_receipt_reuses_key_and_prevents_account_switch(self):
        self.server.outcomes = [RuntimeError("Fixture lost response")]
        first = self.reset()
        original = self.server.consume_calls[0]
        with self.runtime.db() as db:
            row = json.loads(
                db.execute(
                    "SELECT record FROM runtime_limit_reset_attempts WHERE id=?",
                    (first["request_id"],),
                ).fetchone()[0]
            )
            row.update(status="pending")
            db.execute(
                "UPDATE runtime_limit_reset_attempts SET record=? WHERE id=?",
                (json.dumps(row), row["id"]),
            )
        self.server.snapshot["accountId"] = "account-B"
        with self.assertRaises(ValueError):
            self.reset(request_id=first["request_id"])
        with self.assertRaises(ValueError):
            self.reset(request_id=first["request_id"], account_id="account-B")
        self.assertEqual(len(self.server.consume_calls), 1)
        self.server.snapshot["accountId"] = "account-A"
        self.server.outcomes = ["alreadyRedeemed"]
        self.reset()
        self.assertEqual(self.server.consume_calls[-1], original)

    def test_concurrent_double_press_calls_consume_once(self):
        with ThreadPoolExecutor(max_workers=8) as pool:
            results = list(pool.map(lambda _: self.reset(), range(8)))
        self.assertEqual(len(self.server.consume_calls), 1)
        self.assertEqual({r["request_id"] for r in results}, {results[0]["request_id"]})

    def test_refresh_failure_preserves_confirmed_outcome(self):
        self.server.fail_read_at = 2
        result = self.reset()
        self.assertEqual(result["outcome"], "reset")
        self.assertIn("unavailable", result["limits"]["error"])
        self.assertEqual(
            self.reset(request_id=result["request_id"])["outcome"], "reset"
        )
        self.assertEqual(len(self.server.consume_calls), 1)

    def test_fresh_read_failure_never_consumes_and_ignores_cached_available_credit(
        self,
    ):
        self.runtime.rate_limits = {
            "data": copy.deepcopy(self.server.snapshot),
            "at": 1,
            "error": None,
        }
        self.server.fail_read_at = 1
        with self.assertRaises(RuntimeError):
            self.reset()
        self.assertEqual(self.server.consume_calls, [])

    def test_request_id_cannot_be_rebound_to_another_credit(self):
        result = self.reset()
        with self.assertRaises(ValueError):
            self.reset(request_id=result["request_id"], credit_id="credit-B")
        self.assertEqual(len(self.server.consume_calls), 1)

    def test_http_reset_requires_local_token_and_fresh_account(self):
        from codex_canvas import Canvas, make_server

        canvas = Canvas(self.state)
        canvas.runtime = self.runtime
        http = make_server(canvas)
        thread = threading.Thread(target=http.serve_forever, daemon=True)
        thread.start()
        base = f"http://127.0.0.1:{http.server_port}"

        def request(path, body=None, headers=None):
            req = urllib.request.Request(
                base + path,
                data=json.dumps(body).encode() if body is not None else None,
                headers=headers or {},
            )
            with urllib.request.urlopen(req, timeout=5) as response:
                return json.load(response)

        body = {
            "account_id": "account-A",
            "credit_id": "credit-A",
            "request_id": str(uuid.uuid4()),
        }
        try:
            with self.assertRaises(urllib.error.HTTPError) as denied:
                request("/api/limits/reset", body)
            self.assertEqual(denied.exception.code, 403)
            headers = {
                "X-Canvas-Token": request("/api/state")["token"],
                "Content-Type": "application/json",
            }
            with self.assertRaises(urllib.error.HTTPError) as cross:
                request(
                    "/api/limits/reset",
                    body,
                    {**headers, "Origin": "https://outside.invalid"},
                )
            self.assertEqual(cross.exception.code, 403)
            with self.assertRaises(urllib.error.HTTPError) as changed:
                request(
                    "/api/limits/reset", {**body, "account_id": "account-B"}, headers
                )
            self.assertEqual(changed.exception.code, 400)
            self.assertEqual(self.server.consume_calls, [])
            result = request("/api/limits/reset", body, headers)
            self.assertEqual(result["outcome"], "reset")
            self.assertEqual(
                request("/api/limits/reset", body, headers)["outcome"], "reset"
            )
            self.assertEqual(len(self.server.consume_calls), 1)
        finally:
            http.shutdown()
            http.server_close()
            thread.join()


if __name__ == "__main__":
    unittest.main()
