#!/usr/bin/env python3
"""User messages preserve history, identities, and delivery boundaries."""
import importlib.util
import json
from pathlib import Path
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('user_message_fixture', Path(__file__).with_name('workspace-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)
from codex_user_messages import migrate, send_to_user


class UserMessages(unittest.TestCase):
    setUp = f.WorkspaceContract.setUp
    tearDown = f.WorkspaceContract.tearDown
    lead = f.WorkspaceContract.lead
    worker = f.WorkspaceContract.worker
    agent_update = f.WorkspaceContract.agent_update

    def task(self, lead, key='old-task', status='accepted'):
        return {'id': key, 'agent': lead['id'], 'rootId': lead['id'], 'title': 'Approve access',
                'description': 'Open the settings', 'criteria': 'Access works', 'status': status,
                'version': 4, 'created': 100.25, 'updated': 400.5,
                'history': [
                    {'action': 'create', 'text': '', 'actor': lead['id'], 'at': 100.25},
                    {'action': 'update', 'text': '', 'actor': lead['id'], 'at': 200.25},
                    {'action': 'complete', 'text': 'Enabled', 'actor': 'user', 'at': 300.25},
                    {'action': 'accept', 'text': 'Verified', 'actor': lead['id'], 'at': 400.5}]}

    def store_tasks(self, *tasks):
        with self.runtime.db() as db:
            db.execute('CREATE TABLE IF NOT EXISTS runtime_user_tasks (id TEXT PRIMARY KEY, record TEXT NOT NULL)')
            for task in tasks:
                self.runtime.put(db, 'user_tasks', task)

    def test_migration_preserves_history_and_does_not_wake(self):
        lead = self.lead()
        task = self.task(lead)
        self.store_tasks(task)
        self.agent_update(lead, deletedAt=123)
        with self.runtime.db() as db:
            before = db.execute('SELECT count(*) FROM runtime_events').fetchone()[0]
            self.assertEqual(migrate(self.runtime, db), 1)
            message = json.loads(db.execute('SELECT record FROM runtime_complaints WHERE id=?', (task['id'],)).fetchone()[0])
            self.assertEqual(message['created'], 100.25)
            self.assertEqual(message['updated'], 400.5)
            self.assertEqual(message['readAt'], 300.25)
            self.assertEqual(message['status'], 'resolved')
            self.assertEqual(message['recipient'], 'user')
            self.assertEqual([r['at'] for r in message['responses']], [200.25, 300.25, 400.5])
            self.assertEqual([r['author'] for r in message['responses']], [lead['id'], 'user', lead['id']])
            self.assertIn('Access works', message['text'])
            self.assertIn('Enabled', message['responses'][1]['text'])
            self.assertIn('Verified', message['responses'][2]['text'])
            self.assertEqual(json.loads(db.execute('SELECT record FROM runtime_user_tasks').fetchone()[0]), task)
            self.assertEqual(migrate(self.runtime, db), 0)
            self.assertEqual(db.execute('SELECT count(*) FROM runtime_events').fetchone()[0], before)

    def test_cancelled_migrated_request_does_not_need_reply(self):
        lead = self.lead()
        task = self.task(lead, status='cancelled')
        task['history'] = [task['history'][0],
                           {'action': 'cancel', 'text': 'No longer needed', 'actor': lead['id'], 'at': 400.5}]
        self.store_tasks(task)
        with self.runtime.db() as db:
            migrate(self.runtime, db)
            summary = next(item for item in self.runtime.complaint_summaries(db) if item['id'] == task['id'])
            self.assertFalse(summary['needsResponse'])

    def test_returned_migrated_request_needs_new_reply(self):
        lead = self.lead()
        task = self.task(lead, status='open')
        task['history'][-1] = {'action': 'return', 'text': 'Try again', 'actor': lead['id'], 'at': 400.5}
        self.store_tasks(task)
        with self.runtime.db() as db:
            migrate(self.runtime, db)
            summary = next(item for item in self.runtime.complaint_summaries(db) if item['id'] == task['id'])
            self.assertTrue(summary['needsResponse'])

    def test_collision_fails_before_any_migration_write(self):
        lead = self.lead()
        first, collision = self.task(lead, 'first'), self.task(lead, 'collision')
        self.store_tasks(first, collision)
        with self.runtime.db() as db:
            self.runtime.put(db, 'complaints', {'id': 'collision', 'sourceType': 'message'})
            with self.assertRaisesRegex(ValueError, 'same id'):
                migrate(self.runtime, db)
            self.assertEqual(db.execute('SELECT count(*) FROM runtime_complaints').fetchone()[0], 1)

    def test_migration_handles_new_database(self):
        with self.runtime.db() as db:
            db.execute('DROP TABLE IF EXISTS runtime_user_tasks')
            self.assertEqual(migrate(self.runtime, db), 0)

    def test_pending_completion_becomes_message_without_changing_inflight(self):
        lead = self.lead()
        self.store_tasks(self.task(lead))
        payload = json.dumps({'task_id': 'old-task', 'completionNote': 'Done', 'instruction': 'old tool'})
        with self.runtime.db() as db:
            for state in ['pending', 'running', 'done']:
                db.execute('INSERT INTO runtime_events (id,agent,kind,text,status,created,epoch) VALUES (?,?,?,?,?,?,?)',
                           (state, lead['id'], 'user_task_completed', payload, state, 123, lead['epoch']))
            migrate(self.runtime, db)
            rows = {r['id']: dict(r) for r in db.execute('SELECT * FROM runtime_events')}
            self.assertIn('target=user', json.loads(rows['pending']['text'])['instruction'])
            self.assertEqual(rows['running']['text'], payload)
            self.assertEqual(rows['done']['text'], payload)
            for state in ['pending', 'running', 'done']:
                self.assertEqual(rows[state]['status'], state)
                self.assertEqual(rows[state]['created'], 123)

    def test_send_is_atomic_idempotent_and_does_not_wake(self):
        lead = self.lead()
        with self.runtime.db() as db:
            before = db.execute('SELECT count(*) FROM runtime_events').fetchone()[0]
        result = self.runtime.chat_message(lead['id'], 'user', 'Please approve', 'message-1', lead['epoch'])
        self.assertEqual(result, send_to_user(self.runtime, lead['id'], 'Please approve', 'message-1', lead['epoch']))
        with self.assertRaisesRegex(ValueError, 'different content'):
            send_to_user(self.runtime, lead['id'], 'Changed', 'message-1', lead['epoch'])
        with self.runtime.db() as db:
            self.assertEqual(db.execute('SELECT count(*) FROM runtime_complaints').fetchone()[0], 1)
            self.assertEqual(db.execute('SELECT count(*) FROM runtime_operation_receipts WHERE id=?', ('message-1',)).fetchone()[0], 1)
            self.assertEqual(db.execute('SELECT count(*) FROM runtime_events').fetchone()[0], before)
        message = self.runtime.complaint_detail(result['id'])
        self.assertEqual(message['author'], lead['id'])
        self.assertEqual(message['recipient'], 'user')

    def test_failed_receipt_rolls_back_message(self):
        lead = self.lead()
        with patch.object(self.runtime, 'save_receipt', side_effect=RuntimeError('storage failed')):
            with self.assertRaisesRegex(RuntimeError, 'storage failed'):
                send_to_user(self.runtime, lead['id'], 'Please approve', 'failed-message')
        with self.runtime.db() as db:
            self.assertEqual(db.execute('SELECT count(*) FROM runtime_complaints').fetchone()[0], 0)
        self.assertEqual(send_to_user(self.runtime, lead['id'], 'Please approve', 'failed-message')['recipient'], 'user')

    def test_migration_preserves_new_replies_and_return_cancel_history(self):
        lead = self.lead()
        task = self.task(lead, status='cancelled')
        task['history'].extend([
            {'action': 'return', 'text': 'Try again', 'actor': lead['id'], 'at': 450},
            {'action': 'cancel', 'text': 'No longer needed', 'actor': lead['id'], 'at': 500},
        ])
        task.update(updated=500, version=6)
        self.store_tasks(task)
        with self.runtime.db() as db:
            migrate(self.runtime, db)
            message = json.loads(db.execute('SELECT record FROM runtime_complaints').fetchone()[0])
            self.assertEqual(message['status'], 'declined')
            self.assertEqual(message['responses'][-2]['at'], 450)
            self.assertIn('No longer needed', message['responses'][-1]['text'])
            message['responses'].append({'id': 'new-reply', 'author': 'user', 'text': 'Understood', 'at': 600, 'status': 'resolved'})
            message.update(status='resolved', updated=600, version=7)
            self.runtime.put(db, 'complaints', message)
            self.assertEqual(migrate(self.runtime, db), 0)
            self.assertEqual(json.loads(db.execute('SELECT record FROM runtime_complaints').fetchone()[0]), message)

    def test_workers_stopped_deleted_and_stale_senders_rejected(self):
        lead = self.lead()
        worker = self.worker(lead)
        with self.assertRaisesRegex(ValueError, 'Only the lead'):
            send_to_user(self.runtime, worker['id'], 'Hello', 'worker')
        with self.assertRaisesRegex(ValueError, 'stopped'):
            send_to_user(self.runtime, lead['id'], 'Hello', 'stale', lead['epoch'] + 1)
        self.agent_update(lead, autoWake=False)
        with self.assertRaisesRegex(ValueError, 'stopped'):
            send_to_user(self.runtime, lead['id'], 'Hello', 'paused')
        self.agent_update(lead, autoWake=True, deletedAt=123)
        with self.assertRaisesRegex(ValueError, 'stopped'):
            send_to_user(self.runtime, lead['id'], 'Hello', 'deleted')


if __name__ == '__main__':
    unittest.main()
