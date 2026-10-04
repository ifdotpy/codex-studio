"""Unit coverage for the agent API's boundary models."""

import unittest

from pydantic import ValidationError

from .models import (
    AccountTransferRequest,
    CapacityRetryRequest,
    ConversationRequest,
    CreateAgentRequest,
    CreateLeadRequest,
    ImportRequest,
    NativeActionRequest,
    NativeActionResponse,
    NativeCommandRequest,
    RecoveryResponse,
    StopRequest,
    TransferResponse,
    UsageResumeResponse,
)


class AgentRequestModelTests(unittest.TestCase):
    def test_lead_retry_preserves_supplied_identity_and_omitted_fields(self) -> None:
        body = CreateLeadRequest.model_validate_json(
            '{"id":"stable-id","previous":"prior","reuse_empty":false}'
        )

        self.assertEqual(
            body.model_dump(mode="json", exclude_unset=True),
            {"id": "stable-id", "previous": "prior", "reuse_empty": False},
        )

    def test_conversation_settings_preserve_explicit_null(self) -> None:
        body = ConversationRequest.model_validate_json(
            '{"id":"agent-id","effort":null,"expected_account_key":"acct"}'
        )

        self.assertEqual(
            body.model_dump(mode="json", exclude_unset=True),
            {"id": "agent-id", "effort": None, "expected_account_key": "acct"},
        )

    def test_closed_action_unions_reject_unknown_values(self) -> None:
        with self.assertRaises(ValidationError):
            CapacityRetryRequest.model_validate_json(
                '{"id":"a","retry_id":"r","action":"replay"}'
            )
        with self.assertRaises(ValidationError):
            NativeCommandRequest.model_validate_json('{"id":"a","action":"restart"}')

    def test_safety_action_keeps_its_distinct_contract(self) -> None:
        body = NativeActionRequest.model_validate_json(
            '{"id":"a","action":{"safety":"retry","turnId":"turn-1"}}'
        )

        self.assertEqual(
            body.model_dump(mode="json", exclude_unset=True),
            {"id": "a", "action": {"safety": "retry", "turnId": "turn-1"}},
        )
        with self.assertRaises(ValidationError):
            NativeActionRequest.model_validate_json(
                '{"id":"a","action":{"safety":"compact","turnId":"turn-1"}}'
            )

    def test_existing_stop_defaults_and_account_transfer_scope(self) -> None:
        stop = StopRequest.model_validate_json('{"id":"a"}')
        transfer = AccountTransferRequest.model_validate_json(
            '{"id":"a","account_key":"acct","request_id":"durable",'
            '"scope":"subagents"}'
        )

        self.assertTrue(stop.descendants)
        self.assertEqual(transfer.scope.value, "subagents")
        self.assertEqual(transfer.request_id, "durable")

    def test_transfer_receipt_and_safety_response_have_named_shapes(self) -> None:
        transfer = TransferResponse.model_validate_json(
            '{"id":"receipt","leadId":"lead","targetAccountKey":"acct",'
            '"status":"pending","scope":"team","created":10.0,"members":'
            '{"worker":{"phase":"lazy","sourceAccountKey":"default",'
            '"lazy":true}}}'
        )
        safety = NativeActionResponse.model_validate_json(
            '{"id":"a:turn","stage":"waiting","turnId":"turn-1"}'
        )

        assert transfer.status is not None
        assert transfer.members is not None
        self.assertEqual(transfer.status.value, "pending")
        self.assertEqual(transfer.members["worker"].phase.value, "lazy")
        self.assertEqual(safety.stage, "waiting")

    def test_durable_action_receipt_is_explicit_and_closed(self) -> None:
        response = NativeActionResponse.model_validate_json(
            '{"receipt":{"requestId":"request","agentId":"agent",'
            '"action":"review","accountKey":"default","threadId":"thread",'
            '"epoch":2,"attemptId":"attempt","acceptedAt":10.0,"status":"accepted"},'
            '"outcome":{"status":"pending"},"replayed":false}'
        )
        assert response.receipt is not None
        self.assertEqual(response.receipt.requestId, "request")
        with self.assertRaises(ValidationError):
            NativeActionResponse.model_validate_json(
                '{"receipt":{"requestId":"request","agentId":"agent",'
                '"action":"review","accountKey":"default","threadId":"thread",'
                '"epoch":2,"attemptId":"attempt","acceptedAt":10.0,"status":"new"}}'
            )

    def test_native_action_identity_and_context_are_strict(self) -> None:
        valid = '{"id":"agent","action":"review","request_id":"request",' \
            '"context":{"accountKey":"default","epoch":2}}'
        self.assertEqual(
            NativeActionRequest.model_validate_json(valid).request_id,
            "request",
        )
        invalid_requests = (
            '{"id":"agent","action":"review","request_id":""}',
            '{"id":"agent","action":"review","request_id":1}',
            '{"id":"agent","action":"review","request_id":"' + "x" * 201 + '"}',
            '{"id":"agent","action":"review","context":{"secret":"value"}}',
            '{"id":"agent","action":"review","unknown":true}',
        )
        for payload in invalid_requests:
            with self.subTest(payload=payload[:60]):
                with self.assertRaises(ValidationError):
                    NativeActionRequest.model_validate_json(payload)

    def test_transfer_inflight_phase_accepts_exact_source_snapshot_shape(self) -> None:
        transfer = TransferResponse.model_validate_json(
            '{"id":"receipt","leadId":"lead","targetAccountKey":"acct",'
            '"status":"pending","scope":"team","created":10.0,"members":'
            '{"worker":{"phase":"reading","sourceAccountKey":"source",'
            '"sourceThreadId":"thread","provider":"codex"}}}'
        )

        assert transfer.members is not None
        self.assertEqual(transfer.members["worker"].phase.value, "reading")
        self.assertEqual(transfer.members["worker"].sourceAccountKey, "source")

    def test_recovery_response_matches_tool_delivery_producer(self) -> None:
        response = RecoveryResponse.model_validate_json(
            '{"status":"tool_response_delivered","requests":["request-1","request-2"]}'
        )

        self.assertEqual(response.status.value, "tool_response_delivered")
        self.assertEqual(response.requests, ["request-1", "request-2"])

    def test_recovery_response_matches_input_restore_producer(self) -> None:
        response = RecoveryResponse.model_validate_json(
            '{"status":"input_restored","attemptId":"attempt-1"}'
        )

        self.assertEqual(response.status.value, "input_restored")
        self.assertEqual(response.attemptId, "attempt-1")

    def test_usage_resume_keeps_receipt_contract(self) -> None:
        response = UsageResumeResponse.model_validate_json(
            '{"id":"resume-id","status":"scheduled","accountKey":"acct",'
            '"threadId":"thread","epoch":3,"turnId":"turn",'
            '"cause":"usage_limit","failedAt":10.0,"dueAt":20.0}'
        )
        self.assertEqual(response.status.value, "scheduled")

    def test_unknown_fields_fail_before_handler_side_effects(self) -> None:
        with self.assertRaises(ValidationError):
            CreateLeadRequest.model_validate_json(
                '{"id":"stable-id","reuse_empty":true,"unreviewed":true}'
            )

    def test_worker_prompt_and_identity_validate_before_runtime_calls(self) -> None:
        with self.assertRaises(ValidationError):
            CreateAgentRequest.model_validate_json(
                '{"id":"not-a-uuid","prompt":"   ","parent":"lead"}'
            )
        with self.assertRaises(ValidationError):
            CreateAgentRequest.model_validate_json(
                '{"id":"9750de4d-a148-49a7-a15c-14887ab55bdc",'
                '"prompt":"   ","parent":"lead"}'
            )

    def test_lead_and_import_model_names_validate_before_provider_calls(self) -> None:
        with self.assertRaises(ValidationError):
            CreateLeadRequest.model_validate_json('{"model":"   "}')
        with self.assertRaises(ValidationError):
            ImportRequest.model_validate_json(
                '{"threadId":"thread","model":"   "}'
            )


if __name__ == "__main__":
    unittest.main()
