#!/usr/bin/env python3
"""Exact native byte/ordinal ancestry projection contracts."""
import copy
import importlib.util
import json
from pathlib import Path
import unittest
from unittest.mock import patch
import uuid

spec = importlib.util.spec_from_file_location('base', Path(__file__).with_name('context-repair-contract.py'))
b = importlib.util.module_from_spec(spec)
spec.loader.exec_module(b)
r = b.repair


class Ancestry(unittest.TestCase):
    setUp = b.ContextRepair.setUp
    tearDown = b.ContextRepair.tearDown
    lead = b.ContextRepair.lead
    agent_update = b.ContextRepair.agent_update
    write_records = b.ContextRepair.write_records

    def inherit(self):
        self.records[0]['payload']['base_instructions'] = {'text':'Preserve base instructions.'}
        self.records.insert(1, {'type':'response_item','payload':{'type':'message','role':'developer',
            'content':[{'type':'input_text','text':'Preserve developer instructions.'}]}})
        for ordinal, record in enumerate(self.records):
            record['ordinal'] = ordinal
        self.write_records()
        self.ancestor, self.ancestor_bytes = self.path, self.path.read_bytes()
        child = str(uuid.uuid4())
        self.path = self.home / 'sessions' / ('rollout-child-' + child + '.jsonl')
        self.child_records = [
            {'type':'session_meta','ordinal':len(self.records),'payload':{'id':child,
             'base_instructions':{'text':'Preserve target base instructions.'},
             'history_base':{'thread_id':self.tid,'end_byte_offset':len(self.ancestor_bytes),
                             'end_ordinal_exclusive':len(self.records)}}},
            {'type':'event_msg','ordinal':len(self.records)+1,'payload':{'type':'thread_settings_applied','thread_id':child}}]
        self.save_child()
        self.a = self.agent_update(self.a, threadId=child)
        self.server.path, self.server.tid = self.path, child
        return child

    def save_child(self):
        self.path.write_text(''.join(json.dumps(row)+'\n' for row in self.child_records))

    def test_materialized_prefix_preserves_payloads_and_excludes_later_ancestor_suffix(self):
        child = self.inherit()
        with self.ancestor.open('a') as stream:
            stream.write(json.dumps({'ordinal':len(self.records),'type':'response_item',
                'payload':{'type':'message','role':'user','content':[{'type':'input_text','text':'EXCLUDED-LATER-USER'}]}})+'\n')
        original = self.ancestor.read_bytes(), self.path.read_bytes()
        repaired = r.repair_idle(self.runtime, self.a['id'])
        report = repaired['contextRepair']['snapshot']
        projected = [json.loads(line) for line in Path(report['copyPath']).read_text().splitlines()]
        self.assertNotIn('history_base', projected[0]['payload'])
        self.assertEqual(projected[0]['payload']['base_instructions'], {'text':'Preserve target base instructions.'})
        self.assertEqual([x['ordinal'] for x in projected], list(range(len(projected))))
        text = json.dumps(projected)
        for marker in ('Preserve developer instructions.', 'exitCode=0 receipt-192', 'fixture-image', 'Preserve real user text.', 'Preserve summary.'):
            self.assertIn(marker,text)
        self.assertNotIn('EXCLUDED-LATER-USER',text)
        self.assertEqual((self.ancestor.read_bytes(),self.path.read_bytes()),original)
        self.assertEqual(report['ancestry'][0]['endByteOffset'],len(self.ancestor_bytes))
        self.assertEqual(report['sourceSha256'],r._hash_file(self.path))
        self.assertEqual(len([x for x in self.server.calls if x[0]=='thread/fork']),1)

    def test_empty_index_requires_verified_terminal_inherited_metadata_tail(self):
        self.inherit()
        self.server.no_turns = True
        a = r.repair_idle(self.runtime,self.a['id'])
        self.assertEqual(a['contextRepair']['phase'],'completed')
        self.assertEqual(a['contextRepair']['snapshot']['terminalTurnId'],'turn')

    def test_empty_index_rejects_local_items_or_missing_terminal(self):
        self.inherit()
        self.server.no_turns = True
        self.child_records.append({'ordinal':len(self.records)+2,'type':'response_item',
            'payload':{'type':'message','role':'user','content':[]}})
        self.save_child()
        with self.assertRaisesRegex(ValueError,'local history'):
            r.repair_idle(self.runtime,self.a['id'])
        self.child_records.pop()
        self.records[-1]['payload']['type']='token_count'
        self.ancestor.write_text(''.join(json.dumps(x)+'\n' for x in self.records))
        self.child_records[0]['payload']['history_base']['end_byte_offset']=self.ancestor.stat().st_size
        self.save_child()
        with self.assertRaisesRegex(ValueError,'inherited terminal'):
            r.repair_idle(self.runtime,self.a['id'])
        self.assertFalse(any(x[0]=='thread/fork' for x in self.server.calls))

    def test_malformed_boundaries_and_cycle_fail_closed(self):
        child = self.inherit()
        original = copy.deepcopy(self.child_records)
        variants = [
            ('end_byte_offset',len(self.ancestor_bytes)-1), ('end_ordinal_exclusive',len(self.records)-1),
            ('end_byte_offset',True), ('thread_id',child), ('thread_id',str(uuid.uuid4())), ('unexpected',1)]
        for key,value in variants:
            with self.subTest(key=key,value=value):
                self.child_records=copy.deepcopy(original)
                self.child_records[0]['payload']['history_base'][key]=value
                self.save_child()
                with self.assertRaises(ValueError):
                    r.repair_idle(self.runtime,self.a['id'])
        self.assertFalse(any(x[0]=='thread/fork' for x in self.server.calls))

    def test_ancestor_duplicate_prefix_requires_identical_bytes(self):
        self.inherit()
        duplicate = self.ancestor.with_name('duplicate-'+self.ancestor.name)
        duplicate.write_bytes(self.ancestor_bytes)
        self.assertEqual(len(r._rollout_segments(self.home,self.path,self.a['threadId'])),2)
        duplicate.write_bytes(self.ancestor_bytes.replace(b'Preserve summary.',b'DIVERGED summary.'))
        with self.assertRaisesRegex(ValueError,'conflicting'):
            r.repair_idle(self.runtime,self.a['id'])

    def test_ancestor_prefix_change_before_fork_is_rejected(self):
        self.inherit()
        original = r.sanitized_rollout
        def changed(*args,**kwargs):
            report = original(*args,**kwargs)
            self.ancestor.write_bytes(self.ancestor_bytes.replace(b'Preserve summary.',b'DIVERGED summary.'))
            return report
        with patch.object(r,'sanitized_rollout',side_effect=changed):
            with self.assertRaisesRegex(ValueError,'stable saved ancestry'):
                r.repair_idle(self.runtime,self.a['id'])
        self.assertFalse(any(x[0]=='thread/fork' for x in self.server.calls))
        self.assertEqual(list((self.home/'sessions'/'.studio-context-repairs').rglob('*.jsonl')),[])

    def test_copy_cleanup_failure_still_settles_original_operation(self):
        self.inherit()
        original = r.sanitized_rollout
        def changed(*args,**kwargs):
            report = original(*args,**kwargs)
            self.ancestor.write_bytes(self.ancestor_bytes.replace(b'Preserve summary.',b'DIVERGED summary.'))
            return report
        with patch.object(r,'sanitized_rollout',side_effect=changed), patch.object(Path,'unlink',side_effect=OSError('fixture cleanup I/O failure')):
            with self.assertRaisesRegex(ValueError,'stable saved ancestry'):
                r.repair_idle(self.runtime,self.a['id'])
        receipt = self.runtime.agent(self.a['id'])['contextRepair']
        self.assertEqual(receipt['phase'],'failed')
        self.assertEqual(receipt['copyCleanup']['outcome'],'failed')
        self.assertTrue(Path(receipt['copyCleanup']['path']).exists())
        self.assertFalse(any(x[0]=='thread/fork' for x in self.server.calls))

    def test_ancestor_line_limit_and_symlink_cannot_escape_managed_home(self):
        self.inherit()
        escaped = self.root / self.ancestor.name
        escaped.write_bytes(self.ancestor_bytes)
        self.ancestor.unlink()
        self.ancestor.symlink_to(escaped)
        with self.assertRaisesRegex(ValueError,'leaves the account home'):
            r.repair_idle(self.runtime,self.a['id'])
        self.ancestor.unlink()
        self.ancestor.write_bytes(self.ancestor_bytes)
        # A sparse oversized line checks the allocation bound without a large fixture.
        oversized = self.home / 'oversized.jsonl'
        with oversized.open('wb') as stream:
            stream.seek(64 * 1024 * 1024)
            stream.write(b'\n')
        with self.assertRaisesRegex(ValueError,'64 MiB'):
            next(r._prefix_records(oversized,oversized.stat().st_size))

    def test_empty_index_never_bypasses_active_native_requests(self):
        self.inherit()
        self.server.no_turns=True
        self.server.status={'type':'active','activeFlags':['waitingOnUserInput']}
        with self.assertRaisesRegex(ValueError,'native status'):
            r.repair_idle(self.runtime,self.a['id'])
        self.assertFalse(any(x[0]=='thread/fork' for x in self.server.calls))


if __name__ == '__main__':
    unittest.main(verbosity=2)
