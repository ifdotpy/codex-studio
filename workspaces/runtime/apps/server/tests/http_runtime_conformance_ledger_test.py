"""Regression test for transparent HTTPError tracing in the conformance ledger."""
from __future__ import annotations

import importlib.util
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[5]
DIFFERENTIAL = ROOT / "workspaces/runtime/apps/server/tests/http_runtime_extractor_differential.py"
diff_spec = importlib.util.spec_from_file_location("http_runtime_extractor_differential", DIFFERENTIAL)
assert diff_spec and diff_spec.loader
differential = importlib.util.module_from_spec(diff_spec)
sys.modules[diff_spec.name] = differential
diff_spec.loader.exec_module(differential)

LEDGER = ROOT / "workspaces/runtime/apps/server/tests/http_runtime_conformance_ledger.py"
spec = importlib.util.spec_from_file_location("http_runtime_conformance_ledger", LEDGER)
assert spec and spec.loader
ledger = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = ledger
spec.loader.exec_module(ledger)


class LedgerTraceContract(unittest.TestCase):
    def test_split_suite_counts_include_every_unittest_summary(self) -> None:
        with tempfile.TemporaryDirectory() as scratch:
            root = Path(scratch)
            log = root / "full.log"
            content = (
                "test_first (__main__.First.test_first) ... ok\n"
                "Ran 1 test in 0.1s\nOK\n"
                "test_second (__main__.Second.test_second) ... skipped 'fixture'\n"
                "Ran 1 test in 0.2s\nOK (skipped=1)\n"
                "Server suites: 1 passed, 0 failed, 0 skipped (opt-in), 0 skipped (environment)\n"
            )
            log.write_text(content)
            process = SimpleNamespace(returncode=0, stderr="", stdout=json.dumps({
                "logPath": str(log), "status": "passed", "passed": 2,
                "failed": 0, "skipped": 1, "exitCode": 0,
            }))
            suite = ROOT / "workspaces/runtime/apps/server/tests/test_isolation_contract.py"
            with patch.object(ledger.subprocess, "run", return_value=process):
                result = ledger.run_suite(suite, {}, root, [], {})
                self.assertEqual(len(result["tests"]), 2)
                log.write_text(content.replace("Ran 1 test in 0.1s", "Ran 2 tests in 0.1s"))
                with self.assertRaisesRegex(RuntimeError, "result-count mismatch"):
                    ledger.run_suite(suite, {}, root, [], {})

    def test_probe_cases_are_operation_keyed_and_reference_seeded_records(self) -> None:
        documented = set(ledger.operations())
        self.assertLessEqual(set(ledger.PROBE_CASES), documented)
        fixture_keys = {
            "agent", "imageAgent", "task", "complaint", "room", "checkpoint", "item",
            "monitor", "terminal", "activeRule", "chat", "message", "filePath",
            "layoutRevision", "emptyQueueRevision", "queueIds", "question", "toolReceipt", "toolCancel",
            "questionDelete", "providerQuestion", "projectPath", "claudeAccount",
            "voiceRecord", "voiceEndedSession", "voiceActiveSession",
            "analyticsAgent", "schemaHash", "approvalRequest", "codexLogin",
        }

        def references(value):
            if isinstance(value, str) and value.startswith("$"):
                return {value[1:]}
            if isinstance(value, dict):
                return set().union(*(references(item) for item in value.values()))
            if isinstance(value, list):
                return set().union(*(references(item) for item in value))
            return set()

        for operation, spec in ledger.PROBE_CASES.items():
            cases = [spec["primary"], *spec.get("additional", [])]
            for probe_case in cases:
                with self.subTest(operation=operation, case=probe_case.get("name", "primary")):
                    self.assertIn(probe_case.get("expected", "2xx"), {"2xx", "400", "404", "501"})
                    self.assertLessEqual(references(probe_case), fixture_keys)
                    if probe_case.get("expected") == "404":
                        self.assertTrue(probe_case.get("reason"))

    def test_reset_credit_probes_are_named_refusals_without_redemption(self) -> None:
        reset = ledger.PROBE_CASES[("POST", "/api/limits/reset")]
        self.assertEqual(set(reset), {"primary"})
        self.assertEqual(reset["primary"]["expected"], "501")
        self.assertTrue(reset["primary"]["assert_no_write"])
        self.assertEqual(
            reset["primary"]["reason"],
            "redemption is never performed or simulated in tests",
        )

    def test_new_partial_probes_have_success_and_named_refusal_shapes(self) -> None:
        for operation in (
            ("GET", "/api/changes"),
            ("GET", "/api/models"),
            ("POST", "/api/federation"),
            ("POST", "/api/messages"),
            ("POST", "/api/multi-server"),
            ("POST", "/api/peer-teams"),
            ("POST", "/api/projects"),
            ("POST", "/api/questions/delete"),
        ):
            with self.subTest(operation=operation):
                spec = ledger.PROBE_CASES[operation]
                self.assertEqual(spec["primary"]["expected"], "2xx")
                refusals = spec.get("additional", [])
                self.assertTrue(refusals)
                self.assertTrue(all(case["expected"] == "501" for case in refusals))
                self.assertTrue(all(case.get("expected_error") for case in refusals))

    def test_changes_and_models_probes_use_slice_contract_request_shapes(self) -> None:
        identifiers = {"agent": "lead-agent", "claudeAccount": "claude-account"}
        changes = ledger.PROBE_CASES[("GET", "/api/changes")]["additional"][0]
        changes_path, _, _ = ledger.valid_probe(
            "GET", "/api/changes", identifiers, changes,
        )
        self.assertEqual(changes_path, "/api/changes?agent=lead-agent")
        self.assertEqual(changes["fixture_mutations"]["agent_execution_mode"], "vm")
        self.assertIn("agent_record", changes["restore"])

        models = ledger.PROBE_CASES[("GET", "/api/models")]["additional"][0]
        models_path, _, _ = ledger.valid_probe("GET", "/api/models", identifiers, models)
        self.assertEqual(models_path, "/api/models?account_key=claude-account")

    def test_probe_fixture_restore_runs_when_the_probe_raises(self) -> None:
        with tempfile.TemporaryDirectory(prefix="ledger-probe-restore-") as scratch:
            state = Path(scratch)
            with sqlite3.connect(state / "canvas.sqlite3") as connection:
                connection.execute("CREATE TABLE runtime_agents(id TEXT PRIMARY KEY, record TEXT NOT NULL)")
                connection.execute(
                    "INSERT INTO runtime_agents VALUES(?, ?)",
                    ("lead-agent", json.dumps({"id": "lead-agent", "executionMode": "host"})),
                )
            identity_path = state / "multi-server" / "identity.json"
            identity_path.parent.mkdir()
            identity_path.write_bytes(b'{"serverId":"local"}')
            identity_path.chmod(0o600)
            case = {
                "fixture_mutations": {
                    "agent_execution_mode": "vm", "remove_identity": True,
                },
                "restore": ["agent_record", "identity_file"],
            }

            with self.assertRaisesRegex(RuntimeError, "probe failed"):
                with ledger.probe_fixture_state(
                    SimpleNamespace(state=state), {"agent": "lead-agent"}, case,
                ):
                    self.assertFalse(identity_path.exists())
                    with sqlite3.connect(state / "canvas.sqlite3") as connection:
                        changed = json.loads(connection.execute(
                            "SELECT record FROM runtime_agents WHERE id='lead-agent'",
                        ).fetchone()[0])
                    self.assertEqual(changed["executionMode"], "vm")
                    raise RuntimeError("probe failed")

            self.assertEqual(identity_path.read_bytes(), b'{"serverId":"local"}')
            self.assertEqual(identity_path.stat().st_mode & 0o777, 0o600)
            with sqlite3.connect(state / "canvas.sqlite3") as connection:
                restored = json.loads(connection.execute(
                    "SELECT record FROM runtime_agents WHERE id='lead-agent'",
                ).fetchone()[0])
            self.assertEqual(restored["executionMode"], "host")

    def test_probe_inputs_match_exact_panel_and_live_queue_contracts(self) -> None:
        panel = ledger.PROBE_CASES[("POST", "/api/panel/layout")]["primary"]["body"]
        self.assertEqual(
            set(panel),
            {
                "agent", "client", "sequence", "renderer", "revision", "width",
                "height", "contentWidth", "contentHeight", "overflowX", "overflowY",
                "totalLines", "visibleLines", "lastVisibleLine", "lastVisibleHeading",
                "fits", "reason",
            },
        )
        queue = ledger.PROBE_CASES[("POST", "/api/queue")]["primary"]["body"]
        self.assertTrue(ledger.PROBE_CASES[("POST", "/api/queue")]["primary"]["refresh_queue"])
        self.assertEqual(queue["expected_revision"], "$emptyQueueRevision")
        self.assertEqual(queue["ordered_ids"], "$queueIds")

    def test_partial_operations_with_multiple_request_shapes_check_both_results(self) -> None:
        expected_cases = {
            ("GET", "/api/file"): ["2xx", "501"],
            ("GET", "/api/file-info"): ["2xx", "501"],
            ("GET", "/api/sync/pull"): ["2xx", "2xx"],
            ("POST", "/api/sync/drafts"): ["2xx", "400"],
            ("POST", "/api/rules"): ["2xx", "501"],
            ("POST", "/api/agents"): ["2xx", "501"],
            ("POST", "/api/leads"): ["2xx", "501"],
            ("POST", "/api/configure"): ["2xx", "501"],
            ("POST", "/api/conversation"): ["2xx", "501"],
            ("POST", "/api/checkpoint"): ["501", "2xx"],
            ("POST", "/api/checkpoint/preview"): ["2xx"],
            ("POST", "/api/voice/end"): ["2xx", "501"],
            ("POST", "/api/voice/speech"): ["2xx", "501"],
            ("POST", "/api/voice/submit"): ["2xx", "400"],
        }
        for operation, expected in expected_cases.items():
            with self.subTest(operation=operation):
                spec = ledger.PROBE_CASES[operation]
                self.assertEqual(
                    [spec["primary"]["expected"], *[case["expected"] for case in spec.get("additional", [])]],
                    expected,
                )

    def test_classifier_rejects_server_errors_before_allowances(self) -> None:
        case = {"path": "/x", "method": "GET", "label": "known"}
        document = {"paths": {"/x": {"get": {"operationId": "op"}}}}
        with self.assertRaisesRegex(AssertionError, "returned 500"):
            differential.classify(
                [case],
                [{"status": 500, "accepted": False}],
                [{"status": 200, "accepted": True}],
                document,
            )

    def test_classifier_requires_located_envelopes_for_validation_refusals(self) -> None:
        case = {"path": "/x", "method": "POST", "label": "body:type:str"}
        document = {"paths": {"/x": {"post": {"operationId": "op"}}}}
        refusal = {"status": 400, "accepted": False, "body": {"error": "Invalid request"}}
        with self.assertRaisesRegex(AssertionError, "Rust refusal lacks its located error envelope"):
            differential.classify([case], [refusal], [refusal], document)


    def test_classifier_does_not_accept_wildcard_allowances(self) -> None:
        old = differential._EXPECTED_STATUS_DIFFERENCES.copy()
        try:
            differential._EXPECTED_STATUS_DIFFERENCES.clear()
            differential._EXPECTED_STATUS_DIFFERENCES[("*", "query:extra")] = (
                400, 200, "must never be wildcarded"
            )
            case = {"path": "/x", "method": "GET", "label": "query:extra"}
            document = {"paths": {"/x": {"get": {"operationId": "op"}}}}
            with self.assertRaisesRegex(AssertionError, "invalid expected-difference entry"):
                differential.classify(
                    [case],
                    [{"status": 400, "accepted": False, "body": {"error": "bad", "location": ["query"]}}],
                    [{"status": 200, "accepted": True, "value": {}}],
                    document,
                )
        finally:
            differential._EXPECTED_STATUS_DIFFERENCES.clear()
            differential._EXPECTED_STATUS_DIFFERENCES.update(old)

    def test_classifier_rejects_unused_allowance(self) -> None:
        old = differential._EXPECTED_STATUS_DIFFERENCES.copy()
        try:
            differential._EXPECTED_STATUS_DIFFERENCES.clear()
            differential._EXPECTED_STATUS_DIFFERENCES[("op", "unused")] = (
                400, 200, "expired"
            )
            case = {"path": "/x", "method": "GET", "label": "baseline"}
            document = {"paths": {"/x": {"get": {"operationId": "op"}}}}
            with self.assertRaisesRegex(AssertionError, "entries were unused"):
                differential.classify(
                    [case],
                    [{"status": 200, "accepted": True, "value": {}}],
                    [{"status": 200, "accepted": True, "value": {}}],
                    document,
                )
        finally:
            differential._EXPECTED_STATUS_DIFFERENCES.clear()
            differential._EXPECTED_STATUS_DIFFERENCES.update(old)

    def test_classifier_rejects_allowances_without_both_statuses_and_a_cause(self) -> None:
        old = differential._EXPECTED_STATUS_DIFFERENCES.copy()
        try:
            differential._EXPECTED_STATUS_DIFFERENCES.clear()
            differential._EXPECTED_STATUS_DIFFERENCES[("op", "case")] = (400, 200)
            with self.assertRaisesRegex(AssertionError, "invalid expected-difference entry"):
                differential.classify([], [], [], {"paths": {}})
        finally:
            differential._EXPECTED_STATUS_DIFFERENCES.clear()
            differential._EXPECTED_STATUS_DIFFERENCES.update(old)

    def test_classifier_uses_strict_json_types(self) -> None:
        self.assertFalse(differential.strict_equal(True, 1))

    def test_echo_comparison_uses_declared_number_semantics_only(self) -> None:
        operation = {
            "operationId": "numeric_echo",
            "parameters": [
                {"in": "query", "name": "amount", "schema": {"type": "number"}},
                {"in": "query", "name": "count", "schema": {"type": "integer"}},
            ],
        }
        document = {"paths": {"/x": {"get": operation}}}
        rust = {"value": {"query": {"amount": 1, "count": 1}, "headers": {}}}
        python = {"value": {"query": {"amount": 1.0, "count": 1}, "headers": {}}}
        self.assertTrue(differential.observation_values_equal(rust, python, operation, document))
        python["value"]["query"]["amount"] = True
        self.assertFalse(differential.observation_values_equal(rust, python, operation, document))
        python["value"]["query"]["amount"] = 1.0
        python["value"]["query"]["count"] = 1.5
        self.assertFalse(differential.observation_values_equal(rust, python, operation, document))









    def test_opaque_json_precision_is_a_located_rust_refusal(self) -> None:
        case = {"path": "/widgets", "method": "POST", "label": "payload:lax:18446744073709551616"}
        operation = {
            "operationId": "create_widget",
            "requestBody": {"content": {"application/json": {"schema": {"$ref": "#/components/schemas/JsonValue"}}}},
        }
        document = {
            "paths": {"/widgets": {"post": operation}},
            "components": {"schemas": {"JsonValue": {"anyOf": [{"type": "null"}, {"type": "integer"}, {"type": "object"}]}}},
        }
        rust = {"status": 400, "accepted": False, "body": {"error": "Invalid request", "location": ["body", "payload"]}}
        python = {"status": 200, "accepted": True, "value": {"body": {"payload": 2**64, "host": "same"}, "query": {}, "headers": {}}}
        old = differential._EXPECTED_STATUS_DIFFERENCES.copy()
        try:
            differential._EXPECTED_STATUS_DIFFERENCES.clear()
            result = differential.classify([case], [rust], [python], document)
            self.assertEqual(result["mismatches_by_cause"], {differential.OPAQUE_JSON_INTEGER_CAUSE: 1})
            self.assertEqual(result["mismatch_direction"], {"rust_reject_python_accept": 1})
            self.assertEqual(result["expected_difference_cases"][0]["pointer"], "#/components/schemas/JsonValue")
        finally:
            differential._EXPECTED_STATUS_DIFFERENCES.clear()
            differential._EXPECTED_STATUS_DIFFERENCES.update(old)

    def test_opaque_precision_classifier_resolves_additional_properties_inside_unions(self) -> None:
        case = {"path": "/answers", "method": "POST", "label": "answers/key:lax:18446744073709551616"}
        operation = {
            "operationId": "answer_api_answer_post",
            "requestBody": {"content": {"application/json": {"schema": {"$ref": "#/components/schemas/AnswerBody"}}}},
        }
        document = {
            "paths": {"/answers": {"post": operation}},
            "components": {"schemas": {
                "JsonValue": {"anyOf": [{"type": "null"}, {"type": "number"}, {"type": "object"}]},
                "AnswerBody": {"type": "object", "properties": {
                    "answers": {"anyOf": [
                        {"type": "object", "additionalProperties": {"$ref": "#/components/schemas/JsonValue"}},
                        {"type": "null"},
                    ]},
                }},
            }},
        }
        rust = {"status": 400, "accepted": False, "body": {"error": "Invalid request", "location": ["body", "answers", "key"]}}
        python = {"status": 200, "accepted": True, "value": {"body": {"answers": {"key": 2**64}}, "query": {}, "headers": {}}}
        old = differential._EXPECTED_STATUS_DIFFERENCES.copy()
        try:
            differential._EXPECTED_STATUS_DIFFERENCES.clear()
            result = differential.classify([case], [rust], [python], document)
            self.assertEqual(result["mismatches_by_cause"], {differential.OPAQUE_JSON_INTEGER_CAUSE: 1})
            self.assertEqual(result["expected_difference_cases"][0]["pointer"], "#/components/schemas/JsonValue")
        finally:
            differential._EXPECTED_STATUS_DIFFERENCES.clear()
            differential._EXPECTED_STATUS_DIFFERENCES.update(old)

    def test_opaque_precision_allowance_does_not_apply_to_typed_integer(self) -> None:
        case = {"path": "/widgets", "method": "POST", "label": "count:lax:18446744073709551617"}
        operation = {"operationId": "create_widget", "requestBody": {"content": {"application/json": {"schema": {"$ref": "#/components/schemas/Widget"}}}}}
        document = {"paths": {"/widgets": {"post": operation}}, "components": {"schemas": {"Widget": {"type": "object", "properties": {"count": {"type": "integer"}}}}}}
        rust = {"status": 200, "accepted": True, "value": {"body": {"count": float(2**64)}, "query": {}, "headers": {}}}
        python = {"status": 200, "accepted": True, "value": {"body": {"count": 2**64 + 1}, "query": {}, "headers": {}}}
        old = differential._EXPECTED_STATUS_DIFFERENCES.copy()
        try:
            differential._EXPECTED_STATUS_DIFFERENCES.clear()
            with self.assertRaisesRegex(AssertionError, "unclassified typed-value mismatch"):
                differential.classify([case], [rust], [python], document)
        finally:
            differential._EXPECTED_STATUS_DIFFERENCES.clear()
            differential._EXPECTED_STATUS_DIFFERENCES.update(old)

    def test_consumer_counts_all_runner_slices(self) -> None:
        from unittest.mock import patch
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            log = root / "consumer.log"
            log.write_text(
                "test_one (__main__.Example.test_one) ... ok\n"
                "Ran 1 test in 0.1s\n"
                "test_two (__main__.Example.test_two) ... ok\n"
                "Ran 1 test in 0.1s\n"
                "Server suites: 1 passed, 0 failed, 0 skipped (opt-in), 0 skipped (environment)\n"
            )
            result = {"status":"passed", "passed":3, "failed":0, "skipped":0, "logPath":str(log)}
            process = SimpleNamespace(stdout=json.dumps(result), stderr="", returncode=0)
            with patch.object(ledger.subprocess, "run", return_value=process) as run:
                observed = ledger.run_suite(ledger.TESTS / "fixture.py", {}, root, [], {})
            self.assertEqual(run.call_args.kwargs["env"]["CODEX_SERVER_TEST_JOBS"], "1")
            self.assertEqual(observed["passed"], 3)
            self.assertEqual(len(observed["tests"]), 2)

    def test_unittest_ids_keep_their_class_and_are_not_duplicated(self) -> None:
        cases = ledger.case_results(
            "test_example (__main__.ExampleContract.test_example) ... ok"
        )
        self.assertEqual(
            cases,
            [{
                "test": "test_example",
                "testId": "__main__.ExampleContract.test_example",
                "status": "ok",
            }],
        )

    def test_observation_classification_requires_a_valid_probe_result(self) -> None:
        classify = ledger.classify_observation
        valid = (True, True, True)
        self.assertEqual(classify(501, "unimplemented", *valid), "not_implemented")
        self.assertEqual(classify(200, "unimplemented", *valid), "unexpected")
        self.assertEqual(classify(400, "implemented", *valid), "unverified")
        self.assertEqual(classify(302, "implemented", *valid), "unverified")
        self.assertEqual(classify(500, "implemented", *valid), "unexpected")
        self.assertEqual(classify(501, "partial", *valid), "partial")
        self.assertEqual(classify(403, "partial", *valid), "unverified")
        self.assertEqual(classify(502, "partial", *valid), "unexpected")
        self.assertEqual(classify(200, "implemented", False, True, True), "unexpected")

    def test_traced_error_body_remains_readable_by_the_suite(self) -> None:
        with tempfile.TemporaryDirectory() as scratch:
            hook = ledger.install_request_trace(Path(scratch))
            script = r'''
import io, sitecustomize, urllib.request
from urllib.error import HTTPError
payload = b'{"error":"Invalid request"}'
def failing(_request, *args, **kwargs):
    raise HTTPError("http://127.0.0.1/", 400, "bad", {}, io.BytesIO(payload))
sitecustomize._original_urlopen = failing
try:
    urllib.request.urlopen("http://127.0.0.1/")
except HTTPError as error:
    assert error.read() == payload, error.read()
else:
    raise AssertionError("expected HTTPError")
'''
            environment = os.environ.copy()
            environment["PYTHONPATH"] = str(hook.parent)
            subprocess.run([sys.executable, "-c", script], env=environment, check=True)


if __name__ == "__main__":
    unittest.main()
