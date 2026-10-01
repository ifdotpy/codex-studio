#!/usr/bin/env python3
"""Reopen private states written by two historical runtimes and finish upgrades."""
from __future__ import annotations

from contextlib import ExitStack
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import tarfile
import tempfile
import textwrap
import threading
import time
from urllib.parse import urlencode
import urllib.request
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
from codex_canvas import Canvas, make_server
from codex_runtime import Runtime
import codex_analytics_storage
import codex_payload_migrate
import codex_work
from codex_payloads import resolve_record, resolve_result
from codex_sync_entities import ENTITY_TOMBSTONE_LIMIT


# Last main snapshots from 24 and 25 September 2026. Both predate bounded
# entity sync; 24 September is the oldest selected input.
LEGACY_REVISIONS = ("03efa7a4fa0cfee12b949fd6718e4cac5d639a40",
                    "db16e483ea4a9d191cd445d5f511a285c658ed01")


def seed_with_revision(revision: str, state: Path) -> None:
    source = state.parent / ("source-" + revision[:8])
    source.mkdir()
    archive = subprocess.run(["git", "-C", str(ROOT), "archive", revision, "scripts"],
                             check=True, capture_output=True)
    with tarfile.open(fileobj=__import__("io").BytesIO(archive.stdout)) as tar:
        tar.extractall(source)
    seed = textwrap.dedent(r"""
        import json, sqlite3, sys, time
        from pathlib import Path
        sys.path.insert(0, str(Path(sys.argv[1]) / 'scripts'))
        from codex_runtime import Runtime
        class IdleServer:
            def __init__(self, *args, **kwargs): pass
            def close(self): pass
        root = Path(sys.argv[2])
        runtime = Runtime(root, IdleServer)
        try:
            with runtime.lock, runtime.db() as db:
                agent = runtime.create({'name':'Legacy upgrade lead', 'cwd':str(root),
                                        'prompt':'Preserve this old task', 'kind':'lead'}, defer=True)
                agent.update(status='waiting', isLead=True, rootId=agent['id'])
                runtime.put(db, 'agents', agent)
                body = 'legacy transcript body legacymarker ' + ('completebody ' * 2400)
                runtime.item(db, agent['id'], 'long-body', 'assistant', body, 'Legacy report')
                runtime.item(db, agent['id'], 'short-body', 'user', 'Keep this transcript entry')
                task = {'id':'legacy-task', 'agent':agent['id'], 'rootId':agent['id'],
                        'owner':agent['id'], 'kind':'command', 'status':'completed',
                        'created':time.time()-86400, 'tail':'legacy tool output'}
                runtime.put(db, 'tasks', task)
                checkpoint = {'id':'legacy-checkpoint', 'agent':agent['id'],
                              'items':['checkpoint-' + 'c'*90000]}
                db.execute('INSERT OR REPLACE INTO runtime_checkpoints(id,record) VALUES (?,?)',
                           (checkpoint['id'], json.dumps(checkpoint)))
                request = {'id':'legacy-tool-request', 'agent':agent['id'],
                           'result':{'text':'request-'+'r'*90000}}
                db.execute('INSERT OR REPLACE INTO runtime_tool_requests(id,record) VALUES (?,?)',
                           (request['id'], json.dumps(request)))
                db.execute('INSERT OR REPLACE INTO runtime_tool_results(id,result) VALUES (?,?)',
                           ('legacy-tool-result', json.dumps({'text':'result-'+'z'*90000})))
                # Older search indexes retain the complete body for truncated rows.
                db.execute('UPDATE runtime_search SET body=? WHERE id=?',
                           (body, agent['id'] + ':long-body'))
                usage = db.execute("SELECT name FROM sqlite_master WHERE type='table' AND name='analytics_usage'").fetchone()
                if usage:
                    cols = {row[1] for row in db.execute('PRAGMA table_info(analytics_usage)')}
                    values = {'id':'legacy-usage', 'agent':agent['id'], 'root':agent['id'],
                              'thread':'legacy-thread', 'turn':'legacy-turn', 'at':time.time(),
                              'record':json.dumps({'inputTokens':17, 'outputTokens':23, 'totalTokens':40})}
                    selected = [key for key in values if key in cols]
                    db.execute('INSERT OR REPLACE INTO analytics_usage(' + ','.join(selected) + ') VALUES (' +
                               ','.join('?' for _ in selected) + ')', [values[key] for key in selected])
                print(json.dumps({'agent':agent['id'], 'body':body}))
        finally:
            runtime.close()
    """)
    subprocess.run([sys.executable, "-B", "-c", seed, str(source), str(state)],
                   check=True, cwd=ROOT, capture_output=True, text=True,
                   env={**os.environ, "PYTHONDONTWRITEBYTECODE": "1"})


class IdleServer:
    def __init__(self, *args, **kwargs):
        pass

    def close(self):
        pass


class UpgradeContract(unittest.TestCase):
    def test_old_state_upgrade_preserves_records_and_serves_http(self):
        for revision in LEGACY_REVISIONS:
            with self.subTest(revision=revision), tempfile.TemporaryDirectory() as tmp:
                state = Path(tmp) / "state"
                state.mkdir()
                seed_with_revision(revision, state)
                canvas_db = sqlite3.connect(state / "canvas.sqlite3")
                try:
                    agent_id = json.loads(canvas_db.execute(
                        "SELECT record FROM runtime_agents ORDER BY rowid LIMIT 1").fetchone()[0])["id"]
                    baseline_body = canvas_db.execute(
                        "SELECT body FROM runtime_search WHERE id=?", (agent_id + ":long-body",)).fetchone()[0]
                    before_search = {row[0] for row in canvas_db.execute(
                        "SELECT id FROM runtime_search WHERE runtime_search MATCH 'legacymarker'")}
                    before_usage = canvas_db.execute("SELECT count(*),sum(json_extract(record,'$.totalTokens')) "
                                                     "FROM analytics_usage").fetchone()
                    before_task = canvas_db.execute("SELECT record FROM runtime_tasks WHERE id='legacy-task'").fetchone()[0]
                    before_rows = {name: canvas_db.execute(f"SELECT count(*) FROM {name}").fetchone()[0]
                                   for name in ("runtime_checkpoints", "runtime_tool_requests", "runtime_tool_results")}
                finally:
                    canvas_db.close()

                runtime = Runtime(state, IdleServer)
                canvas = Canvas(state)
                canvas.runtime = runtime
                server = make_server(canvas, port=0)
                thread = threading.Thread(target=server.serve_forever, daemon=True)
                thread.start()
                try:
                    # Deterministically model a spacious volume. The database and
                    # blob fixture are tiny; no large files are allocated.
                    class Usage:
                        free = 128 * 1024**3
                    with ExitStack() as stack:
                        stack.enter_context(patch.object(codex_work.shutil, "disk_usage", return_value=Usage()))
                        stack.enter_context(patch.object(codex_analytics_storage.shutil, "disk_usage", return_value=Usage()))
                        stack.enter_context(patch.object(codex_payload_migrate.shutil, "disk_usage", return_value=Usage()))
                        runtime.search_migration_start()
                        codex_analytics_storage.start(runtime)
                        codex_payload_migrate.run(
                            state, tables=list(codex_payload_migrate.TARGETS), batch_rows=8,
                            batch_bytes=128 * 1024, max_batches=None)
                        deadline = time.monotonic() + 30
                        while time.monotonic() < deadline:
                            with runtime.db() as db:
                                phase = db.execute("SELECT phase FROM runtime_search_rollout WHERE id=1").fetchone()[0]
                            analytics_status = getattr(runtime, "analytics_migration_status", {}).get("status")
                            if phase == "complete" and analytics_status == "complete":
                                break
                            time.sleep(.02)
                        self.assertEqual(phase, "complete")
                        self.assertEqual(analytics_status, "complete")
                    with sqlite3.connect(state / "canvas.sqlite3") as db:
                        payload_states = dict(db.execute(
                            "SELECT name,complete FROM runtime_payload_migrations"))
                    self.assertTrue(all(payload_states.get("payload-v1:" + name) == 1
                                        for name in codex_payload_migrate.TARGETS))
                    origin = f"http://127.0.0.1:{server.server_address[1]}"
                    def get(path):
                        with urllib.request.urlopen(origin + path, timeout=5) as response:
                            return json.load(response)
                    state_payload = get("/api/state")
                    self.assertTrue(any(item.get("id") == agent_id
                                        for item in state_payload.get("threads", [])))
                    identity = get("/api/sync/identity")
                    self.assertTrue(identity["workspaceId"])
                    entity_pull = get("/api/sync/pull?scope=state%3Aentities%3Av1&after=0&limit=100")
                    self.assertTrue(entity_pull["documents"])
                    floor = max(2, entity_pull["maxSeq"])
                    with runtime.db() as db:
                        db.execute("INSERT INTO sync_entity_meta(key,value) VALUES "
                                   "('entity_tombstone_floor',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                                   (str(floor),))
                    reset = get("/api/sync/pull?" + urlencode(
                        {"scope": "state:entities:v1", "after": 1, "reset": 1}))
                    self.assertTrue(reset["reset"])
                    self.assertEqual(reset["floor"], floor)
                    transcript = get("/api/transcript/page?" + urlencode({"id": agent_id, "limit": 20}))
                    self.assertTrue(any(item.get("id") == agent_id + ":long-body"
                                        and item.get("truncated") for item in transcript["items"]))
                    full_item = get("/api/transcript/item?" + urlencode(
                        {"id": agent_id, "message_id": agent_id + ":long-body"}))
                    self.assertEqual(full_item["text"], baseline_body)
                    diagnostics = get("/api/diagnostics")
                    self.assertEqual(diagnostics["migrations"]["search"]["phase"], "complete")
                    self.assertIsNone(diagnostics["searchMigrationError"])
                    self.assertEqual(diagnostics["migrations"]["analyticsFile"]["status"], "complete")
                    tombstones = diagnostics["migrations"]["entityTombstones"]
                    self.assertLessEqual(tombstones["count"], ENTITY_TOMBSTONE_LIMIT)
                    self.assertEqual(tombstones["pruning"]["status"], "complete")
                    self.assertTrue(all(value["status"] == "complete"
                                        for value in diagnostics["migrations"]["payloads"].values()))
                    runtime.search_migration_error = "fixture migration failure"
                    failure_diagnostics = get("/api/diagnostics")
                    self.assertEqual(failure_diagnostics["searchMigrationError"], "fixture migration failure")
                    self.assertEqual(failure_diagnostics["migrations"]["search"]["error"],
                                     "fixture migration failure")
                    runtime.search_migration_error = None
                    upgrade_check = json.loads(subprocess.run(
                        [str(ROOT / "scripts/codex-upgrade-check"), "--repo", str(ROOT),
                         "--state-dir", str(state), "--diagnostics-url", origin + "/api/diagnostics"],
                        check=True, capture_output=True, text=True, cwd=ROOT).stdout)
                    self.assertTrue(upgrade_check["prechecks"]["stateDatabase"]["readable"])
                    self.assertEqual(upgrade_check["runningAgentCount"], 1)
                    self.assertEqual(upgrade_check["migrations"]["search"]["phase"], "complete")
                    with runtime.db() as db:
                        new_search = {row[0] for row in db.execute(
                            "SELECT m.id FROM runtime_search_next JOIN runtime_search_next_meta m "
                            "ON m.search_rowid=runtime_search_next.rowid "
                            "WHERE runtime_search_next MATCH 'legacymarker'")}
                        fulltext = db.execute("SELECT body FROM runtime_item_fulltext WHERE id=?",
                                              (agent_id + ":long-body",)).fetchone()[0]
                        payload = json.loads(db.execute(
                            "SELECT record FROM runtime_checkpoints WHERE id='legacy-checkpoint'").fetchone()[0])
                        request = json.loads(db.execute(
                            "SELECT record FROM runtime_tool_requests WHERE id='legacy-tool-request'").fetchone()[0])
                        result = db.execute("SELECT result FROM runtime_tool_results WHERE id='legacy-tool-result'").fetchone()[0]
                    self.assertEqual(new_search, before_search)
                    api_search = get("/api/search?" + urlencode({"q": "legacymarker", "limit": 20}))
                    self.assertTrue(any(item.get("id") == agent_id + ":long-body"
                                         for item in api_search.get("results", [])))
                    self.assertEqual(fulltext, baseline_body)
                    self.assertGreater(len(fulltext), 20000)
                    self.assertEqual(len(resolve_record(state, payload)["items"][0]), 90000 + len("checkpoint-"))
                    self.assertEqual(len(resolve_record(state, request)["result"]["text"]),
                                     90000 + len("request-"))
                    self.assertEqual(len(resolve_result(state, result)["text"]), 90000 + len("result-"))
                    with sqlite3.connect(state / "analytics.sqlite3") as analytics:
                        after_usage = analytics.execute("SELECT count(*),sum(json_extract(record,'$.totalTokens')) "
                                                        "FROM analytics_usage").fetchone()
                    self.assertEqual(tuple(after_usage), tuple(before_usage))
                    self.assertIn("entityTombstones", diagnostics["migrations"])
                    with sqlite3.connect(state / "canvas.sqlite3") as db:
                        self.assertEqual({name: db.execute(f"SELECT count(*) FROM {name}").fetchone()[0]
                                          for name in before_rows}, before_rows)
                        self.assertEqual(db.execute(
                            "SELECT record FROM runtime_tasks WHERE id='legacy-task'").fetchone()[0], before_task)
                finally:
                    server.shutdown()
                    server.server_close()
                    runtime.close()


if __name__ == "__main__":
    unittest.main()
