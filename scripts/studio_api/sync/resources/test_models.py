"""Contracts for the protocol 3 typed resource stream."""

from __future__ import annotations

import unittest

from pydantic import ValidationError

from studio_api.sync.resources.models import (
    ResourceChangeEvent,
    ResourceHeartbeatEvent,
    ResourceRef,
)


class ResourceContractTests(unittest.TestCase):
    def test_resource_ref_is_a_closed_discriminated_union(self) -> None:
        parsed = ResourceRef.model_validate({"kind": "panel", "agentId": "agent-a"})
        self.assertEqual(parsed.model_dump(), {"kind": "panel", "agentId": "agent-a"})
        with self.assertRaises(ValidationError):
            ResourceRef.model_validate({"kind": "unknown"})
        with self.assertRaises(ValidationError):
            ResourceRef.model_validate({"kind": "panel", "agentId": "agent-a", "extra": True})

    def test_event_envelopes_validate_safe_revision_and_typed_refs(self) -> None:
        event = ResourceChangeEvent.model_validate({
            "protocol": 3,
            "workspaceId": "workspace-a",
            "epoch": "server-epoch",
            "revision": 0,
            "reason": "initial",
            "resources": [{"kind": "panel", "agentId": "agent-a"}],
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
        with self.assertRaises(ValidationError):
            ResourceChangeEvent.model_validate({
                "protocol": 3,
                "workspaceId": "workspace-a",
                "epoch": "server-epoch",
                "revision": 0,
                "reason": "change",
                "resources": [{"kind": "panel"}],
            })


if __name__ == "__main__":
    unittest.main()
