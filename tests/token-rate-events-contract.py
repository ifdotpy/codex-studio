#!/usr/bin/env python3
"""Token-rate resource publication follows actual telemetry changes."""
from test_isolation import isolate_supervisor_environment

isolate_supervisor_environment()

from pathlib import Path
import sys
import types
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
from codex_token_rate import TokenRates


class TokenRateEventContracts(unittest.TestCase):
    def test_first_runtime_observation_seeds_hub_initialized_before_runtime(self):
        publications = []
        hub = types.ModuleType("studio_api.sync.resources.hub")
        hub.publish_token_rates = lambda state_dir, snapshot: publications.append((state_dir, snapshot))
        models = types.ModuleType("studio_api.sync.resources.models")

        class TokenRateValue:
            def __init__(self, **values):
                self.values = values

        class TokenRateSnapshot:
            def __init__(self, *, rates, teams):
                self.rates = rates
                self.teams = teams

        models.TokenRateValue = TokenRateValue
        models.TokenRateSnapshot = TokenRateSnapshot
        # ResourceHub setup precedes Runtime in make_server. Register the stub
        # first, then let the first runtime telemetry event seed its snapshot.
        with patch.dict(sys.modules, {
            "studio_api.sync.resources.hub": hub,
            "studio_api.sync.resources.models": models,
        }):
            rates = TokenRates(clock=lambda: 100.0, state_dir="/private/state")
            agent = {"id": "agent-a", "rootId": "agent-a", "threadId": "thread-a",
                     "inFlight": True, "turnId": "turn-a"}
            rates.observe(agent, "turn/started", {"turnId": "turn-a"},
                          "account-a", "connection-a", 100.0)

        self.assertEqual(len(publications), 1)
        state_dir, snapshot = publications[0]
        self.assertEqual(state_dir, Path("/private/state"))
        self.assertIn("agent-a", snapshot.rates)
        self.assertEqual(snapshot.rates["agent-a"].values["turnId"], "turn-a")
        self.assertEqual(snapshot.teams, {})

    def test_changed_snapshots_publish_after_rate_lock_release(self):
        rates = TokenRates(clock=lambda: 100.0)
        publications = []

        def capture(previous):
            acquired = rates.lock.acquire(blocking=False)
            current = rates.workspace_snapshot()
            publications.append((previous, current, acquired))
            if acquired:
                rates.lock.release()

        rates._publish_if_changed = capture
        agent = {"id": "agent-a", "rootId": "agent-a", "threadId": "thread-a", "inFlight": True, "turnId": "turn-a"}
        rates.observe(agent, "turn/started", {"turnId": "turn-a"}, "account-a", "connection-a", 100.0)
        self.assertEqual(len(publications), 1)
        self.assertEqual(publications[0][0], {"rates": {}, "teams": {}})
        self.assertIn("agent-a", publications[0][1]["rates"])
        self.assertTrue(publications[0][2])

        rates.stream(
            "item/agentMessage/delta",
            {"threadId": "thread-a", "turnId": "turn-a", "delta": "x" * 40},
            "account-a",
            "connection-a",
            102.0,
        )
        self.assertEqual(len(publications), 2)
        self.assertTrue(publications[1][2])
        self.assertTrue(publications[1][1]["rates"]["agent-a"]["estimated"])

        rates.observe(agent, "turn/completed", {"turnId": "turn-a"}, "account-a", "connection-a", 103.0)
        self.assertEqual(len(publications), 3)
        self.assertFalse(publications[2][1]["rates"]["agent-a"]["active"])
        self.assertTrue(publications[2][2])

        key = ("account-a", "connection-a", "thread-a")
        rates.expiry_timers[key].cancel()
        rates._expire(key, "agent-a", "turn-a")
        self.assertEqual(len(publications), 4)
        self.assertEqual(publications[3][1], {"rates": {}, "teams": {}})
        self.assertTrue(publications[3][2])


if __name__ == "__main__":
    unittest.main()
