#!/usr/bin/env python3
"""Real Python HTTP null-decision refusal, paired with Rust's attached TCP test."""
from __future__ import annotations
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()
import importlib.util
import json
import os
from pathlib import Path
import sqlite3
import unittest
from http_backend_harness import TimedHTTPContractTest, no_write_snapshot, start_contract_backend

spec = importlib.util.spec_from_file_location("questions_contract", Path(__file__).with_name("rust-questions-write-http-contract.py"))
assert spec is not None and spec.loader is not None
contract = importlib.util.module_from_spec(spec)
spec.loader.exec_module(contract)

class NativeNullHTTP(TimedHTTPContractTest, unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        if os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"):
            raise RuntimeError("Run this Python contract without STUDIO_HTTP_BACKEND_COMMAND; the Rust attached backend case is wave7_p04_callback_http")
        cls.backend = start_contract_backend(cls)
        cls.backend.stop()
        contract.seed_records(cls.backend)
        cls.backend.start()
        with sqlite3.connect(cls.backend.state / "canvas.sqlite3") as db:
            row = json.loads(db.execute("SELECT record FROM runtime_requests WHERE id='questions-write-native'").fetchone()[0])
            for index, method in enumerate(["item/commandExecution/requestApproval", "item/fileChange/requestApproval", "mcpServer/elicitation/request", "item/tool/requestUserInput"]):
                card = {**row, "id": f"p04-null-{index}", "method": method, "status": "pending"}
                db.execute("INSERT INTO runtime_requests(id,record) VALUES(?,?)", (card["id"], json.dumps(card)))

    @classmethod
    def tearDownClass(cls):
        cls.backend.stop()

    def test_explicit_null_refuses_without_any_write(self):
        for index in range(4):
            with self.subTest(method=index):
                before = no_write_snapshot(self.backend.state)
                response = self.backend.request("POST", "/api/answer", body={"id": f"p04-null-{index}", "decision": None, "answers": None}, headers={"Origin": self.backend.origin, "X-Canvas-Token": self.backend.session["token"], "X-Studio-API-Schema": self.backend.schema_hash})
                self.assertEqual(response.status, 400, response.body)
                self.assertEqual(response.json()["error"], "Supply question answers" if index == 3 else "Choose accept, decline or cancel")
                self.assertEqual(no_write_snapshot(self.backend.state), before)

if __name__ == "__main__":
    unittest.main()
