#!/usr/bin/env python3
"""Authenticated relay replay and bounded protocol-3 stream contracts."""
from __future__ import annotations

import importlib.util
import json
import sys
import unittest
from pathlib import Path
from urllib.parse import urlencode
from urllib.request import Request, urlopen

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()
HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent / "src"))
from http_backend_harness import HTTP_WAIT_GUARD_SECONDS, TimedHTTPContractTest, no_write_snapshot, start_contract_backend

spec = importlib.util.spec_from_file_location("p14_stream_support", HERE / "rust-sync-stream-http-contract.py")
assert spec is not None and spec.loader is not None
support = importlib.util.module_from_spec(spec)
spec.loader.exec_module(support)


class SyncResourcesContract(TimedHTTPContractTest, unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        # No agents/events are created: dispatcher state is quiescent for exact no-write checks.
        cls.backend = start_contract_backend(cls)

    def headers(self):
        return {"Origin": self.backend.origin,
                "X-Canvas-Token": self.backend.session["token"],
                "X-Studio-API-Schema": self.backend.schema_hash}

    def open_stream(self, resources, revision=None):
        path = "/api/sync/stream?" + urlencode({"protocol": "3", "resources": json.dumps(resources)})
        headers = {} if revision is None else {"Last-Event-ID": str(revision)}
        return urlopen(Request(self.backend.origin + path, headers=headers), timeout=HTTP_WAIT_GUARD_SECONDS)

    def initial(self, stream):
        resource = support.read_event(stream)
        rates = support.read_event(stream)
        self.assertEqual([resource["event"], rates["event"]], ["resources", "token-rates"])
        self.assertEqual(rates["data"]["rates"], {})
        self.assertEqual(rates["data"]["teams"], {})
        self.assertEqual(rates["data"]["revision"], resource["data"]["revision"])
        return resource["data"]

    def test_authenticated_replay_conflict_disconnect_reconnect_and_no_writes(self):
        resources = [{"kind": "accounts"}, {"kind": "session-cost", "agentId": "fixture-agent"}]
        body = {"requestId": "p14-lost-ack", "resources": resources}
        before = no_write_snapshot(self.backend.state)
        with self.open_stream(resources) as stream:
            initial = self.initial(stream)
            self.assertEqual(initial["reason"], "initial")
            denied = self.backend.request("POST", "/api/sync/notify", body=body)
            self.assertEqual(denied.status, 403, denied.body)
            with self.open_stream(resources, initial["revision"]) as probe:
                self.assertEqual(self.initial(probe)["revision"], initial["revision"])
            accepted = self.backend.request("POST", "/api/sync/notify", body=body, headers=self.headers())
            self.assertEqual(accepted.status, 200, accepted.body)
            self.assertEqual(accepted.json(), {"requestId": "p14-lost-ack", "accepted": True})
            event = support.read_event(stream)
            self.assertEqual(event["event"], "resources")
            changed = event["data"]
            self.assertGreater(changed["revision"], initial["revision"])
            self.assertCountEqual(changed["resources"], resources)
        # A changed order and duplicates are the same set, including after disconnect.
        replay = dict(body, resources=[resources[1], resources[0], resources[0]])
        result = self.backend.request("POST", "/api/sync/notify", body=replay, headers=self.headers())
        self.assertEqual(result.status, 200, result.body)
        conflict = self.backend.request("POST", "/api/sync/notify", body=dict(body, resources=[{"kind":"models"}]), headers=self.headers())
        self.assertEqual(conflict.status, 409, conflict.body)
        with self.open_stream(resources, changed["revision"]) as stream:
            reconnected = self.initial(stream)
            self.assertEqual(reconnected["reason"], "reconnect")
            self.assertEqual(reconnected["revision"], changed["revision"])
            self.assertEqual(reconnected["epoch"], initial["epoch"])
        self.assertEqual(no_write_snapshot(self.backend.state), before)

    def test_state_notify_emits_reset_without_a_store_write(self):
        resources = [{"kind": "state"}]
        before = no_write_snapshot(self.backend.state)
        with self.open_stream(resources) as stream:
            initial = self.initial(stream)
            result = self.backend.request("POST", "/api/sync/notify", body={"requestId":"p14-state-reset", "resources":resources}, headers=self.headers())
            self.assertEqual(result.status, 200, result.body)
            changed = support.read_event(stream)["data"]
            self.assertGreater(changed["revision"], initial["revision"])
            self.assertEqual(changed["resourceVersions"][0]["revision"], initial["resourceVersions"][0]["revision"])
            self.assertIs(changed["resourceVersions"][0]["entitySequenceReset"], True)
        self.assertEqual(no_write_snapshot(self.backend.state), before)

    def test_resource_query_bound_and_notify_identity_bound(self):
        resources = [{"kind": "session-cost", "agentId": f"agent-{index}"} for index in range(256)]
        with self.open_stream(resources) as stream:
            self.assertEqual(len(self.initial(stream)["resources"]), 256)
        invalid = self.backend.request("GET", "/api/sync/stream?" + urlencode({"protocol":"3", "resources":json.dumps(resources + [{"kind":"accounts"}])}))
        self.assertEqual(invalid.status, 400, invalid.body)
        for request_id, expected in [("x" * 200, 200), ("x" * 201, 400), ("", 400)]:
            result = self.backend.request("POST", "/api/sync/notify", body={"requestId": request_id, "resources": [{"kind":"accounts"}]}, headers=self.headers())
            self.assertEqual(result.status, expected, result.body)


if __name__ == "__main__":
    unittest.main()
