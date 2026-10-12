#!/usr/bin/env python3
"""A draft can cross sync before the client performs an initial pull."""

import json
import os
import unittest

from test_isolation import isolate_supervisor_environment

isolate_supervisor_environment()

from http_backend_harness import TimedHTTPContractTest, start_contract_backend
from sync_http_contract_support import prepare_sync_store_for_rust_contract


class FirstDraftHTTPContract(TimedHTTPContractTest, unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.backend = start_contract_backend(cls)
        prepare_sync_store_for_rust_contract(cls.backend)
        cls.session = cls.backend.session
        cls.workspace_id = cls.backend.identity["workspaceId"]

    def test_first_draft_push_is_served_without_an_initial_pull(self) -> None:
        backend = self.backend
        row = {"newDocumentState": {
            "id": "phone:first-chat",
            "payload": json.dumps({
                "device": "phone", "session": "first-chat", "text": "first push",
            }),
        }}
        headers = {
            "Origin": backend.origin or "",
            "X-Canvas-Token": self.session["token"],
            "X-Canvas-Workspace": self.workspace_id,
        }
        pushed = backend.request("POST", "/api/sync/drafts", body={"rows": [row]}, headers=headers)
        if os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"):
            self.assertEqual(pushed.status, 501, pushed.body[:1000])
            self.assertEqual(
                pushed.json()["error"],
                "Not implemented: requires resource-policy/publish-drafts",
            )
            return
        self.assertEqual(pushed.status, 200, pushed.body[:1000])
        self.assertEqual(pushed.json(), [])

        pulled = backend.request("GET", "/api/sync/pull?scope=drafts&after=0")
        self.assertEqual(pulled.status, 200, pulled.body[:1000])
        documents = pulled.json()["documents"]
        self.assertEqual(len(documents), 1)
        self.assertEqual(documents[0]["id"], "phone:first-chat")
        self.assertEqual(json.loads(documents[0]["payload"])["text"], "first push")


if __name__ == "__main__":
    unittest.main(verbosity=2)
