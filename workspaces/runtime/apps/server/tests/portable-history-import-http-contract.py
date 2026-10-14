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
            cls.prewarm_agent_id = agent_id
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
        frames_before_changed_replay = self.backend.received_provider_frames()
        changed = {**body, "threadId": "different-source-thread"}
        rejected = self.backend.request(
            "POST", "/api/import", body=changed, headers=self._common_headers(),
        )
        self.assertNotEqual(rejected.status, 200, self._error_detail(rejected))
        self.assertEqual(self._agent_count(), before + 1)
        self.assertEqual(
            self.backend.received_provider_frames(), frames_before_changed_replay,
        )

    def test_import_metadata_and_initial_event_commit_atomically(self) -> None:
        if not os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"):
            self.skipTest("The Rust import route owns this atomic creation boundary")
        before = self._agent_count()
        agent_id = str(uuid.uuid4())
        body = {
            "id": agent_id,
            "threadId": "source-thread",
            "prompt": "Recover the same import after a failed durable write.",
        }
        database_path = self.backend.state / "canvas.sqlite3"
        with sqlite3.connect(database_path) as database:
            database.execute(
                "CREATE TRIGGER fail_import_initial BEFORE INSERT ON runtime_events "
                f"WHEN NEW.id='{agent_id}:initial' BEGIN "
                "SELECT RAISE(ABORT, 'injected import event failure'); END"
            )
            database.commit()
        failed = self.backend.request(
            "POST", "/api/import", body=body, headers=self._common_headers(),
        )
        self.assertNotEqual(failed.status, 200, self._error_detail(failed))
        with sqlite3.connect(database_path) as database:
            self.assertIsNone(database.execute(
                "SELECT 1 FROM runtime_agents WHERE id=?", (agent_id,),
            ).fetchone())
            self.assertIsNone(database.execute(
                "SELECT 1 FROM runtime_events WHERE id=?", (f"{agent_id}:initial",),
            ).fetchone())
            database.execute("DROP TRIGGER fail_import_initial")
            database.commit()

        before_retry = self.backend.received_provider_frames()
        retry = self.backend.request(
            "POST", "/api/import", body=body, headers=self._common_headers(),
        )
        self.assertEqual(retry.status, 200, self._error_detail(retry))
        with sqlite3.connect(database_path) as database:
            record = json.loads(database.execute(
                "SELECT record FROM runtime_agents WHERE id=?", (agent_id,),
            ).fetchone()[0])
            event_count = database.execute(
                "SELECT COUNT(*) FROM runtime_events WHERE id=?", (f"{agent_id}:initial",),
            ).fetchone()[0]
        self.assertEqual(record["importedFrom"], "source-thread")
        self.assertTrue(record["autoWake"])
        self.assertEqual(event_count, 1)
        self.assertEqual(self._agent_count(), before + 1)
        after_retry = self.backend.received_provider_frames()
        self.assertEqual(
            [frame for frame in after_retry if frame.get("method") in {"thread/read", "thread/turns/list"}],
            [frame for frame in before_retry if frame.get("method") in {"thread/read", "thread/turns/list"}],
        )
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
        forks_before_changed_replay = sum(
            frame.get("method") == "thread/fork"
            for frame in self.backend.received_provider_frames()
        )
        changed = {**body, "before": True}
        rejected = self.backend.request(
            "POST", "/api/branch", body=changed, headers=self._common_headers(),
        )
        self.assertNotEqual(rejected.status, 200, self._error_detail(rejected))
        self.assertEqual(self._agent_count(), before + 1)
        forks_after_changed_replay = sum(
            frame.get("method") == "thread/fork"
            for frame in self.backend.received_provider_frames()
        )
        self.assertEqual(forks_after_changed_replay, forks_before_changed_replay)

    def test_branch_before_searches_older_turn_pages(self) -> None:
        if not os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"):
            self.skipTest("The Python backend reads turns from thread/read in one response")
        agent_id, message_id = self._seed_branch_source()
        body = {
            "agent": agent_id,
            "id": str(uuid.uuid4()),
            "message_id": message_id,
            "before": True,
        }
        response = self.backend.request(
            "POST", "/api/branch", body=body, headers=self._common_headers(),
        )
        self.assertEqual(response.status, 200, self._error_detail(response))
        frames = self.backend.received_provider_frames()
        turns = [frame for frame in frames if frame.get("method") == "thread/turns/list"]
        self.assertEqual(len(turns), 2)
        self.assertEqual(turns[0].get("params", {}).get("cursor"), None)
        self.assertEqual(turns[1].get("params", {}).get("cursor"), "older")
        fork = next(frame for frame in frames if frame.get("method") == "thread/fork")
        self.assertEqual(fork["params"]["lastTurnId"], "older-turn")

    def test_branch_provider_result_write_failure_is_not_replayed(self) -> None:
        if not os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"):
            self.skipTest("The Rust branch route owns durable provider-result recovery")
        agent_id, message_id = self._seed_branch_source()
        body = {"agent": agent_id, "id": str(uuid.uuid4()), "message_id": message_id}
        operation_id = f"branch:{body['id']}"
        prior_forks = sum(
            frame.get("method") == "thread/fork"
            for frame in self.backend.received_provider_frames()
        )
        database_path = self.backend.state / "canvas.sqlite3"
        with sqlite3.connect(database_path) as database:
            database.execute(
                "CREATE TRIGGER fail_branch_provider_result BEFORE UPDATE ON "
                "runtime_workspace_operations "
                "WHEN NEW.id='" + operation_id + "' "
                "AND json_extract(NEW.record,'$.phase')='provider_ready' BEGIN "
                "SELECT RAISE(ABORT, 'injected provider result persistence failure'); END"
            )
            database.commit()
        failed = self.backend.request(
            "POST", "/api/branch", body=body, headers=self._common_headers(),
        )
        self.assertNotEqual(failed.status, 200, self._error_detail(failed))
        with sqlite3.connect(database_path) as database:
            operation = json.loads(database.execute(
                "SELECT record FROM runtime_workspace_operations WHERE id=?", (operation_id,),
            ).fetchone()[0])
            database.execute("DROP TRIGGER fail_branch_provider_result")
            database.commit()
        self.assertEqual(operation["phase"], "recovery_required")
        forks_after_failure = sum(
            frame.get("method") == "thread/fork"
            for frame in self.backend.received_provider_frames()
        )
        self.assertEqual(forks_after_failure, prior_forks + 1)

        replay = self.backend.request(
            "POST", "/api/branch", body=body, headers=self._common_headers(),
        )
        self.assertNotEqual(replay.status, 200, self._error_detail(replay))
        forks_after_replay = sum(
            frame.get("method") == "thread/fork"
            for frame in self.backend.received_provider_frames()
        )
        self.assertEqual(forks_after_replay, forks_after_failure)

    def test_branch_reservation_failure_does_not_call_provider(self) -> None:
        if not os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"):
            self.skipTest("The Rust branch route owns workspace reservation")
        agent_id, message_id = self._seed_branch_source()
        body = {"agent": agent_id, "id": str(uuid.uuid4()), "message_id": message_id}
        operation_id = f"branch:{body['id']}"
        prior_forks = sum(
            frame.get("method") == "thread/fork"
            for frame in self.backend.received_provider_frames()
        )
        database_path = self.backend.state / "canvas.sqlite3"
        with sqlite3.connect(database_path) as database:
            database.execute(
                "CREATE TRIGGER fail_branch_reservation BEFORE INSERT ON "
                "runtime_workspace_operations WHEN NEW.id='" + operation_id + "' BEGIN "
                "SELECT RAISE(ABORT, 'injected branch reservation failure'); END"
            )
            database.commit()
        failed = self.backend.request(
            "POST", "/api/branch", body=body, headers=self._common_headers(),
        )
        self.assertNotEqual(failed.status, 200, self._error_detail(failed))
        with sqlite3.connect(database_path) as database:
            self.assertIsNone(database.execute(
                "SELECT 1 FROM runtime_workspace_operations WHERE id=?", (operation_id,),
            ).fetchone())
            agent = json.loads(database.execute(
                "SELECT record FROM runtime_agents WHERE id=?", (agent_id,),
            ).fetchone()[0])
            database.execute("DROP TRIGGER fail_branch_reservation")
            database.commit()
        self.assertIsNone(agent.get("workspaceReservationId"))
        forks_after_failure = sum(
            frame.get("method") == "thread/fork"
            for frame in self.backend.received_provider_frames()
        )
        self.assertEqual(forks_after_failure, prior_forks)

    def test_branch_local_copy_failure_recovers_after_restart_without_refork(self) -> None:
        if not os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"):
            self.skipTest("The Rust branch route owns provider-ready recovery")
        agent_id, message_id = self._seed_branch_source()
        body = {"agent": agent_id, "id": str(uuid.uuid4()), "message_id": message_id}
        prior_fork_count = sum(
            frame.get("method") == "thread/fork"
            for frame in self.backend.received_provider_frames()
        )
        prior_provider_starts = len(self.backend.provider_start_times())
        database_path = self.backend.state / "canvas.sqlite3"
        with sqlite3.connect(database_path) as database:
            database.execute(
                "CREATE TRIGGER fail_branch_copy BEFORE INSERT ON runtime_items "
                f"WHEN NEW.agent<>'{agent_id}' BEGIN "
                "SELECT RAISE(ABORT, 'injected branch transcript copy failure'); END"
            )
            database.commit()
        failed = self.backend.request(
            "POST", "/api/branch", body=body, headers=self._common_headers(),
        )
        self.assertNotEqual(failed.status, 200, self._error_detail(failed))
        operation_id = f"branch:{body['id']}"
        with sqlite3.connect(database_path) as database:
            operation = json.loads(database.execute(
                "SELECT record FROM runtime_workspace_operations WHERE id=?", (operation_id,),
            ).fetchone()[0])
            database.execute("DROP TRIGGER fail_branch_copy")
            database.commit()
        self.assertEqual(operation["phase"], "provider_ready")
        forks_before_restart = [
            frame for frame in self.backend.received_provider_frames()
            if frame.get("method") == "thread/fork"
        ]
        self.assertEqual(len(forks_before_restart), prior_fork_count + 1)

        self.backend.stop()
        with sqlite3.connect(database_path) as database:
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
            raw = database.execute(
                "SELECT record FROM runtime_agents WHERE id=?", (self.prewarm_agent_id,),
            ).fetchone()[0]
            prewarm = json.loads(raw)
            prewarm.update(status="queued", autoWake=True, inFlight=False)
            database.execute(
                "UPDATE runtime_agents SET record=? WHERE id=?",
                (json.dumps(prewarm), self.prewarm_agent_id),
            )
            database.execute(
                "UPDATE runtime_events SET status='pending' WHERE id=?",
                (f"{self.prewarm_agent_id}:initial",),
            )
            database.commit()
        self.backend.start()
        self.backend.wait_until(
            lambda: len(self.backend.provider_start_times()) > prior_provider_starts,
            "provider session after backend restart",
        )
        replay = self.backend.request(
            "POST", "/api/branch", body=body, headers=self._common_headers(),
        )
        self.assertEqual(replay.status, 200, self._error_detail(replay))
        forks_after_restart = [
            frame for frame in self.backend.received_provider_frames()
            if frame.get("method") == "thread/fork"
        ]
        self.assertEqual(forks_after_restart, forks_before_restart)
        with sqlite3.connect(database_path) as database:
            final = json.loads(database.execute(
                "SELECT record FROM runtime_workspace_operations WHERE id=?", (operation_id,),
            ).fetchone()[0])
            child = database.execute(
                "SELECT id FROM runtime_agents WHERE json_extract(record,'$.forkedFrom')=?",
                (agent_id,),
            ).fetchall()
        self.assertEqual(final["phase"], "completed")
        self.assertEqual(len(child), 1)


if __name__ == "__main__":
    unittest.main()
