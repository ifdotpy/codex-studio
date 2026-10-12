"""Regression test for transparent HTTPError tracing in the conformance ledger."""
from __future__ import annotations

import importlib.util
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

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









    def test_opaque_json_precision_normalizes_only_the_named_leaf(self) -> None:
        case = {"path": "/widgets", "method": "POST", "label": "payload:lax:18446744073709551616"}
        operation = {
            "operationId": "create_widget",
            "requestBody": {"content": {"application/json": {"schema": {"$ref": "#/components/schemas/JsonValue"}}}},
        }
        document = {
            "paths": {"/widgets": {"post": operation}},
            "components": {"schemas": {"JsonValue": {"anyOf": [{"type": "null"}, {"type": "integer"}, {"type": "object"}]}}},
        }
        rust = {"status": 200, "accepted": True, "value": {"body": {"payload": float(2**64), "host": "same"}, "query": {}, "headers": {}}}
        python = {"status": 200, "accepted": True, "value": {"body": {"payload": 2**64, "host": "same"}, "query": {}, "headers": {}}}
        old = differential._EXPECTED_STATUS_DIFFERENCES.copy()
        try:
            differential._EXPECTED_STATUS_DIFFERENCES.clear()
            result = differential.classify([case], [rust], [python], document)
            self.assertEqual(result["mismatches_by_cause"], {differential.OPAQUE_JSON_INTEGER_CAUSE: 1})
            self.assertEqual(result["opaque_json_precision_fields"][0]["pointer"], "#/components/schemas/JsonValue")
            altered_values = (
                {"body": {"payload": 2**64, "host": "wrong"}, "query": {}, "headers": {}},
                {"body": {"payload": 2**64}, "query": {}, "headers": {}},
                {"body": {"payload": 2**64, "host": "same", "extra": True}, "query": {}, "headers": {}},
                {"body": {"payload": 2**64, "host": "same"}, "query": {"extra": "x"}, "headers": {}},
                {"body": {"payload": 2**64, "host": "same"}, "query": {}, "headers": {"x-extra": "x"}},
                {"body": {"payload": 2**64, "host": "same", "other": 2**64}, "query": {}, "headers": {}},
            )
            for value in altered_values:
                with self.subTest(value=value), self.assertRaisesRegex(AssertionError, "opaque JsonValue"):
                    differential.classify([case], [rust], [{"status": 200, "accepted": True, "value": value}], document)
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
