from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import json
import re
import unittest
from urllib.parse import urlencode
from urllib.request import urlopen

import codex_budget


def _json(url):
    with urlopen(url, timeout=10) as response:
        return response.status, json.loads(response.read())


class LiveSyncPatchHTTPContract(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        import __main__
        cls.server = __main__._studio_lp_server
        cls.runtime = __main__._studio_lp_runtime
        cls.origin = f"http://127.0.0.1:{cls.server.server_address[1]}"

    def pull(self, scope, after=0, limit=100):
        query = urlencode({"scope": scope, "after": after, "limit": limit})
        return _json(self.origin + "/api/sync/pull?" + query)

    def test_01_entity_pages_and_legacy_scopes(self):
        after = 0
        documents = []
        for _ in range(20):
            status, result = self.pull("state:entities:v1", after)
            self.assertEqual(status, 200)
            batch = result["documents"]
            documents.extend(batch)
            after = result["checkpoint"]["seq"]
            if after >= result["maxSeq"] or not batch:
                break
        self.assertTrue(documents)
        self.assertTrue(all(row["id"].startswith("entity:") for row in documents))
        self.assertGreaterEqual(after, result["maxSeq"])
        for scope in ("state", "state:chat"):
            status, legacy = self.pull(scope)
            self.assertEqual(status, 400)
            self.assertEqual(legacy, {"error": "Invalid sync scope"})

    def test_02_numeric_stream_after_runtime_write(self):
        status, before = self.pull("state:entities:v1")
        self.assertEqual(status, 200)
        with self.runtime.db() as db:
            row = db.execute(
                "SELECT record FROM runtime_agents WHERE id='lp-budget-probe'",
            ).fetchone()
            agent = json.loads(row[0])
            agent["status"] = "running"
            self.runtime.put(db, "agents", agent)
        status, changed = self.pull("state:entities:v1", before["maxSeq"])
        self.assertEqual(status, 200)
        self.assertTrue(changed["documents"])
        with urlopen(
            self.origin + "/api/sync/stream?scope=state%3Aentities%3Av1", timeout=8,
        ) as response:
            line = response.readline().decode().strip()
        self.assertRegex(line, r"^data: [0-9]+$")

    def test_03_lagging_transcript_cursor_has_hash_only_rows(self):
        agent_id = "lp-budget-probe"
        status, base = self.pull("transcript:" + agent_id)
        self.assertEqual(status, 200)
        cursor = base["checkpoint"]["seq"]
        with self.runtime.db() as db:
            self.runtime.item(db, agent_id, "later-a", "assistant", "later item A")
            self.runtime.item(db, agent_id, "later-b", "assistant", "later item B")
        status, delta = self.pull("transcript:" + agent_id, cursor)
        self.assertEqual(status, 200)
        self.assertTrue(delta["documents"])
        transcript_delta = json.loads(delta["documents"][0]["payload"])
        self.assertTrue(transcript_delta["delta"])
        self.assertEqual([item["text"] for item in transcript_delta["items"]],
                         ["later item A", "later item B"])
        with self.runtime.db() as db:
            self.assertTrue(all(row[0] is None for row in db.execute(
                "SELECT payload FROM sync_entities WHERE collection=?",
                ("transcript:" + agent_id,),
            )))

    def test_04_budget_save_registers_old_reused_connection(self):
        runtime = self.runtime
        old_connection = runtime._studio_lp_prepatch_connection
        local = runtime.__dict__["_callback_db"]
        self.assertIs(local.connection, old_connection)
        agent_id = "lp-budget-probe"
        state = {
            "spent": 11, "floor": 0, "after": 11, "before": 0,
            "noticeSpent": 0, "historicalNotices": 0,
            "faults": [], "ambiguousNotices": [],
        }
        with runtime.db() as db:
            self.assertIs(db, old_connection)
            codex_budget.budget_init(db)
            record = db.execute(
                "SELECT record FROM runtime_agents WHERE id=?", (agent_id,),
            ).fetchone()
            codex_budget._save(db, json.loads(record[0]), state)
            db.execute(
                "INSERT INTO runtime_events VALUES (?,?,?,?,?,?,?,?,?)",
                ("lp-event-probe", agent_id, "agent_message", "probe", "pending", 1.0, 0, None, None),
            )
            self.assertEqual(
                db.execute("SELECT json_extract(record,'$.tokensUsed') FROM runtime_agents WHERE id=?",
                           (agent_id,)).fetchone()[0],
                11,
            )
            event = db.execute(
                "SELECT payload FROM sync_entities WHERE collection='event' AND id='lp-event-probe'",
            ).fetchone()
            self.assertIsNotNone(event)
        self.assertIsNone(old_connection.execute(
            "SELECT sync_invalidate_agent(?)", (agent_id,),
        ).fetchone()[0])


if __name__ == "__main__":
    unittest.main()
