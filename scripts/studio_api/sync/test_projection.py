"""Historical runtime records must pass the public state response boundary."""

from __future__ import annotations

from contextlib import contextmanager
from collections.abc import Iterator
from typing import Any, cast
import unittest

from pydantic import ValidationError

from codex_canvas import Canvas
from studio_api.context import ApiContext
from studio_api.models import JsonValue
from studio_api.sync.models import SnapshotAgentDto, StateSnapshot
from studio_api.sync.projection import project_snapshot


def historical_snapshot() -> dict[str, Any]:
    agent: dict[str, JsonValue] = {
        "id": "worker-a", "kind": "agent", "workerBaseBehindMain": 182,
        "nativeNameSynced": {"accountKey": "default", "threadId": "thread-a", "name": "Worker"},
        "accountHistory": [{"threadId": "old-thread"}], "deliveredMode": {"epoch": ["thread-a", 136]},
        "nativeRelease": {"phase": "released", "targetEpoch": 0, "targetRootId": "lead-a"},
        "startAttempt": {"id": "attempt-a", "claudeInputRequest": {"input": [{"text": "private prompt"}]}},
    }
    limits: dict[str, JsonValue] = {"accountKey": "default", "at": 1.0, "processedAt": 2.0, "checkedAt": 3.0}
    return {
        "token": "test", "stateDir": "/isolated", "at": 1.0,
        "threads": [agent], "nodes": [agent], "chats": [], "edges": [],
        "runtime": {
            "agents": [agent], "projects": [], "projectOrganizationVersion": 1,
            "peerTeamsVersion": 1, "peerTeams": [], "tasks": [], "tasksHistoryLimit": 100,
            "monitors": [], "requests": [], "rooms": [], "complaints": [], "events": [],
            "nativeNotices": [], "connected": False, "rateLimits": limits,
            "rateLimitsByAccount": {"default": limits},
            "work": [{"id": "work-a", "archive": {"status": "kept"},
                      "archiveIntent": {"status": "pending"}, "releases": [{"agent": "worker-a"}]}],
            "rules": [{"id": "rule-a", "status": "completed", "lastExitCode": 0,
                       "restartHoldNotified": {"epoch": 1, "reason": "restart"},
                       "lastOutput": "", "lastFinished": 2.0, "activeWorkers": 1,
                       "lastStallExitCode": 0, "stallProbe": False, "eventText": "event"}],
        },
    }


class SnapshotProjectionTests(unittest.TestCase):
    def test_actual_context_projects_private_fields_and_keeps_public_history(self) -> None:
        raw = historical_snapshot()

        class RuntimeStub:
            @contextmanager
            def read_db(self) -> Iterator[None]:
                yield None

            def snapshot(self, **_kwargs: object) -> dict[str, Any]:
                return cast(dict[str, Any], raw["runtime"])

        class CanvasStub:
            runtime = RuntimeStub()

            def snapshot(self, **_kwargs: object) -> dict[str, Any]:
                return {key: value for key, value in raw.items() if key != "runtime"}

        context = ApiContext.for_schema()
        context.canvas = cast(Canvas, CanvasStub())
        snapshot = StateSnapshot.model_validate(context.snapshot())
        assert snapshot.runtime is not None
        assert isinstance(snapshot.nodes[0], SnapshotAgentDto)
        assert snapshot.runtime.work is not None
        for agent in (snapshot.threads[0], snapshot.nodes[0], snapshot.runtime.agents[0]):
            self.assertEqual(agent.workerBaseBehindMain, 182)
            assert agent.nativeNameSynced is not None
            self.assertEqual(agent.nativeNameSynced.name, "Worker")
            data = agent.model_dump(exclude_unset=True)
            self.assertNotIn("accountHistory", data)
            self.assertNotIn("deliveredMode", data)
            self.assertNotIn("targetEpoch", data["nativeRelease"])
            self.assertNotIn("claudeInputRequest", data["startAttempt"])
        self.assertEqual(snapshot.runtime.rateLimits.processedAt, 2.0)
        self.assertEqual(snapshot.runtime.rules[0].status, "completed")
        self.assertEqual(snapshot.runtime.rules[0].lastExitCode, 0)
        self.assertEqual(snapshot.runtime.work[0].archive, {"status": "kept"})
        self.assertIn("accountHistory", raw["threads"][0])
        self.assertIn("claudeInputRequest", raw["threads"][0]["startAttempt"])

    def test_projection_keeps_strict_public_field_validation(self) -> None:
        raw = historical_snapshot()
        raw["threads"][0]["workerBaseBehindMain"] = "182"
        with self.assertRaises(ValidationError):
            StateSnapshot.model_validate(project_snapshot(raw))
