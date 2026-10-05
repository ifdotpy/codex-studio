#!/usr/bin/env python3
"""Native rollout parsing, profile boundaries, and resumable import contracts."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import contextlib
import hashlib
import json
import os
from pathlib import Path
import sqlite3
import sys
import tempfile
import threading
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
from analytics.rollout_parser import rollout_actions
import codex_analytics_history as history
from codex_analytics_history import AnalyticsHistoryMixin, inherited_usage_threads

THREAD = "01a07781-5d19-7390-bc74-c12094143962"
OTHER_THREAD = "01a07781-5d19-7390-bc74-c12094143963"


def line(kind, payload):
    return json.dumps({"type": kind, "timestamp": "2026-09-06T18:00:00Z", "payload": payload}).encode() + b"\n"


class Fixture(AnalyticsHistoryMixin):
    def __init__(self, root, account="first"):
        self.root = Path(root)
        self.lock = threading.RLock()
        self.closed = False
        self.homes = {"first": self.root / "profile-one", "second": self.root / "profile-two"}
        for home in self.homes.values():
            (home / "sessions" / "2026").mkdir(parents=True, exist_ok=True)
        self.accounts = type("Accounts", (), {"home": lambda _, key: self.homes[key]})()
        self.path = self.homes[account] / "sessions" / "2026" / ("rollout-2026-09-06-" + THREAD + ".jsonl")
        with self.db() as db:
            db.execute("CREATE TABLE IF NOT EXISTS runtime_agents (id TEXT PRIMARY KEY, record TEXT)")
            db.execute("CREATE TABLE IF NOT EXISTS captured (kind TEXT, method TEXT, record TEXT)")
            db.execute("INSERT OR IGNORE INTO runtime_agents VALUES (?,?)", ("agent", json.dumps({
                "id": "agent", "accountKey": account, "threadId": THREAD, "model": "current-not-historical"})))
            self.analytics_history_init(db)

    @contextlib.contextmanager
    def db(self):
        db = sqlite3.connect(self.root / "fixture.sqlite3")
        try:
            with db:
                yield db
        finally:
            db.close()

    def records(self, db, table):
        return [json.loads(r[0]) for r in db.execute("SELECT record FROM runtime_" + table)]

    def agent(self, key, db):
        return json.loads(db.execute("SELECT record FROM runtime_agents WHERE id=?", (key,)).fetchone()[0])

    def analytics_event(self, db, a, method, p, *, at=None, source=None, measurements=None):
        db.execute("INSERT INTO captured VALUES (?,?,?)", ("event", method, json.dumps({"a": a, "p": p, "at": at, "source": source,
                                                                           "measurements": measurements})))

    def analytics_model_payload(self, db, a, p, *, at=None, turn_id=None, source=None, measurements=None):
        db.execute("INSERT INTO captured VALUES (?,?,?)", ("payload", p.get("type"), json.dumps({"a": a, "p": p, "at": at, "turnId": turn_id,
                                                                                         "source": source, "measurements": measurements})))

    def state(self):
        with self.db() as db:
            row = db.execute("SELECT record FROM analytics_history").fetchone()
        return json.loads(row[0]) if row else None

    def captured(self):
        with self.db() as db:
            return [(r[0], r[1], json.loads(r[2])) for r in db.execute("SELECT * FROM captured")]


class InheritedUsageTests(unittest.TestCase):
    def ancestry_agent(self):
        return {'id':'agent', 'accountKey':'profile', 'threadId':'current-fork', 'contextRepair':{
            'agent':'agent', 'phase':'completed', 'newThreadId':'current-fork',
            'source':{'id':'agent','accountKey':'profile','threadId':THREAD},
            'snapshot':{'sourcePath':'/source.jsonl','sourceSha256':'a'*64,'sourceBytes':100,
                'copyPath':'/copy.jsonl','copySha256':'c'*64,'terminalTurnId':'terminal',
                'ancestry':[{'threadId':OTHER_THREAD,'path':'/ancestor.jsonl','sha256':'b'*64,
                             'endByteOffset':200,'endOrdinalExclusive':12},
                            {'threadId':THREAD,'path':'/source.jsonl','sha256':'a'*64,
                             'endByteOffset':100,'endOrdinalExclusive':None}]}}}

    def test_verified_materialized_ancestor_usage_keeps_exact_response_identity(self):
        agent = self.ancestry_agent()
        allowed = inherited_usage_threads(agent)
        self.assertEqual(allowed, sorted([THREAD, OTHER_THREAD]))
        context = {'threadId':agent['threadId'], 'allowedSourceThreadIds':allowed}
        raw = {'thread_id':OTHER_THREAD, 'turn_id':'old-turn', 'response_id':'original-charge',
               'usage':{'total_tokens':100}, 'thread_token_usage':{'total_tokens':100}}
        action = rollout_actions({'type':'token_usage_record','payload':raw}, context, 'id', 1)[0]
        self.assertEqual(action[0], 'event')
        self.assertEqual(action[2]['responseId'], 'original-charge')
        self.assertEqual(action[2]['rawTokenUsageRecord'], raw)

    def test_unverified_ancestry_never_expands_usage_scope(self):
        for defect in ('account','agent','phase','target','hash','copyHash','path','sourceSize',
                       'boundary','bytes','duplicate','missingField','nonUuid','terminal'):
            with self.subTest(defect=defect):
                agent = self.ancestry_agent()
                r = agent['contextRepair']; snapshot = r['snapshot']; ancestor = snapshot['ancestry'][0]
                if defect == 'account': r['source']['accountKey'] = 'other'
                elif defect == 'agent': r['agent'] = 'other'
                elif defect == 'phase': r['phase'] = 'unknown'
                elif defect == 'target': r['newThreadId'] = 'other'
                elif defect == 'hash': snapshot['sourceSha256'] = 'd'*64
                elif defect == 'copyHash': snapshot['copySha256'] = 'invalid'
                elif defect == 'path': snapshot['sourcePath'] = '/other.jsonl'
                elif defect == 'sourceSize': snapshot['sourceBytes'] = 99
                elif defect == 'boundary': ancestor['endOrdinalExclusive'] = None
                elif defect == 'bytes': ancestor['endByteOffset'] = 0
                elif defect == 'duplicate': snapshot['ancestry'].insert(0, dict(ancestor))
                elif defect == 'missingField': ancestor.pop('sha256')
                elif defect == 'nonUuid': ancestor['threadId'] = 'not-a-uuid'
                elif defect == 'terminal': snapshot.pop('terminalTurnId')
                allowed = inherited_usage_threads(agent)
                self.assertNotIn(OTHER_THREAD, allowed)
                context = {'threadId':agent['threadId'], 'allowedSourceThreadIds':allowed}
                raw = {'thread_id':OTHER_THREAD, 'response_id':'original-charge','usage':{'total_tokens':100}}
                self.assertEqual(rollout_actions({'type':'token_usage_record','payload':raw}, context,'id',1)[0][0], 'coverage')



class ImportTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.f = Fixture(self.temp.name)
        self.header = line("session_meta", {"id": THREAD})

    def tearDown(self):
        self.temp.cleanup()

    def test_payload_measurements_run_before_the_runtime_lock(self):
        self.f.path.write_bytes(self.header +
            line("response_item", {"type": "function_call_output", "call_id": "one", "output": "model output"}) +
            line("event_msg", {"type": "item_completed", "item": {"id": "two", "type": "commandExecution",
                "aggregatedOutput": "native output"}}))
        seen = []
        original_event = history.event_payload_measurements
        original_model = history.model_payload_measurements

        def event_measure(method, payload):
            self.assertFalse(self.f.lock._is_owned())
            seen.append("event")
            return original_event(method, payload)

        def model_measure(payload):
            self.assertFalse(self.f.lock._is_owned())
            seen.append("payload")
            return original_model(payload)

        with patch.object(history, "event_payload_measurements", event_measure), patch.object(
                history, "model_payload_measurements", model_measure):
            self.assertTrue(self.f.analytics_history_step())
        self.assertEqual(seen, ["payload", "event"])
        captured = self.f.captured()
        self.assertEqual(captured[0][2]["measurements"]["output"]["bytes"], 12)
        self.assertEqual(captured[1][2]["measurements"]["output"]["bytes"], 13)

    def test_idle_checkpoint_does_not_rewrite(self):
        self.f.path.write_bytes(self.header)
        self.assertTrue(self.f.analytics_history_step())
        with self.f.db() as db:
            before = db.execute('SELECT record FROM analytics_history').fetchone()[0]
        changes = []
        original = self.f._analytics_history_record
        def observe(db, *args):
            start = db.total_changes
            result = original(db, *args)
            changes.append(db.total_changes - start)
            return result
        self.f._analytics_history_record = observe
        self.assertFalse(self.f.analytics_history_step())
        self.assertEqual(changes, [0])
        with self.f.db() as db:
            self.assertEqual(db.execute('SELECT record FROM analytics_history').fetchone()[0], before)

    def test_bounded_import_resume_and_no_historical_model_guess(self):
        self.f.path.write_bytes(self.header + line("turn_context", {"turn_id": "past", "model": "past-model"}) +
                               line("response_item", {"type": "function_call", "call_id": "c", "arguments": "secret"}))
        self.assertTrue(self.f.analytics_history_step(max_records=1))
        self.assertEqual(self.f.state()["status"], "catchingUp")
        self.assertEqual(self.f.state()["importedRecords"], 1)
        other = Fixture(self.temp.name)
        other.analytics_history_step()
        self.assertEqual(other.state()["status"], "current")
        self.assertEqual(len(other.captured()), 1)
        self.assertEqual(other.captured()[0][2]["a"]["model"], "past-model")
        self.assertNotIn("secret", json.dumps(other.state()))
        self.assertFalse(other.analytics_history_step())
        self.assertEqual(len(other.captured()), 1)

    def test_deleted_agent_history_is_retained(self):
        self.f.path.write_bytes(self.header + line("response_item", {"type": "message", "role": "user", "content": []}))
        with self.f.db() as db:
            a = self.f.agent("agent", db)
            a["deletedAt"] = 1
            db.execute("UPDATE runtime_agents SET record=? WHERE id='agent'", (json.dumps(a),))
        self.f.analytics_history_step()
        self.assertEqual(self.f.state()["status"], "current")
        self.assertEqual(self.f.state()["deletedAt"], 1)
        self.assertEqual(len(self.f.captured()), 1)

    def test_agent_ids_are_selected_without_decoding_full_records(self):
        self.f.path.write_bytes(self.header)
        with self.f.db() as db:
            for key in ("second", "third"):
                db.execute("INSERT INTO runtime_agents VALUES (?,?)", (key, json.dumps(
                    {"id": key, "accountKey": "first", "threadId": "missing-" + key})))
        decoded, records = [], self.f.records
        self.f.records = lambda db, table: decoded.append(table) or records(db, table)
        agent = self.f.agent

        def current(key, db):
            if not db.execute("SELECT 1 FROM runtime_agents WHERE id=?", (key,)).fetchone():
                raise ValueError("Unknown managed agent")
            return agent(key, db)
        self.f.agent = current
        seen, save = [], self.f._analytics_history_save
        self.f._analytics_history_save = lambda key, a, state: seen.append(a["id"]) or save(key, a, state)
        self.f.analytics_history_step()
        self.f.analytics_history_step()
        # An agent removed during the round is skipped without a new decode.
        with self.f.db() as db:
            db.execute("DELETE FROM runtime_agents WHERE id='third'")
        self.assertFalse(self.f.analytics_history_step())
        self.assertEqual((decoded, seen), ([], ["second"]))
        self.f.analytics_history_step()
        self.assertEqual(decoded, [])
        self.f.analytics_history_step()
        self.assertEqual(seen, ["second", "second"])

    def test_partial_final_line_is_retried_only_when_complete(self):
        output = line("response_item", {"type": "function_call_output", "call_id": "c", "output": "answer"})
        self.f.path.write_bytes(self.header + output[:-5])
        self.f.analytics_history_step()
        self.assertEqual(self.f.state()["status"], "partialLine")
        self.assertEqual(self.f.state()["offset"], len(self.header))
        with self.f.path.open("ab") as handle:
            handle.write(output[-5:])
        self.f.analytics_history_step()
        self.assertEqual(self.f.state()["status"], "current")
        self.assertEqual(len(self.f.captured()), 1)

    def test_malformed_complete_line_marks_partial_coverage_and_advances(self):
        self.f.path.write_bytes(self.header + b"not-json\n" + line("response_item", {"type": "message", "role": "user", "content": []}))
        self.f.analytics_history_step()
        self.assertEqual(self.f.state()["malformedLines"], 1)
        self.assertEqual(self.f.state()["coverage"], "partial")
        self.assertEqual(self.f.state()["offset"], self.f.path.stat().st_size)
        self.assertEqual(len(self.f.captured()), 1)

    def test_file_replacement_retains_checkpoint(self):
        self.f.path.write_bytes(self.header)
        self.f.analytics_history_step()
        previous = self.f.state()["offset"]
        replacement = self.f.path.with_suffix(".new")
        replacement.write_bytes(self.header + b"{}\n")
        os.replace(replacement, self.f.path)
        self.f.analytics_history_step()
        self.assertEqual(self.f.state()["status"], "identityChanged")
        self.assertEqual(self.f.state()["offset"], previous)

    def set_history_state(self, state):
        with self.f.db() as db:
            db.execute("UPDATE analytics_history SET record=? WHERE id=?", (json.dumps(state), state["id"]))

    def test_device_renumber_preserves_offset_and_analytics_identity_across_restart(self):
        payload = line("response_item", {"type": "message", "role": "user", "content": []})
        self.f.path.write_bytes(self.header + payload)
        self.f.analytics_history_step()
        state = self.f.state()
        state["identity"][0] += 1
        state.pop("filesystemIdentity")
        state["status"] = "identityChanged"
        original_identity, previous_offset = list(state["identity"]), state["offset"]
        self.set_history_state(state)
        with self.f.path.open("ab") as stream:
            stream.write(payload)
        self.assertTrue(self.f.analytics_history_step())
        current = self.f.state()
        self.assertEqual(current["status"], "current")
        self.assertEqual(current["identity"], original_identity)
        self.assertEqual(current["filesystemRemap"]["offset"], previous_offset)
        self.assertEqual(len(self.f.captured()), 2)
        expected = hashlib.sha256((state["id"] + ":" + str(original_identity) + ":" + str(previous_offset)).encode()).hexdigest()
        self.assertEqual(self.f.captured()[-1][2]["p"]["_analyticsId"], expected)
        # A second remount and process restart preserve the original identity.
        current["filesystemIdentity"][0] += 2
        self.set_history_state(current)
        restarted = Fixture(self.temp.name)
        self.assertFalse(restarted.analytics_history_step())
        self.assertEqual(restarted.state()["identity"], original_identity)
        self.assertEqual(restarted.state()["filesystemRemapCount"], 2)
        self.assertEqual(len(restarted.captured()), 2)

    def test_device_remap_requires_all_checkpoint_evidence(self):
        for changed in ("inode", "path", "missingPath", "missingAnchor", "anchor", "header", "shorter"):
            with self.subTest(changed=changed):
                self.f.path.write_bytes(self.header + b"{}\n" * 100)
                with self.f.db() as db:
                    db.execute("DELETE FROM analytics_history")
                self.f.analytics_history_step()
                state = self.f.state()
                state["identity"][0] += 1
                state.pop("filesystemIdentity")
                original_identity, offset = list(state["identity"]), state["offset"]
                if changed == "inode":
                    state["identity"][1] += 1
                    original_identity = list(state["identity"])
                elif changed == "path":
                    state["path"] += ".different"
                elif changed == "missingPath":
                    state.pop("path")
                elif changed == "missingAnchor":
                    state.pop("anchor")
                elif changed == "anchor":
                    state["anchor"] = "changed"
                elif changed == "header":
                    self.f.path.write_bytes(self.f.path.read_bytes().replace(THREAD.encode(), OTHER_THREAD.encode()))
                else:
                    self.f.path.write_bytes(self.header)
                self.set_history_state(state)
                self.assertFalse(self.f.analytics_history_step())
                current = self.f.state()
                self.assertEqual(current["status"], "identityChanged")
                self.assertEqual(current["offset"], offset)
                self.assertEqual(current["identity"], original_identity)
                self.assertNotIn("filesystemRemap", current)

    def test_in_place_rewrite_is_detected_by_checkpoint_anchor(self):
        self.f.path.write_bytes(self.header)
        self.f.analytics_history_step()
        self.f.path.write_bytes(self.header.replace(b"session_meta", b"session_metb"))
        self.f.analytics_history_step()
        self.assertEqual(self.f.state()["status"], "identityChanged")

    def test_missing_managed_profile_does_not_search_other_account(self):
        alternate = self.f.homes["second"] / "sessions" / ("rollout-" + THREAD + ".jsonl")
        alternate.write_bytes(self.header)
        self.f.analytics_history_step()
        self.assertEqual(self.f.state()["status"], "missing")
        self.assertEqual(self.f.captured(), [])

    def test_wrong_header_is_rejected(self):
        self.f.path.write_bytes(line("session_meta", {"id": OTHER_THREAD}))
        self.f.analytics_history_step()
        self.assertEqual(self.f.state()["status"], "wrongThread")
        self.assertEqual(self.f.state()["offset"], 0)

    def test_symlink_outside_profile_is_rejected(self):
        outside = Path(self.temp.name) / "outside.jsonl"
        outside.write_bytes(self.header)
        self.f.path.symlink_to(outside)
        self.f.analytics_history_step()
        self.assertEqual(self.f.state()["status"], "outsideProfile")

    def test_ambiguous_thread_files_are_rejected(self):
        self.f.path.write_bytes(self.header)
        second = self.f.path.parent / ("duplicate-" + THREAD + ".jsonl")
        second.write_bytes(self.header)
        self.f.analytics_history_step()
        self.assertEqual(self.f.state()["status"], "ambiguous")

    def test_collector_failure_rolls_back_checkpoint_and_collected_events(self):
        self.f.path.write_bytes(self.header + line("response_item", {"type": "message", "role": "user", "content": []}))
        def fail(*args, **kwargs):
            raise ValueError("collector failure")
        original = self.f.analytics_model_payload
        self.f.analytics_model_payload = fail
        with self.assertRaises(ValueError):
            self.f.analytics_history_step()
        self.assertIsNone(self.f.state())
        self.f.analytics_model_payload = original
        self.f.analytics_history_step()
        self.assertEqual(len(self.f.captured()), 1)


if __name__ == "__main__":
    unittest.main()
