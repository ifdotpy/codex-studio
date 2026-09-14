#!/usr/bin/env python3
"""Exercise terminal proof, exact receipt repair and atomic journal rollback."""
import copy
import hashlib
import json
from pathlib import Path
import sqlite3
import sys
import tempfile
import threading
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
import codex_runtime
import codex_terminal_receipt_recovery_update as update


class TerminalReceiptRecoveryContract(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.plan = copy.deepcopy(update.PLAN)
        self.plan_patch = patch.object(update, 'PLAN', self.plan)
        self.plan_patch.start()
        self.addCleanup(self.plan_patch.stop)
        self.runtime = object.__new__(codex_runtime.Runtime)
        self.runtime.root = self.root
        self.runtime.db_path = self.root / 'canvas.sqlite3'
        self.runtime.lock = threading.RLock()
        self.runtime.closed = False
        self.runtime.connections = {'active': object()}
        self.runtime.requests = {'pending': object()}
        self.runtime.streams = {'native': object()}
        self.saved = {key: self.plan[key] for key in ('agent', 'accountKey', 'threadId', 'turnId', 'callId', 'tool', 'epoch')}
        self.saved.update(id=self.plan['accountKey'] + ':' + self.plan['threadId'] + ':' + self.plan['callId'],
                          signature=hashlib.sha256(update._encoded({'tool': self.plan['tool'], 'arguments': {}})).hexdigest(),
                          stage='queued', outcome='pending', cancelRequested=False, created=1, updated=1)
        self.result = {'success': False, 'contentItems': [{'type': 'inputText', 'text': 'disk I/O error'}]}
        self.actor = {'id': self.plan['agent'], **{key: self.plan[key] for key in ('accountKey', 'threadId', 'epoch')}}
        with self.runtime.db() as db:
            db.executescript('''CREATE TABLE runtime_agents(id TEXT PRIMARY KEY, record TEXT NOT NULL);
                CREATE TABLE runtime_tool_requests(id TEXT PRIMARY KEY, record TEXT NOT NULL);
                CREATE TABLE runtime_tool_results(id TEXT PRIMARY KEY, result TEXT NOT NULL);
                CREATE TABLE runtime_operation_receipts(id TEXT PRIMARY KEY, result TEXT NOT NULL);''')
            db.execute('INSERT INTO runtime_agents VALUES (?,?)', (self.actor['id'], json.dumps(self.actor)))
            self.runtime.put(db, 'tool_requests', self.saved)
        self.path = self.root / 'accounts' / self.plan['accountKey'] / 'sessions' / ('rollout-fixture-' + self.plan['threadId'] + '.jsonl')
        self.path.parent.mkdir(parents=True)
        header = {'type': 'session_meta', 'payload': {'id': self.plan['threadId']}}
        receipt = {'ordinal': 1, 'type': 'event_msg', 'payload': {'type': 'item_completed',
            'thread_id': self.plan['threadId'], 'turn_id': self.plan['turnId'],
            'item': {'type': 'DynamicToolCall', 'id': self.plan['callId'], 'tool': self.plan['tool'],
                     'arguments': {}, 'status': 'failed', 'success': False, 'content_items': self.result['contentItems']}}}
        terminal = {'ordinal': 2, 'type': 'event_msg', 'payload': {'type': 'task_complete', 'turn_id': self.plan['turnId']}}
        raw = [(json.dumps(row) + '\n').encode() for row in (header, receipt, terminal)]
        self.path.write_bytes(b''.join(raw))
        def proof(index, record):
            return {'line': index + 1, 'byteOffset': sum(map(len, raw[:index])), 'record': record,
                    'rawRecord': raw[index].decode(), 'recordSha256': hashlib.sha256(raw[index]).hexdigest()}
        info = self.path.stat()
        self.evidence = {'sourcePath': str(self.path), 'savedRequest': self.saved, 'nativeResult': self.result,
                         'receipt': proof(1, receipt), 'terminalTurn': proof(2, terminal),
                         'sourceObservation': {'device': info.st_dev, 'inode': info.st_ino, 'size': info.st_size}}
        self.write_evidence()

    def write_evidence(self):
        raw = update._encoded(self.evidence)
        self.evidence_path = self.root / self.plan['evidence']
        self.evidence_path.parent.mkdir(parents=True, exist_ok=True)
        self.evidence_path.write_bytes(raw)
        self.plan['sha256'] = hashlib.sha256(raw).hexdigest()

    def state(self):
        with self.runtime.db() as db:
            return list(db.iterdump())

    def apply(self):
        return update.apply(self.runtime)

    def test_exact_native_failure_repairs_once_and_preserves_live_objects(self):
        callbacks = self.runtime.finish_tool_request, self.runtime.put
        objects = self.runtime.connections, self.runtime.requests, self.runtime.streams
        self.assertEqual(self.apply()['status'], 'applied')
        with self.runtime.db() as db:
            after = self.runtime.tool_request(self.saved['id'], db)
            journal = json.loads(db.execute('SELECT record FROM runtime_receipt_recoveries').fetchone()[0])
            self.assertEqual(journal, {'id': self.plan['id'], 'evidenceSha256': self.plan['sha256'],
                                      'proof': self.evidence, 'before': self.saved, 'after': after})
            self.assertEqual((after['stage'], after['outcome'], after['result']), ('failed', 'unknown', self.result))
            self.assertEqual(db.execute('SELECT count(*) FROM runtime_tool_results').fetchone()[0], 0)
            self.assertEqual(json.loads(db.execute('SELECT record FROM runtime_agents').fetchone()[0]), self.actor)
        before = self.state()
        self.assertEqual(self.apply()['status'], 'already_applied')
        self.assertEqual(before, self.state())
        self.assertEqual(callbacks, (self.runtime.finish_tool_request, self.runtime.put))
        self.assertEqual(objects, (self.runtime.connections, self.runtime.requests, self.runtime.streams))

    def test_changed_local_identity_newer_or_definitive_receipt_rejects(self):
        for field, value in {'agent': 'other', 'accountKey': 'other', 'threadId': 'other', 'turnId': 'other',
                             'callId': 'other', 'signature': 'other', 'epoch': 9, 'stage': 'running',
                             'outcome': 'applied', 'updated': 2, 'result': self.result}.items():
            with self.runtime.db() as db:
                self.runtime.put(db, 'tool_requests', {**self.saved, field: value})
            before = self.state()
            with self.subTest(field=field), self.assertRaisesRegex(RuntimeError, 'local receipt changed'):
                self.apply()
            self.assertEqual(before, self.state())

    def test_changed_actor_scope_rejects(self):
        for field, value in {'accountKey': 'other', 'threadId': 'other', 'epoch': 9, 'deletedAt': 2}.items():
            with self.runtime.db() as db:
                db.execute('UPDATE runtime_agents SET record=?', (json.dumps({**self.actor, field: value}),))
            before = self.state()
            with self.subTest(field=field), self.assertRaisesRegex(RuntimeError, 'agent account'):
                self.apply()
            self.assertEqual(before, self.state())

    def test_existing_cached_or_operation_receipt_rejects(self):
        for table in ('runtime_tool_results', 'runtime_operation_receipts'):
            with self.runtime.db() as db:
                db.execute('INSERT INTO ' + table + ' VALUES (?,?)', (self.saved['id'], '{}'))
            before = self.state()
            with self.subTest(table=table), self.assertRaisesRegex(RuntimeError, 'already saved'):
                self.apply()
            self.assertEqual(before, self.state())
            with self.runtime.db() as db:
                db.execute('DELETE FROM ' + table)

    def test_unreviewed_or_changed_native_bytes_reject(self):
        before = self.state()
        self.evidence_path.write_bytes(self.evidence_path.read_bytes() + b' ')
        with self.assertRaisesRegex(RuntimeError, 'unreviewed evidence'):
            self.apply()
        self.write_evidence()
        with self.path.open('r+b') as source:
            source.seek(self.evidence['receipt']['byteOffset'] + 1)
            source.write(b'!')
        with self.assertRaisesRegex(RuntimeError, 'native record bytes changed'):
            self.apply()
        self.assertEqual(before, self.state())

    def test_account_escape_and_replaced_source_reject(self):
        before = self.state()
        self.evidence['sourcePath'] = str(self.evidence_path)
        self.write_evidence()
        with self.assertRaisesRegex(RuntimeError, 'escapes its account'):
            self.apply()
        self.evidence['sourcePath'] = str(self.path)
        self.evidence['sourceObservation']['inode'] += 1
        self.write_evidence()
        with self.assertRaisesRegex(RuntimeError, 'native source identity changed'):
            self.apply()
        self.assertEqual(before, self.state())

    def test_native_source_changes_during_read_reject(self):
        original = update._record
        def changing(source, proof):
            record = original(source, proof)
            with self.path.open('ab') as output:
                output.write(b'\n')
            return record
        before = self.state()
        with patch.object(update, '_record', side_effect=changing):
            with self.assertRaisesRegex(RuntimeError, 'source changed during proof read'):
                self.apply()
        self.assertEqual(before, self.state())

    def test_unknown_journal_schema_or_proof_reject(self):
        with self.runtime.db() as db:
            db.execute('CREATE TABLE runtime_receipt_recoveries(id TEXT PRIMARY KEY, record BLOB)')
        before = self.state()
        with self.assertRaisesRegex(RuntimeError, 'unknown recovery journal schema'):
            self.apply()
        self.assertEqual(before, self.state())
        with self.runtime.db() as db:
            db.execute('DROP TABLE runtime_receipt_recoveries')
        self.apply()
        with self.runtime.db() as db:
            db.execute('UPDATE runtime_receipt_recoveries SET record=?', ('{}',))
        before = self.state()
        with self.assertRaisesRegex(RuntimeError, 'changed after its recorded recovery'):
            self.apply()
        self.assertEqual(before, self.state())

    def test_native_semantics_reject_even_with_reviewed_bytes(self):
        original = copy.deepcopy(self.evidence)
        for kind in ('call', 'thread', 'turn', 'success', 'status', 'arguments', 'terminal'):
            self.evidence = copy.deepcopy(original)
            receipt = self.evidence['receipt']['record']
            item = receipt['payload']['item']
            if kind == 'call': item['id'] = 'other'
            if kind == 'thread': receipt['payload']['thread_id'] = 'other'
            if kind == 'turn': receipt['payload']['turn_id'] = 'other'
            if kind == 'success': item['success'] = True
            if kind == 'status': item['status'] = 'inProgress'
            if kind == 'arguments': item['arguments'] = {'changed': True}
            if kind == 'terminal': self.evidence['terminalTurn']['record']['payload']['turn_id'] = 'other'
            header = (json.dumps({'type': 'session_meta', 'payload': {'id': self.plan['threadId']}}) + '\n').encode()
            raw = header
            for key in ('receipt', 'terminalTurn'):
                proof = self.evidence[key]
                line = (json.dumps(proof['record']) + '\n').encode()
                proof.update(byteOffset=len(raw), rawRecord=line.decode(), recordSha256=hashlib.sha256(line).hexdigest())
                raw += line
            self.path.write_bytes(raw)
            self.evidence['sourceObservation']['size'] = len(raw)
            self.write_evidence()
            before = self.state()
            with self.subTest(kind=kind), self.assertRaises(RuntimeError):
                self.apply()
            self.assertEqual(before, self.state())

    def test_commit_failure_rolls_back_receipt_and_journal_then_retry_succeeds(self):
        class RejectCommit(sqlite3.Connection):
            def __exit__(self, kind, value, trace):
                if kind is None:
                    self.rollback()
                    raise sqlite3.OperationalError('Controlled commit failure')
                return super().__exit__(kind, value, trace)
        connect = sqlite3.connect
        before = self.state()
        with patch.object(codex_runtime.sqlite3, 'connect', side_effect=lambda *a, **kw: connect(*a, **kw, factory=RejectCommit)):
            with self.assertRaisesRegex(sqlite3.OperationalError, 'Controlled commit failure'):
                self.apply()
        self.assertEqual(before, self.state())
        self.assertEqual(self.apply()['status'], 'applied')

    def test_writer_failure_rolls_back_receipt_and_journal(self):
        put = self.runtime.put
        def fail(db, table, record):
            put(db, table, record)
            raise RuntimeError('Controlled write failure')
        before = self.state()
        with patch.object(self.runtime, 'put', side_effect=fail):
            with self.assertRaisesRegex(RuntimeError, 'Controlled write failure'):
                self.apply()
        self.assertEqual(before, self.state())
        self.assertEqual(self.apply()['status'], 'applied')

    def test_changed_receipt_after_recovery_never_overwrites_it(self):
        self.apply()
        with self.runtime.db() as db:
            row = self.runtime.tool_request(self.saved['id'], db)
            self.runtime.put(db, 'tool_requests', {**row, 'outcome': 'applied', 'updated': row['updated'] + 1})
        before = self.state()
        with self.assertRaisesRegex(RuntimeError, 'changed after its recorded recovery'):
            self.apply()
        self.assertEqual(before, self.state())

    def test_unknown_writer_and_closed_runtime_reject(self):
        before = self.state()
        with patch.object(self.runtime, 'finish_tool_request', return_value={}):
            with self.assertRaisesRegex(RuntimeError, 'unknown receipt writer'):
                self.apply()
        self.runtime.closed = True
        with self.assertRaisesRegex(RuntimeError, 'runtime is closed'):
            self.apply()
        self.assertEqual(before, self.state())


if __name__ == '__main__':
    unittest.main()
