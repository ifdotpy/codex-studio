#!/usr/bin/env python3
"""Remove only proven Studio role suffixes; preserve user text and receipts."""
import copy
import importlib.util
import json
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location('context_fixture', Path(__file__).with_name('context-repair-contract.py'))
f = importlib.util.module_from_spec(spec); spec.loader.exec_module(f)
from codex_efficiency import digest


class RoleDedup(unittest.TestCase):
    setUp = f.ContextRepair.setUp
    tearDown = f.ContextRepair.tearDown
    write_records = f.ContextRepair.write_records
    forks = f.ContextRepair.forks
    agent_update = f.ContextRepair.agent_update
    lead = f.ContextRepair.lead

    def role_fixture(self):
        self.role = self.runtime.role_guidance(self.a)
        self.text = json.dumps({'id':'monitor-fixture', 'exitCode':0})
        with self.runtime.db() as db:
            db.execute('UPDATE runtime_events SET text=? WHERE id=?', (self.text, self.event['id']))
            db.execute('INSERT INTO runtime_event_meta VALUES (?,?)', (self.event['id'], json.dumps({
                'contextManifest':{'versions':{'roleSkill':digest(self.role)}}})))
        self.prefix = '[Orchestration event: monitor_exit]\n' + self.text
        message = self.records[2]['payload']
        message['content'][0]['text'] = self.prefix + '\n\n' + self.role
        self.records[4]['payload']['replacement_history'][0] = copy.deepcopy(message)
        self.write_records()

    def test_exact_role_copy_is_removed_and_current_role_is_in_fork_instructions(self):
        self.role_fixture()
        original = self.path.read_bytes()
        repaired = f.repair.repair_idle(self.runtime, self.a['id'])
        receipt = repaired['contextRepair']
        self.assertIn('studio-role:' + self.event['id'], receipt['snapshot']['eventIds'])
        clean = [json.loads(line) for line in Path(receipt['snapshot']['copyPath']).read_text().splitlines()]
        self.assertEqual(clean[2]['payload']['content'][0]['text'], self.prefix)
        self.assertEqual(clean[4]['payload']['replacement_history'][0]['content'][0]['text'], self.prefix)
        self.assertEqual(clean[4]['payload']['replacement_history'][1:], self.records[4]['payload']['replacement_history'][1:])
        self.assertIn(self.role, self.forks()[0]['developerInstructions'])
        self.assertEqual(self.path.read_bytes(), original)
        f.repair.repair_idle(self.runtime, self.a['id'])
        self.assertEqual(len(self.forks()), 1)

    def test_unproven_version_or_suffix_is_preserved(self):
        self.role_fixture()
        self.records[2]['payload']['content'][0]['text'] += '\nDo not remove this.'
        self.records[4]['payload']['replacement_history'][0] = copy.deepcopy(self.records[2]['payload'])
        self.write_records()
        result = f.repair.repair_idle(self.runtime, self.a['id'])
        self.assertEqual(result['contextRepair']['phase'], 'unchanged')
        self.assertEqual(self.forks(), [])
        with self.runtime.db() as db:
            db.execute('UPDATE runtime_event_meta SET record=? WHERE id=?', (json.dumps({
                'contextManifest':{'versions':{'roleSkill':'wrong-version'}}}), self.event['id']))
            actor = dict(self.a, contextRepair={})
            events = f.repair.verified_events(db, actor)
        report = f.repair.sanitized_rollout(self.path, self.home/'unproven.jsonl', self.tid, events)
        self.assertEqual(report['savedBytes'], 0)

    def test_mixed_user_turn_never_authorizes_role_removal(self):
        self.role_fixture()
        with self.runtime.db() as db:
            db.execute('INSERT INTO runtime_events VALUES (?,?,?,?,?,?,?,?,?)',
                       ('quoted',self.a['id'],'user',self.role,'delivered',2,self.a['epoch'],'turn',None))
            self.assertEqual(f.repair.verified_events(db,self.a), [])

    def test_prepared_developer_versions_survive_compaction_but_not_thread_change(self):
        params = self.runtime.new_thread_params(self.a)
        versions = self.runtime.preparation_context_versions(self.a, params)
        actor = {**self.a, 'compactions':4, 'preparedContext':{'epoch':[self.tid,0], 'versions':versions}}
        with self.runtime.db() as db:
            known = self.runtime.model_known_context(db, actor)[2]
            self.assertEqual(known['roleSkill'], versions['roleSkill'])
            actor['threadId'] = 'another-thread'
            self.assertNotIn('roleSkill', self.runtime.model_known_context(db, actor)[2])


if __name__ == '__main__':
    unittest.main()
