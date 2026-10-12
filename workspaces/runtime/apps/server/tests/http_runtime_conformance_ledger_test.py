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

    def test_pre_tightening_python_boundary_envelope_allowance_is_narrow_and_counted(self) -> None:
        case = {
            "path": "/x",
            "method": "POST",
            "label": ":type:NoneType",
            "raw": "null",
            "bodyPresent": True,
        }
        document = {"paths": {"/x": {"post": {"operationId": "op"}}}}
        rust = {"status": 400, "accepted": False, "body": {"error": "Invalid request", "location": ["body"]}}
        python = {"status": 400, "accepted": False, "body": {"error": "JSON object required"}}
        self.assertTrue(differential.allowed_pre_tightening_python_boundary_envelope(case, rust, python))
        old = differential._EXPECTED_STATUS_DIFFERENCES.copy()
        try:
            differential._EXPECTED_STATUS_DIFFERENCES.clear()
            result = differential.classify([case], [rust], [python], document)
            self.assertEqual(result["mismatches"], 1)
            self.assertEqual(result["mismatches_by_cause"], {
                "temporary: Python refusal without a location on the pre-tightening baseline": 1
            })
            self.assertFalse(differential.allowed_pre_tightening_python_boundary_envelope(
                {**case, "label": "nested/value:type:NoneType"}, rust, python
            ))
            self.assertTrue(differential.allowed_pre_tightening_python_boundary_envelope(
                {**case, "label": ":type:bool", "raw": "true"}, rust, python
            ))
            with self.assertRaisesRegex(AssertionError, "Rust refusal lacks its located error envelope"):
                differential.classify([case], [{**rust, "body": {"error": "Invalid request"}}], [python], document)
        finally:
            differential._EXPECTED_STATUS_DIFFERENCES.clear()
            differential._EXPECTED_STATUS_DIFFERENCES.update(old)

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

    def test_repeated_scalar_predicate_requires_400_vs_200_before_observations(self) -> None:
        case = {"path": "/x", "method": "GET", "label": "query:repeated:p"}
        document = {"paths": {"/x": {"get": {
            "operationId": "op",
            "parameters": [{"name": "p", "in": "query", "schema": {"type": "integer"}}],
        }}}}
        rows = [
            {"status": 200, "accepted": True, "value": {"query": {"p": 1}}},
            {"status": 200, "accepted": True, "value": {"query": {"p": 1}}},
        ]
        with self.assertRaisesRegex(AssertionError, r"predicts statuses \(400, 200\)"):
            differential.classify([case], [rows[0]], [rows[1]], document)

    def test_predicted_class_counts_agreement_refusals_and_requires_an_expected_difference(self) -> None:
        case = {"path": "/x", "method": "GET", "label": "query:repeated:p"}
        operation = {
            "operationId": "op",
            "parameters": [{"name": "p", "in": "query", "schema": {"type": "integer"}}],
        }
        document = {"paths": {"/x": {"get": operation}}}
        refusal = {"status": 400, "accepted": False, "body": {"error": "bad", "location": ["query", "p"]}}
        accepted = {"status": 200, "accepted": True, "value": {"body": None, "query": {"p": 1}, "headers": {}}}
        old = differential._EXPECTED_STATUS_DIFFERENCES.copy()
        try:
            differential._EXPECTED_STATUS_DIFFERENCES.clear()
            mixed = differential.classify(
                [case, case], [refusal, refusal], [refusal, accepted], document
            )
            self.assertEqual(
                mixed["agreement_on_refusal_by_cause"][differential.REPEATED_SCALAR_QUERY_CAUSE],
                1,
            )
            self.assertEqual(mixed["mismatches_by_cause"], {
                differential.REPEATED_SCALAR_QUERY_CAUSE: 1
            })
            with self.assertRaisesRegex(AssertionError, "no case with their exact expected difference"):
                differential.classify([case], [refusal], [refusal], document)
        finally:
            differential._EXPECTED_STATUS_DIFFERENCES.clear()
            differential._EXPECTED_STATUS_DIFFERENCES.update(old)

    def test_unbounded_body_integer_allows_refusal_agreement_and_counts_it(self) -> None:
        operation = {
            "operationId": "create_widget",
            "requestBody": {"content": {"application/json": {"schema": {"$ref": "#/components/schemas/Widget"}}}},
        }
        document = {
            "paths": {"/widgets": {"post": operation}},
            "components": {"schemas": {"Widget": {
                "type": "object", "properties": {"count": {"type": "integer"}},
            }}},
        }
        refusal_case = {"path": "/widgets", "method": "POST", "label": "count:lax:18446744073709551616"}
        precision_case = {"path": "/widgets", "method": "POST", "label": "count:lax:1267650600228229401496703205376"}
        refusal = {"accepted": False, "status": 400, "body": {"error": "bad", "location": ["body", "count"]}}
        rust_accept = {"accepted": True, "status": 200, "value": {"body": {"count": float(2**100)}, "query": {}, "headers": {}}}
        python_accept = {"accepted": True, "status": 200, "value": {"body": {"count": 2**100}, "query": {}, "headers": {}}}
        old = differential._EXPECTED_STATUS_DIFFERENCES.copy()
        try:
            differential._EXPECTED_STATUS_DIFFERENCES.clear()
            result = differential.classify(
                [refusal_case, precision_case],
                [refusal, rust_accept],
                [refusal, python_accept],
                document,
            )
            self.assertEqual(
                result["agreement_on_refusal_by_cause"][differential.UNBOUNDED_INTEGER_CAUSE],
                1,
            )
            self.assertEqual(result["mismatches_by_cause"], {
                differential.UNBOUNDED_INTEGER_CAUSE: 1
            })
        finally:
            differential._EXPECTED_STATUS_DIFFERENCES.clear()
            differential._EXPECTED_STATUS_DIFFERENCES.update(old)

    def test_unbounded_integer_nested_in_nullable_array_is_predicted(self) -> None:
        case = {
            "path": "/rules", "method": "POST",
            "label": "fingerprint/0:lax:18446744073709551616",
        }
        operation = {
            "operationId": "update_rules",
            "requestBody": {"content": {"application/json": {
                "schema": {"$ref": "#/components/schemas/RuleBody"},
            }}},
        }
        document = {
            "paths": {"/rules": {"post": operation}},
            "components": {"schemas": {"RuleBody": {
                "type": "object", "properties": {"fingerprint": {"anyOf": [
                    {"type": "array", "items": {"type": "integer"}},
                    {"type": "null"},
                ]}},
            }}},
        }
        rust = {
            "accepted": True, "status": 200,
            "value": {"body": {"fingerprint": [float(2**64)]}, "query": {}, "headers": {}},
        }
        python = {
            "accepted": True, "status": 200,
            "value": {"body": {"fingerprint": [2**64]}, "query": {}, "headers": {}},
        }
        predicted = differential._predicted_difference(case, operation, document)
        self.assertEqual(predicted, (200, 200, differential.UNBOUNDED_INTEGER_CAUSE))
        old = differential._EXPECTED_STATUS_DIFFERENCES.copy()
        try:
            differential._EXPECTED_STATUS_DIFFERENCES.clear()
            result = differential.classify([case], [rust], [python], document)
            self.assertEqual(result["mismatches_by_cause"], {
                differential.UNBOUNDED_INTEGER_CAUSE: 1
            })
            self.assertEqual(result["typed_integer_precision_fields"][0]["pointer"],
                "#/components/schemas/RuleBody/properties/fingerprint/anyOf/0/items")
        finally:
            differential._EXPECTED_STATUS_DIFFERENCES.clear()
            differential._EXPECTED_STATUS_DIFFERENCES.update(old)

    def test_numeric_leaf_allowance_preserves_strict_comparison_of_the_rest(self) -> None:
        case = {
            "path": "/widgets", "method": "POST",
            "label": "count:lax:18446744073709551616",
            "body": {"count": 2**64},
        }
        operation = {
            "operationId": "create_widget",
            "requestBody": {"content": {"application/json": {"schema": {"$ref": "#/components/schemas/Widget"}}}},
        }
        document = {"paths": {"/widgets": {"post": operation}}, "components": {"schemas": {
            "Widget": {"type": "object", "properties": {"count": {"type": "integer"}}}
        }}}
        rust_value = {"body": {"count": float(2**64)}, "query": {"q": "x"}, "headers": {}}
        python_value = {"body": {"count": 2**64}, "query": {"q": "x"}, "headers": {}}
        self.assertTrue(differential.strict_equal(
            rust_value,
            {**python_value, "body": differential._normalize_allowed_integer_leaf(
                case, operation, document, rust_value["body"], python_value["body"],
                differential.UNBOUNDED_INTEGER_CAUSE,
            )},
        ))
        altered = [
            {**python_value, "body": {"count": 2**64, "host": "wrong"}},
            {**python_value, "body": {}},
            {**python_value, "query": {"q": "x", "extra": "y"}},
            {**python_value, "headers": {"x-extra": "y"}},
            {**python_value, "body": {"count": 2**64, "other": float(2**64)}},
        ]
        for value in altered:
            with self.subTest(value=value):
                with self.assertRaisesRegex(AssertionError, "numeric allowance predicate|unrelated value mismatch"):
                    differential.classify(
                        [case],
                        [{"status": 200, "accepted": True, "value": rust_value}],
                        [{"status": 200, "accepted": True, "value": value}],
                        document,
                    )

    def test_unbounded_integer_exception_requires_exact_f64_and_python_integer(self) -> None:
        operation = {
            "operationId": "create_widget",
            "requestBody": {"content": {"application/json": {"schema": {"$ref": "#/components/schemas/Widget"}}}},
        }
        case = {
            "method": "POST",
            "path": "/widgets",
            "label": "count:lax:18446744073709551616",
        }
        rows = (
            {"accepted": True, "status": 200, "value": {"body": {"count": float(2**64)}}},
            {"accepted": True, "status": 200, "value": {"body": {"count": 2**64}}},
        )
        document = {"components": {"schemas": {"Widget": {"type": "object", "properties": {"count": {"anyOf": [{"type": "integer"}, {"type": "null"}]}}}}}}
        self.assertEqual(
            differential.opaque_integer_difference(case, *rows, operation, document),
            differential.UNBOUNDED_INTEGER_CAUSE,
        )
        self.assertEqual(
            differential.body_schema_pointer(document, operation, case),
            "#/components/schemas/Widget/properties/count",
        )

        document["components"]["schemas"]["Widget"]["properties"]["count"]["anyOf"][0]["maximum"] = 2**53 - 1
        self.assertIsNone(differential.opaque_integer_difference(case, *rows, operation, document))
        self.assertIsNone(differential.opaque_integer_difference(
            case,
            {"accepted": True, "status": 200, "value": {"body": {"count": "not-float"}}},
            rows[1],
            operation,
            document,
        ))

    def test_integer_number_union_allowance_is_exactly_the_enumerated_pre_tightening_fields(self) -> None:
        pointer = "#/components/schemas/SignedEnvelope/properties/created"
        schema = {"anyOf": [{"type": "integer"}, {"type": "number"}]}
        case = {
            "method": "POST",
            "path": "/messages",
            "label": "created:lax:18446744073709551616",
        }
        operation = {
            "operationId": "message_api_federation_v1_message_post",
            "requestBody": {"content": {"application/json": {"schema": {"$ref": "#/components/schemas/SignedEnvelope"}}}},
        }
        document = {
            "paths": {"/messages": {"post": operation}},
            "components": {"schemas": {"SignedEnvelope": {
                "type": "object", "properties": {"created": schema},
            }}},
        }
        self.assertIn(pointer, differential.INTEGER_NUMBER_UNION_POINTERS)
        self.assertEqual(
            differential._predicted_difference(case, operation, document),
            (200, 200, differential.INTEGER_NUMBER_UNION_CAUSE),
        )
        rust_body = {"created": float(2**64), "host": "same"}
        python_body = {"created": 2**64, "host": "same"}
        self.assertEqual(
            differential._normalize_allowed_integer_leaf(
                case, operation, document, rust_body, python_body,
                differential.INTEGER_NUMBER_UNION_CAUSE,
            ),
            rust_body,
        )
        old = differential._EXPECTED_STATUS_DIFFERENCES.copy()
        try:
            differential._EXPECTED_STATUS_DIFFERENCES.clear()
            result = differential.classify(
                [case],
                [{"accepted": True, "status": 200, "value": {"body": rust_body, "query": {}, "headers": {}}}],
                [{"accepted": True, "status": 200, "value": {"body": python_body, "query": {}, "headers": {}}}],
                document,
            )
            self.assertEqual(result["mismatches_by_cause"], {
                differential.INTEGER_NUMBER_UNION_CAUSE: 1
            })
        finally:
            differential._EXPECTED_STATUS_DIFFERENCES.clear()
            differential._EXPECTED_STATUS_DIFFERENCES.update(old)
        for changed in (
            {"created": 2**64, "host": "changed"},
            {"created": 2**64, "extra": True},
        ):
            with self.subTest(changed=changed):
                self.assertFalse(differential.strict_equal(
                    {"body": rust_body},
                    {"body": differential._normalize_allowed_integer_leaf(
                        case, operation, document, rust_body, changed,
                        differential.INTEGER_NUMBER_UNION_CAUSE,
                    )},
                ))
        self.assertFalse(differential._approved_integer_number_union_without_wire_bound(
            {"anyOf": [{"type": "integer", "maximum": 2**53 - 1}, {"type": "number"}]}, 2**64
        ))
        self.assertFalse(differential._approved_integer_number_union_without_wire_bound(
            {"anyOf": [{"type": "integer", "minimum": -(2**53 - 1)}, {"type": "number"}]},
            -(1 << 63),
        ))

    def test_unbounded_parameter_integer_allowance_requires_exact_wire_and_schema(self) -> None:
        operation = {
            "operationId": "agent_chat",
            "parameters": [{
                "name": "before",
                "in": "query",
                "schema": {"type": "integer"},
            }],
        }
        case = {
            "method": "GET",
            "path": "/api/agent-chat",
            "label": "query:before:18446744073709551616",
        }
        rows = (
            {"accepted": False, "status": 400, "body": {"error": "Invalid request", "location": ["query", "before"]}},
            {"accepted": True, "status": 200, "value": {"query": {"before": 2**64}}},
        )
        document = {"components": {"schemas": {}}}
        result = differential.unbounded_parameter_integer_difference(
            case, *rows, operation, document
        )
        self.assertEqual(result, (
            "#/paths/~1api~1agent-chat/get/parameters/0/schema",
            differential.UNBOUNDED_PARAMETER_INTEGER_CAUSE,
        ))
        operation["parameters"][0]["schema"]["maximum"] = 2**53 - 1
        self.assertIsNone(differential.unbounded_parameter_integer_difference(
            case, *rows, operation, document
        ))
        self.assertIsNone(differential.unbounded_parameter_integer_difference(
            case, rows[0], {**rows[1], "value": {"query": {"before": float(2**64)}}}, operation, document
        ))

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
