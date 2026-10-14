#!/usr/bin/env python3
"""HTTP boundary contract for portable-history native thread import."""

from __future__ import annotations

from test_isolation import isolate_supervisor_environment

import json
import hashlib
import os
import sqlite3
import sys
import time
import unittest
import uuid
from pathlib import Path


isolate_supervisor_environment()
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from http_backend_harness import TimedHTTPContractTest, start_contract_backend


class PortableHistoryImportHTTPContract(TimedHTTPContractTest, unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.backend = start_contract_backend(cls, "portable-history-import.json")
        if os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"):
            # Rust routes use the retained session of a live account worker.
            # Seed one shared-workspace queued event: a bare worktree has no
            # source commit, while the provider session itself is independent
            # of the source thread used by import and branch requests.
            response = cls.backend.request(
                "POST",
                "/api/leads",
                body={
                    "id": str(uuid.uuid4()),
                    "cwd": str(cls.backend.cwd),
                },
                headers={
                    "Origin": cls.backend.origin or "",
                    "X-Canvas-Token": cls.backend.session["token"],
                    "X-Studio-API-Schema": cls.backend.schema_hash,
                },
            )
            if response.status != 200:
                raise AssertionError(response.body[:1000])
            agent_id = response.json()["id"]
            with sqlite3.connect(cls.backend.state / "canvas.sqlite3") as database:
                agent = json.loads(database.execute(
                    "SELECT record FROM runtime_agents WHERE id=?", (agent_id,),
                ).fetchone()[0])
                agent.update(
                    status="queued", autoWake=True, workspaceMode="shared",
                    worktree=False, worktreeReady=False, cwd=str(cls.backend.cwd),
                    hostProjectPath=str(cls.backend.cwd),
                )
                agent.pop("error", None)
                agent.pop("branch", None)
                database.create_function(
                    "sync_entity_payload", 4,
                    lambda collection, key, raw, deleted: json.dumps(
                        {"collection": collection, "id": key,
                         "value": {} if deleted else json.loads(raw),
                         "deleted": bool(deleted)}, sort_keys=True, separators=(",", ":"),
                    ),
                )
                database.create_function(
                    "sync_entity_hash", 1,
                    lambda payload: hashlib.sha256(payload.encode()).hexdigest(),
                )
                database.execute(
                    "UPDATE runtime_agents SET record=? WHERE id=?",
                    (json.dumps(agent), agent_id),
                )
                database.execute(
                    "INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch,turn_id,error) "
                    "VALUES(?,?,?,?,?,?,?,?,NULL)",
                    (f"{agent_id}:initial", agent_id, "user",
                     "Fixture account session prewarm.", "pending", time.time(),
                     agent.get("epoch", 0), None),
                )
                database.commit()
            cls.backend.wait_until(
                lambda: any(
                    frame.get("method") == "thread/start"
                    for frame in cls.backend.received_provider_frames()
                ),
                "Rust account worker retained provider session",
            )

    def _agent_count(self) -> int:
        with sqlite3.connect(self.backend.state / "canvas.sqlite3") as database:
            return database.execute("SELECT COUNT(*) FROM runtime_agents").fetchone()[0]

    def _common_headers(self) -> dict[str, str]:
        return {
            "Origin": self.backend.origin or "",
            "X-Canvas-Token": self.backend.session["token"],
            "X-Studio-API-Schema": self.backend.schema_hash,
        }

    def _error_detail(self, response) -> str:
        logs = []
        database = self.backend.state / "canvas.sqlite3"
        if database.is_file():
            with sqlite3.connect(database) as connection:
                for table in ("runtime_agents", "runtime_workspace_operations"):
                    try:
                        if table == "runtime_agents":
                            rows = connection.execute(
                                "SELECT id,json_extract(record,'$.status'),json_extract(record,'$.threadId'),json_extract(record,'$.importedFrom'),json_extract(record,'$.workspaceOperation') FROM runtime_agents ORDER BY rowid DESC LIMIT 6",
                            ).fetchall()
                        else:
                            rows = connection.execute(
                                "SELECT id,json_extract(record,'$.phase'),json_extract(record,'$.error'),json_extract(record,'$.result') FROM runtime_workspace_operations ORDER BY rowid DESC LIMIT 6",
                            ).fetchall()
                    except sqlite3.Error:
                        continue
                    logs.append(f"{table}: {json.dumps(rows, default=str)}")
                try:
                    rows = connection.execute(
                        "SELECT id,substr(result,1,300) FROM runtime_operation_receipts ORDER BY rowid DESC LIMIT 12",
                    ).fetchall()
                    logs.append(f"runtime_operation_receipts: {json.dumps(rows, default=str)}")
                except sqlite3.Error:
                    pass
        frames = self.backend.received_provider_frames()
        logs.append(f"provider frames: {[(f.get('method'), f.get('id'), f.get('params')) for f in frames]!r}")
        logs.append(f"backend stderr: {self.backend.stderr[-3000:]!r}")
        for path in (
            self.backend.state / "app-server.log",
            self.backend.state / "diagnostics/http-requests.json",
            self.backend.state / "diagnostics/sqlite-transactions.json",
        ):
            if path.is_file():
                logs.append(f"{path.name}: {path.read_text(errors='replace')[-3000:]}")
        return f"{response.body[:1000]!r} {' '.join(logs)}"

    def _seed_branch_source(self) -> tuple[str, str]:
        agent_id = str(uuid.uuid4())
        created = self.backend.request(
            "POST", "/api/leads", body={"id": agent_id, "cwd": str(self.backend.cwd)},
            headers=self._common_headers(),
        )
        self.assertEqual(created.status, 200, created.body[:1000])
        item_id = f"{agent_id}:branch-message"
        database_path = self.backend.state / "canvas.sqlite3"
        with sqlite3.connect(database_path) as database:
            agent_raw = database.execute(
                "SELECT record FROM runtime_agents WHERE id=?", (agent_id,),
            ).fetchone()[0]
            agent = json.loads(agent_raw)
            agent.update(threadId="source-thread", turnId=None, inFlight=False,
                         workspaceOperation=None)
            agent.pop("workspaceReservationId", None)
            database.execute(
                "UPDATE runtime_agents SET record=? WHERE id=?",
                (json.dumps(agent), agent_id),
            )
            item = {
                "id": item_id, "role": "user", "text": "Synthetic branch question",
                "turnId": "source-turn",
            }
            database.execute(
                "INSERT INTO runtime_items(id,agent,record,created) VALUES(?,?,?,?)",
                (item_id, agent_id, json.dumps(item), 1.0),
            )
            database.commit()
        return agent_id, item_id

    def test_list_import_threads_on_both_backends(self) -> None:
        before = self._agent_count()
        response = self.backend.request(
            "GET", "/api/import?account_key=default", headers=self._common_headers(),
        )
        self.assertEqual(response.status, 200, self._error_detail(response))
        self.assertEqual(
            response.json()["data"],
            [{"id": "source-thread", "title": "Fixture thread"}],
        )
        self.assertEqual(self._agent_count(), before)

    def test_import_thread_on_both_backends(self) -> None:
        before = self._agent_count()
        body = {
            "id": "8d7ddc2a-e52e-4ddb-8c5f-69dd08b45545",
            "threadId": "source-thread",
            "prompt": "Continue the synthetic fixture.",
        }
        response = self.backend.request(
            "POST",
            "/api/import",
            body=body,
            headers=self._common_headers(),
        )
        self.assertEqual(response.status, 200, self._error_detail(response))
        agent_id = response.json()["id"]
        with sqlite3.connect(self.backend.state / "canvas.sqlite3") as database:
            record = database.execute(
                "SELECT record FROM runtime_agents WHERE id=?", (agent_id,),
            ).fetchone()
        self.assertIsNotNone(record)
        self.assertEqual(json.loads(record[0])["importedFrom"], "source-thread")
        self.assertEqual(self._agent_count(), before + 1)
        replay = self.backend.request(
            "POST", "/api/import", body=body, headers=self._common_headers(),
        )
        self.assertEqual(replay.status, 200, replay.body[:1000])
        self.assertEqual(replay.json()["id"], agent_id)
        self.assertEqual(self._agent_count(), before + 1)

    def test_branch_copies_history_and_replays_completed_request(self) -> None:
        agent_id, message_id = self._seed_branch_source()
        body = {"agent": agent_id, "id": str(uuid.uuid4()), "message_id": message_id}
        before = self._agent_count()
        response = self.backend.request(
            "POST", "/api/branch", body=body, headers=self._common_headers(),
        )
        self.assertEqual(response.status, 200, self._error_detail(response))
        branch_id = response.json()["id"]
        self.assertEqual(response.json()["threadId"], "branch-thread")
        self.assertEqual(self._agent_count(), before + 1)
        database_path = self.backend.state / "canvas.sqlite3"
        with sqlite3.connect(database_path) as database:
            child_items = database.execute(
                "SELECT record FROM runtime_items WHERE agent=?", (branch_id,),
            ).fetchall()
            operation = database.execute(
                "SELECT record FROM runtime_workspace_operations WHERE id=?",
                (f"branch:{body['id']}",),
            ).fetchone()
            child = database.execute(
                "SELECT record FROM runtime_agents WHERE id=?", (branch_id,),
            ).fetchone()
        self.assertEqual(len(child_items), 1)
        self.assertEqual(json.loads(child_items[0][0])["text"], "Synthetic branch question")
        self.assertEqual(json.loads(operation[0])["phase"], "completed")
        self.assertEqual(json.loads(child[0])["forkedFrom"], agent_id)
        self.assertEqual(json.loads(child[0])["sourceMessage"], message_id)
        replay = self.backend.request(
            "POST", "/api/branch", body=body, headers=self._common_headers(),
        )
        self.assertEqual(replay.status, 200, replay.body[:1000])
        self.assertEqual(replay.json()["id"], branch_id)
        self.assertEqual(self._agent_count(), before + 1)


if __name__ == "__main__":
    unittest.main()
