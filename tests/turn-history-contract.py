#!/usr/bin/env python3
"""Terminal outcomes and historical patches use the actual notification path."""
import importlib.util
import json
from pathlib import Path
import unittest
spec = importlib.util.spec_from_file_location("fixture", Path(__file__).with_name("workspace-contract.py"))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)
class TurnHistoryContract(unittest.TestCase):
    setUp = f.WorkspaceContract.setUp
    tearDown = f.WorkspaceContract.tearDown
    lead = f.WorkspaceContract.lead
    start = f.WorkspaceContract.start
    def notify(self, actor, method, **params):
        self.runtime.notification({"method": method, "params": {"threadId": actor["threadId"], "turnId": actor["turnId"], **params}})
    def test_final_phase_and_failed_outcome_are_preserved(self):
        actor = self.start(self.lead())
        self.notify(actor, "item/started", item={"id": "answer", "type": "agentMessage", "phase": "final_answer", "text": ""})
        self.notify(actor, "item/agentMessage/delta", itemId="answer", delta="Partial answer")
        self.assertEqual(self.runtime.transcript(actor["id"])["items"][-1]["phase"], "final_answer")
        self.notify(actor, "item/completed", item={"id": "answer", "type": "agentMessage", "phase": "final_answer", "text": "Check failed"})
        self.notify(actor, "turn/completed", turn={"id": actor["turnId"], "status": "failed", "error": {"message": "Failed"}})
        answer = next(item for item in self.runtime.transcript(actor["id"])["items"] if item["id"].endswith(":answer"))
        self.assertEqual((answer["phase"],answer["turnStatus"]), ("final_answer","failed"))
    def test_distinct_turns_preserve_patches_and_latest_changes(self):
        actor = self.start(self.lead())
        first = actor["turnId"]
        for number in (1,2):
            diff = f"diff --git a/file.txt b/file.txt\n--- a/file.txt\n+++ b/file.txt\n@@ -0,0 +1 @@\n+version {number}\n"
            self.notify(actor, "turn/diff/updated", diff=diff)
            self.notify(actor, "turn/completed", turn={"id": actor["turnId"], "status": "completed"})
            if number == 1: actor = self.start(actor, "Next task")
        rows = [item for item in self.runtime.transcript(actor["id"])["items"] if item.get("title") == "Changes"]
        self.assertEqual(len(rows), 2)
        self.assertEqual(rows[0]["turnId"], first)
        self.assertIn("version 1",json.loads(rows[0]["text"])["diff"])
        self.assertIn("version 2",self.runtime.reported_changes(actor["id"])["diff"])
    def test_old_completed_record_does_not_claim_success(self):
        actor = self.lead()
        with self.runtime.lock,self.runtime.db() as db:
            self.runtime.item(db, actor["id"], "old", "assistant", "Old report", turnId="old-turn")
            db.execute("INSERT INTO runtime_completed_turns VALUES (?)", (actor["id"]+":old-turn",))
        self.assertEqual(self.runtime.transcript(actor["id"])["items"][0]["turnStatus"], "ended")

    def test_historical_error_arrives_with_transcript_without_extra_rows(self):
        actor = self.start(self.lead())
        error = {'message': 'Usage limit reached', 'codexErrorInfo': 'usageLimitExceeded'}
        self.notify(actor, 'item/completed', item={'id': 'command', 'type': 'commandExecution', 'command': 'true', 'status': 'completed', 'exitCode': 0})
        self.notify(actor, 'turn/completed', turn={'id': actor['turnId'], 'status': 'failed', 'error': error})
        with self.runtime.db() as db:
            # Reproduce an older transcript with the native error only in analytics.
            db.execute("DELETE FROM runtime_items WHERE agent=? AND json_extract(record,'$.nativeNotice')='error'", (actor['id'],))
            count = db.execute('SELECT count(*) FROM runtime_items').fetchone()[0]
        transcript = self.runtime.transcript(actor['id'])
        command = next(item for item in transcript['items'] if item['id'].endswith(':command'))
        self.assertEqual(command['turnError'], error)
        self.assertTrue(command['turnErrorResolved'])
        with self.runtime.db() as db:
            self.assertEqual(db.execute('SELECT count(*) FROM runtime_items').fetchone()[0], count)
if __name__ == "__main__": unittest.main()
