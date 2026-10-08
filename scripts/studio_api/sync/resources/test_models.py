"""Contracts for the protocol 3 typed resource stream."""

from __future__ import annotations

import unittest
import math

from pydantic import ValidationError

from studio_api.sync.resources.models import (
    EntityChangeBatch,
    MAX_ENTITY_CHANGE_BYTES,
    MAX_ENTITY_CHANGE_DOCUMENTS,
    ResourceChangeEvent,
    ResourceHeartbeatEvent,
    ResourceRef,
    ResourceTokenRatesEvent,
    TranscriptResource,
)


class ResourceContractTests(unittest.TestCase):
    def test_entity_batch_exports_bounded_documents_and_rejects_false_ranges(self) -> None:
        document = {"id": "entity:project:p", "seq": 2, "payload": "{}", "_deleted": False}
        valid = {"after": 1, "through": 2, "documents": [document]}
        parsed = EntityChangeBatch.model_validate(valid)
        self.assertEqual(parsed.model_dump(by_alias=True), valid)
        invalid_batches = (
            {**valid, "after": 2},
            {**valid, "after": -1},
            {**valid, "through": 9_007_199_254_740_992},
            {**valid, "documents": [{**document, "seq": 1}]},
            {**valid, "documents": [{**document, "seq": 3}]},
            {**valid, "documents": [document, document]},
            {**valid, "documents": [{**document, "payload": "x" * MAX_ENTITY_CHANGE_BYTES}]},
            {**valid, "documents": [document] * (MAX_ENTITY_CHANGE_DOCUMENTS + 1)},
        )
        for value in invalid_batches:
            with self.subTest(value=str(value)[:80]), self.assertRaises(ValidationError):
                EntityChangeBatch.model_validate(value)
        schema = EntityChangeBatch.model_json_schema()
        self.assertEqual(schema["properties"]["documents"]["maxItems"], MAX_ENTITY_CHANGE_DOCUMENTS)

        event = {
            "protocol": 3, "workspaceId": "w", "epoch": "e", "revision": 1,
            "reason": "change", "resources": [{"kind": "state"}],
            "resourceVersions": [{"revision": 2, "entityChanges": valid}],
        }
        ResourceChangeEvent.model_validate(event)
        for invalid in (
            {**event, "reason": "initial"},
            {**event, "resources": [{"kind": "accounts"}]},
            {**event, "resources": []},
            {**event, "resourceVersions": [{"revision": 3, "entityChanges": valid}]},
            {**event, "resourceVersions": [{"revision": 2, "entityChanges": valid, "entitySequenceReset": True}]},
        ):
            with self.subTest(event=invalid), self.assertRaises(ValidationError):
                ResourceChangeEvent.model_validate(invalid)

    def test_resource_ref_is_a_closed_discriminated_union(self) -> None:
        parsed = ResourceRef.model_validate({"kind": "panel", "agentId": "agent-a"})
        self.assertEqual(parsed.model_dump(), {"kind": "panel", "agentId": "agent-a"})
        transcript = ResourceRef(TranscriptResource(kind="transcript", agentId="agent-a"))
        self.assertEqual(transcript.model_dump(), {"kind": "transcript", "agentId": "agent-a"})
        with self.assertRaises(ValidationError):
            ResourceRef.model_validate({"kind": "unknown"})
        with self.assertRaises(ValidationError):
            ResourceRef.model_validate({"kind": "worktree-disk", "agentId": "agent-a"})
        with self.assertRaises(ValidationError):
            ResourceRef.model_validate({"kind": "panel", "agentId": "agent-a", "extra": True})
        with self.assertRaises(ValidationError):
            ResourceRef.model_validate({"kind": "panel", "agentId": ""})

    def test_event_envelopes_validate_safe_revision_and_typed_refs(self) -> None:
        event = ResourceChangeEvent.model_validate({
            "protocol": 3,
            "workspaceId": "workspace-a",
            "epoch": "server-epoch",
            "revision": 0,
            "reason": "initial",
            "resources": [{"kind": "panel", "agentId": "agent-a"}],
            "resourceVersions": [
                {
                    "resource": {"kind": "panel", "agentId": "agent-a"},
                    "revision": 0,
                }
            ],
        })
        self.assertEqual(event.resources[0].root.kind, "panel")
        heartbeat = ResourceHeartbeatEvent.model_validate({
            "protocol": 3,
            "workspaceId": "workspace-a",
            "epoch": "server-epoch",
            "revision": 0,
        })
        self.assertEqual(heartbeat.revision, 0)
        for invalid in (-1, 9_007_199_254_740_992, 1.5):
            with self.subTest(revision=invalid), self.assertRaises(ValidationError):
                ResourceHeartbeatEvent.model_validate({
                    "protocol": 3,
                    "workspaceId": "workspace-a",
                    "epoch": "server-epoch",
                    "revision": invalid,
                })

    def test_token_rate_event_uses_exact_typed_workspace_snapshot_shape(self) -> None:
        event = ResourceTokenRatesEvent.model_validate({
            "protocol": 3,
            "workspaceId": "workspace-a",
            "epoch": "server-epoch",
            "revision": 7,
            "rates": {
                "agent-a": {
                    "turnId": "turn-a",
                    "active": True,
                    "estimated": False,
                    "rate": 12.5,
                    "outputTokens": 25,
                }
            },
            "teams": {},
        })
        self.assertEqual(event.rates["agent-a"].outputTokens, 25)
        with self.assertRaises(ValidationError):
            ResourceTokenRatesEvent.model_validate({
                "protocol": 3,
                "workspaceId": "workspace-a",
                "epoch": "server-epoch",
                "revision": 7,
                "rates": {"agent-a": {
                    "turnId": "",
                    "active": True,
                    "estimated": False,
                    "rate": 12.5,
                    "outputTokens": 25,
                }},
                "teams": {},
            })
        with self.assertRaises(ValidationError):
            ResourceTokenRatesEvent.model_validate({
                "protocol": 3,
                "workspaceId": "",
                "epoch": "server-epoch",
                "revision": 7,
                "rates": {},
                "teams": {},
            })
        with self.assertRaises(ValidationError):
            ResourceTokenRatesEvent.model_validate({
                "protocol": 3,
                "workspaceId": "workspace-a",
                "epoch": "server-epoch",
                "revision": 7,
                "rates": {"agent-a": {"rate": 12.5}},
                "teams": {},
            })
        with self.assertRaises(ValidationError):
            ResourceChangeEvent.model_validate({
                "protocol": 3,
                "workspaceId": "workspace-a",
                "epoch": "server-epoch",
                "revision": 0,
                "reason": "change",
                "resources": [{"kind": "panel"}],
                "resourceVersions": [],
            })

    def test_token_rates_reject_non_finite_and_negative_values_and_export_json_constraints(self) -> None:
        base = {
            "protocol": 3,
            "workspaceId": "workspace-a",
            "epoch": "server-epoch",
            "revision": 1,
            "rates": {"agent-a": {
                "turnId": "turn-a",
                "active": True,
                "estimated": False,
                "rate": 1.0,
                "outputTokens": 0,
            }},
            "teams": {},
        }
        for invalid in (-0.1, math.inf, math.nan):
            with self.subTest(rate=invalid), self.assertRaises(ValidationError):
                ResourceTokenRatesEvent.model_validate({
                    **base,
                    "rates": {"agent-a": {**base["rates"]["agent-a"], "rate": invalid}},
                })
        schema = ResourceTokenRatesEvent.model_json_schema()
        rate_schema = schema["$defs"]["TokenRateValue"]["properties"]["rate"]
        self.assertTrue(all("minimum" in option and option["minimum"] == 0 for option in rate_schema["anyOf"]))


if __name__ == "__main__":
    unittest.main()
