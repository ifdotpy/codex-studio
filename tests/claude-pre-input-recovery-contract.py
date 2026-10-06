#!/usr/bin/env python3
"""Exact Claude pre-input rejection retries one batch without repeating unknown work."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import copy
import importlib.util
import json
import os
from pathlib import Path
import time
from types import SimpleNamespace
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('claude_input_workspace_fixture', ROOT / 'tests/workspace-contract.py')
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)
from codex_native_errors import NativeRpcError
from codex_source import source_function
import codex_runtime


class ClaudePreInputRecovery(unittest.TestCase):
    tearDown = f.WorkspaceContract.tearDown
    lead = f.WorkspaceContract.lead
    agent_update = f.WorkspaceContract.agent_update

    def setUp(self):
        baseline = os.environ.get('STUDIO_CLAUDE_INPUT_BASELINE')
        if baseline:
            source = Path(baseline).read_bytes()
            for name in ('start_error', 'start_accepted', 'notification'):
                function, _ = source_function(source, ('Runtime', name), vars(codex_runtime), baseline)
                replacement = patch.object(codex_runtime.Runtime, name, function)
                replacement.start()
                self.addCleanup(replacement.stop)
        f.WorkspaceContract.setUp(self)
        self.agent = self.agent_update(self.lead(), provider='claude')
        self.agent = self.runtime.prepare(self.agent)
        self.server = self.runtime.server
        self.connection = self.runtime.connection_ids['default']
        self.ids = ['original-a', 'original-b']
        self.native_inputs = {}
        call = self.server.call

        def exact_input(method, params, timeout=60):
            if method == 'turn/start':
                key = params['clientUserMessageId']
                if key in self.native_inputs and self.native_inputs[key] != params['input']:
                    self.server.calls.append((method, copy.deepcopy(params)))
                    raise NativeRpcError({'code': -32000, 'message':
                                          'This message identity has different content'})
                self.native_inputs[key] = copy.deepcopy(params['input'])
            return call(method, params, timeout)

        replacement = patch.object(self.server, 'call', side_effect=exact_input)
        replacement.start()
        self.addCleanup(replacement.stop)

    def queue(self):
        for key in self.ids:
            self.runtime.send(self.agent['id'], 'Private fixture input ' + key, key, manual=False)

    def start(self):
        self.queue()
        self.runtime.dispatch()
        f.eventually(lambda: all(self.runtime.delivery_receipt(key)['status'] == 'delivered' for key in self.ids))
        self.agent = self.runtime.agent(self.agent['id'])
        self.attempt = copy.deepcopy(self.agent['startAttempt'])
        self.turn_id = self.agent['turnId']
        self.calls = list(self.server.calls)

    def terminal(self, **changes):
        turn = {'id': self.turn_id, 'status': 'failed', 'startOutcome': 'not_applied',
                'error': {'message': 'Claude preparation timed out before input was submitted',
                          'data': {'turnStartOutcome': 'not_applied'}}}
        turn.update(changes)
        return turn

    def complete(self, turn=None, connection=None):
        turn = turn or self.terminal()
        self.server.active_turns.pop(self.agent['threadId'], None)
        self.runtime.notification({'method': 'turn/completed', 'params': {
            'threadId': self.agent['threadId'], 'turn': turn}}, 'default', connection or self.connection)

    def receipt(self, attempt=None):
        with self.runtime.db() as db:
            row = db.execute('SELECT record FROM runtime_execution_attempts WHERE id=?',
                             ((attempt or self.attempt)['id'],)).fetchone()
            return json.loads(row[0])['claudeInputRejection']

    def assert_pending_batch(self):
        for key in self.ids:
            self.assertEqual(self.runtime.delivery_receipt(key)['status'], 'pending')
        with self.runtime.db() as db:
            self.assertEqual(db.execute('SELECT COUNT(*) FROM runtime_events WHERE agent=? AND id IN (?,?)',
                                        (self.agent['id'], *self.ids)).fetchone()[0], 2)

    def test_account_validation_rejection_keeps_exact_input_pending_without_auto_retry(self):
        self.start()
        terminal = self.terminal(error={'message': 'Claude sign-in cannot be verified', 'data': {
            'turnStartOutcome': 'not_applied', 'claudePreparationFailure': 'account_validation'}})
        self.complete(terminal)
        self.assert_pending_batch()
        current = self.runtime.agent(self.agent['id'])
        self.assertTrue(current['nativeFailureHold'])
        self.assertEqual(current['status'], 'failed')
        self.assertFalse(self.receipt()['retry'])
        self.runtime.dispatch()
        self.assertEqual(self.server.calls, self.calls)
        self.runtime.send(self.agent['id'], 'The user restores the same account', 'auth-continue', manual=True)
        self.runtime.dispatch()
        f.eventually(lambda: all(self.runtime.delivery_receipt(key)['status'] == 'delivered' for key in self.ids))
        starts = [params for method, params in self.server.calls if method == 'turn/start']
        self.assertEqual(starts[0]['input'], starts[1]['input'])
        self.assertEqual(starts[0]['clientUserMessageId'], starts[1]['clientUserMessageId'])
        self.assertEqual(self.runtime.delivery_receipt('auth-continue')['status'], 'pending')

    def test_notification_after_rpc_acceptance_retries_same_ids_once(self):
        self.start()
        self.runtime.send(self.agent['id'], 'Private later input', 'later', manual=False)
        self.complete()
        current = self.runtime.agent(self.agent['id'])
        self.assertFalse(current.get('nativeFailureHold'))
        self.assertEqual(current['status'], 'queued')
        self.assertIsNone(current.get('startAttempt'))
        self.assert_pending_batch()
        self.assertEqual(self.server.calls, self.calls)
        self.assertEqual(self.receipt()['events'], self.ids)
        self.assertTrue(self.receipt()['retry'])
        self.complete()
        self.assert_pending_batch()
        self.assertEqual(self.server.calls, self.calls)
        self.runtime.dispatch()
        f.eventually(lambda: all(self.runtime.delivery_receipt(key)['status'] == 'delivered' for key in self.ids))
        self.assertEqual(self.runtime.delivery_receipt('later')['status'], 'pending')
        starts = [params for method, params in self.server.calls if method == 'turn/start']
        self.assertEqual(len(starts), 2)
        self.assertEqual(starts[0]['clientUserMessageId'], self.ids[0])
        self.assertEqual(starts[1]['clientUserMessageId'], self.ids[0])
        current = self.runtime.agent(self.agent['id'])
        later_attempt = copy.deepcopy(current['startAttempt'])
        self.runtime.start_accepted(self.agent['id'], self.attempt, {'turn': {'id': self.turn_id}})
        self.assertEqual(self.runtime.agent(self.agent['id'])['startAttempt'], later_attempt)
        self.assertEqual(self.runtime.agent(self.agent['id'])['turnId'], current['turnId'])
        self.assertEqual(len([m for m, _ in self.server.calls if m == 'turn/start']), 2)

    def test_retry_keeps_native_body_and_does_not_merge_new_input(self):
        call = self.server.call
        inputs = {}

        def exact_input(method, params, timeout=60):
            if method == 'turn/start':
                key = params['clientUserMessageId']
                if key in inputs and inputs[key] != params['input']:
                    self.server.calls.append((method, copy.deepcopy(params)))
                    raise NativeRpcError({'code': -32000, 'message':
                                          'This message identity has different content'})
                inputs[key] = copy.deepcopy(params['input'])
            return call(method, params, timeout)

        with patch.object(self.server, 'call', side_effect=exact_input):
            self.start()
            self.runtime.send(self.agent['id'], 'Private later input', 'later', manual=False)
            self.complete()
            self.runtime.dispatch()
            f.eventually(lambda: sum(method == 'turn/start' for method, _ in self.server.calls) == 2)
            starts = [params for method, params in self.server.calls if method == 'turn/start']
            self.assertEqual(starts[1]['input'], starts[0]['input'])
            f.eventually(lambda: all(self.runtime.delivery_receipt(key)['status'] == 'delivered'
                                    for key in self.ids))
            self.assertEqual(self.runtime.agent(self.agent['id'])['startAttempt']['events'], self.ids)
            self.assertEqual(self.runtime.delivery_receipt('later')['status'], 'pending')

    def test_notification_before_rpc_answer_keeps_rejection_after_late_answer(self):
        self.queue()
        call = self.server.call
        original = []

        def finish_first(method, params, timeout=60):
            result = call(method, params, timeout)
            if method == 'turn/start':
                self.agent = self.runtime.agent(self.agent['id'])
                self.turn_id = result['turn']['id']
                original.append(copy.deepcopy(self.agent['startAttempt']))
                self.complete()
            return result

        with patch.object(self.server, 'call', side_effect=finish_first):
            self.runtime.dispatch()
            f.eventually(lambda: original and self.runtime.agent(self.agent['id'])['status'] == 'queued')
            # Join the actual start worker after its synchronous late response.
            self.runtime.pool.submit(lambda: None).result(timeout=3)
        self.attempt = original[0]
        self.assert_pending_batch()
        self.assertTrue(self.receipt()['retry'])
        self.assertFalse(self.runtime.agent(self.agent['id']).get('nativeFailureHold'))
        self.runtime.start_accepted(self.agent['id'], self.attempt, {'turn': {'id': self.turn_id}})
        self.assert_pending_batch()
        self.assertIsNone(self.runtime.agent(self.agent['id']).get('startAttempt'))
        self.assertEqual(sum(method == 'turn/start' for method, _ in self.server.calls), 1)

    def test_second_rejection_holds_until_new_user_instruction(self):
        self.start()
        self.complete()
        self.runtime.dispatch()
        f.eventually(lambda: all(self.runtime.delivery_receipt(key)['status'] == 'delivered' for key in self.ids))
        self.agent = self.runtime.agent(self.agent['id'])
        self.attempt = copy.deepcopy(self.agent['startAttempt'])
        self.turn_id = self.agent['turnId']
        self.complete()
        self.assert_pending_batch()
        self.assertFalse(self.receipt()['retry'])
        self.assertTrue(self.runtime.agent(self.agent['id'])['nativeFailureHold'])
        self.assertEqual(self.runtime.agent(self.agent['id'])['status'], 'failed')
        self.runtime.dispatch()
        self.assertEqual(sum(method == 'turn/start' for method, _ in self.server.calls), 2)
        self.runtime.send(self.agent['id'], 'The user authorizes continuation', 'manual-continue', manual=True)
        self.runtime.dispatch()
        f.eventually(lambda: all(self.runtime.delivery_receipt(key)['status'] == 'delivered' for key in self.ids))
        self.assertEqual(self.runtime.delivery_receipt('manual-continue')['status'], 'pending')
        self.assertFalse(self.runtime.agent(self.agent['id']).get('nativeFailureHold'))
        self.runtime.dispatch()
        f.eventually(lambda: self.runtime.delivery_receipt('manual-continue')['status'] == 'delivered')
        self.assertEqual(sum(method == 'turn/start' for method, _ in self.server.calls), 4)
        starts = [params for method, params in self.server.calls if method == 'turn/start']
        self.assertEqual(starts[0]['input'], starts[1]['input'])
        self.assertEqual(starts[0]['input'], starts[2]['input'])
        self.assertEqual(starts[3]['clientUserMessageId'], 'manual-continue')
        self.assertFalse(self.runtime.agent(self.agent['id']).get('nativeFailureHold'))

    def test_structured_rpc_rejection_retries_one_unobserved_request(self):
        self.queue()
        error = NativeRpcError({'code': -32000, 'message': 'Pre-input control failed',
                                'data': {'turnStartOutcome': 'not_applied'}})
        call = self.server.call

        def reject(method, params, timeout=60):
            if method == 'turn/start':
                self.server.calls.append((method, params))
                self.attempt = copy.deepcopy(self.runtime.agent(self.agent['id'])['startAttempt'])
                raise error
            return call(method, params, timeout)

        with patch.object(self.server, 'call', side_effect=reject):
            self.runtime.dispatch()
            f.eventually(lambda: hasattr(self, 'attempt') and self.runtime.agent(self.agent['id'])['status'] == 'queued')
        self.assert_pending_batch()
        self.assertTrue(self.receipt()['retry'])
        self.assertIsNone(self.receipt()['turnId'])
        self.assertFalse(self.runtime.agent(self.agent['id']).get('nativeFailureHold'))
        self.runtime.start_error(self.agent['id'], self.attempt['id'], error)
        self.runtime.start_accepted(self.agent['id'], self.attempt, {'turn': {'id': 'late-conflicting-turn'}})
        self.assert_pending_batch()
        self.assertIsNone(self.runtime.agent(self.agent['id'])['turnId'])
        self.assertEqual(sum(method == 'turn/start' for method, _ in self.server.calls), 1)

    def test_rpc_rejection_cannot_recover_an_observed_or_accepted_turn(self):
        self.start()
        error = NativeRpcError({'code': -32000, 'message': 'Post-input failure',
                                'data': {'turnStartOutcome': 'not_applied'}})
        self.runtime.start_error(self.agent['id'], self.attempt['id'], error)
        self.assertEqual(self.runtime.agent(self.agent['id'])['startAttempt'], self.attempt)
        for key in self.ids:
            self.assertEqual(self.runtime.delivery_receipt(key)['status'], 'delivered')
        self.assertEqual(self.server.calls, self.calls)

    def test_lost_response_keeps_uncertain_input_without_retry(self):
        self.queue()
        self.server.fail_start = True
        self.runtime.dispatch()
        f.eventually(lambda: all(self.runtime.delivery_receipt(key)['status'] == 'uncertain' for key in self.ids))
        self.runtime.dispatch()
        for key in self.ids:
            self.assertEqual(self.runtime.delivery_receipt(key)['status'], 'uncertain')
        self.assertEqual(sum(method == 'turn/start' for method, _ in self.server.calls), 1)

    def test_post_input_turn_metadata_and_text_alone_cannot_allow_retry(self):
        self.start()
        for change in ({'startOutcome': 'accepted'}, {'startOutcome': None},
                       {'error': {'message': 'Claude preparation timed out before input was submitted'}},
                       {'error': {'message': 'Malformed receipt', 'data': None}}, {'inputSubmitted': True}):
            with self.subTest(change=change):
                # Invoke the helper against the same exact active source; no native completion is inferred.
                from codex_claude_input_recovery import recover_rejected_start
                with self.runtime.lock, self.runtime.db() as db:
                    agent = self.runtime.agent(self.agent['id'], db)
                    self.assertIsNone(recover_rejected_start(self.runtime, db, agent, agent['startAttempt'],
                        turn=self.terminal(**change), account_key='default', connection_id=self.connection))
                self.assertEqual(self.runtime.agent(self.agent['id'])['startAttempt'], self.attempt)
        self.complete(self.terminal(startOutcome='accepted'))
        self.assertTrue(self.runtime.agent(self.agent['id'])['nativeFailureHold'])
        for key in self.ids:
            self.assertEqual(self.runtime.delivery_receipt(key)['status'], 'delivered')
        self.assertEqual(self.server.calls, self.calls)

    def test_actual_assistant_or_tool_work_disproves_pre_input_receipt(self):
        self.start()
        self.runtime.notification({'method': 'item/agentMessage/delta', 'params': {
            'threadId': self.agent['threadId'], 'turnId': self.turn_id,
            'itemId': 'actual-answer', 'delta': 'Private provider output'}}, 'default', self.connection)
        self.complete()
        self.assertTrue(self.runtime.agent(self.agent['id'])['nativeFailureHold'])
        for key in self.ids:
            self.assertEqual(self.runtime.delivery_receipt(key)['status'], 'delivered')
        self.assertEqual(self.server.calls, self.calls)

    def test_actual_command_receipt_disproves_pre_input_even_after_terminal_projection(self):
        self.start()
        command = {'id': 'started-command', 'agent': self.agent['id'], 'kind': 'command',
                   'turnId': self.turn_id, 'threadId': self.agent['threadId'],
                   'status': 'running', 'created': time.time()}
        with self.runtime.db() as db:
            self.runtime.put(db, 'tasks', command)
        self.complete()
        self.assertTrue(self.runtime.agent(self.agent['id'])['nativeFailureHold'])
        for key in self.ids:
            self.assertEqual(self.runtime.delivery_receipt(key)['status'], 'delivered')
        with self.runtime.db() as db:
            saved = json.loads(db.execute('SELECT record FROM runtime_tasks WHERE id=?', (command['id'],)).fetchone()[0])
            self.assertEqual(saved['turnId'], self.turn_id)
            self.assertEqual(saved['status'], 'interrupted')
        self.assertEqual(self.server.calls, self.calls)

    def test_new_supervisor_generation_on_same_connection_rejects_old_proof(self):
        self.server.supervisor_mode = True
        self.server.proc = SimpleNamespace(root=self.state.resolve(), handle='fixture-native', generation=1)
        self.start()
        self.server.proc.generation = 2
        self.complete()
        self.assertTrue(self.runtime.agent(self.agent['id'])['nativeFailureHold'])
        for key in self.ids:
            self.assertEqual(self.runtime.delivery_receipt(key)['status'], 'delivered')
        self.assertEqual(self.server.calls, self.calls)

    def test_canceled_input_prevents_partial_batch_retry(self):
        self.start()
        with self.runtime.db() as db:
            db.execute("UPDATE runtime_events SET status='cancelled' WHERE id=?", (self.ids[1],))
        self.complete()
        self.assertTrue(self.runtime.agent(self.agent['id'])['nativeFailureHold'])
        self.assertEqual(self.runtime.delivery_receipt(self.ids[0])['status'], 'delivered')
        self.assertEqual(self.runtime.delivery_receipt(self.ids[1])['status'], 'cancelled')
        self.assertEqual(self.server.calls, self.calls)

    def test_unrelated_unknown_receipt_stays_uncertain_and_is_not_replayed(self):
        self.start()
        with self.runtime.db() as db:
            db.execute("INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch) VALUES (?,?,?,?,?,?,?)",
                       ('other-unknown', self.agent['id'], 'user', 'Private unknown input', 'uncertain', time.time(), self.agent['epoch']))
        self.complete()
        self.assert_pending_batch()
        self.assertEqual(self.runtime.delivery_receipt('other-unknown')['status'], 'uncertain')
        self.runtime.dispatch()
        self.assertEqual(sum(method == 'turn/start' for method, _ in self.server.calls), 1)
        self.assertEqual(self.runtime.delivery_receipt('other-unknown')['status'], 'uncertain')

    def test_exact_scope_and_operation_guards_reject_changed_state(self):
        self.start()
        from codex_claude_input_recovery import recover_rejected_start
        changes = ({'autoWake': False}, {'status': 'paused'}, {'epoch': self.agent['epoch'] + 1},
                   {'accountKey': 'other'}, {'threadId': 'other'}, {'turnId': 'other'},
                   {'deletedAt': 1}, {'workspaceOperation': 'restore'}, {'nativeFailureHold': True})
        for change in changes:
            with self.subTest(change=change), self.runtime.lock, self.runtime.db() as db:
                agent = self.runtime.agent(self.agent['id'], db)
                agent.update(change)
                self.assertIsNone(recover_rejected_start(self.runtime, db, agent, agent['startAttempt'],
                    turn=self.terminal(), account_key='default', connection_id=self.connection))
        for change in ({'action': 'review'}, {'activeAtReservation': True}, {'submitted': False},
                       {'nativeOperationId': 'other-operation'}, {'epoch': self.agent['epoch'] + 1},
                       {'connectionId': 'old-connection'}, {'observedTurnId': 'other-turn'},
                       {'supervisorIdentity': {'handle': 'same', 'generation': 1}}):
            with self.subTest(attempt_change=change), self.runtime.lock, self.runtime.db() as db:
                agent = self.runtime.agent(self.agent['id'], db)
                agent['startAttempt'].update(change)
                self.assertIsNone(recover_rejected_start(self.runtime, db, agent, agent['startAttempt'],
                    turn=self.terminal(), account_key='default', connection_id=self.connection))
        self.assertEqual(self.runtime.agent(self.agent['id'])['startAttempt'], self.attempt)
        self.assertEqual(self.server.calls, self.calls)

    def test_stale_notification_connection_and_turn_cannot_retry(self):
        self.start()
        self.complete(connection='old-connection')
        self.complete(self.terminal(id='unrelated-turn'))
        for key in self.ids:
            self.assertEqual(self.runtime.delivery_receipt(key)['status'], 'delivered')
        self.assertEqual(self.runtime.agent(self.agent['id'])['startAttempt'], self.attempt)
        self.assertEqual(self.server.calls, self.calls)

    def test_stop_and_epoch_change_preserve_user_stop(self):
        self.start()
        self.runtime.stop(self.agent['id'], False)
        before = list(self.server.calls)
        self.complete()
        current = self.runtime.agent(self.agent['id'])
        self.assertFalse(current['autoWake'])
        self.assertEqual(current['status'], 'paused')
        self.assertGreater(current['epoch'], self.agent['epoch'])
        for key in self.ids:
            self.assertNotEqual(self.runtime.delivery_receipt(key)['status'], 'pending')
        self.assertEqual(self.server.calls, before)

    def test_old_attempt_without_frozen_input_cannot_retry(self):
        self.start()
        with self.runtime.lock, self.runtime.db() as db:
            agent = self.runtime.agent(self.agent['id'], db)
            agent['startAttempt'].pop('claudeInputRequest')
            self.runtime.put(db, 'agents', agent)
        self.complete()
        self.assertTrue(self.runtime.agent(self.agent['id'])['nativeFailureHold'])
        self.assertEqual(self.server.calls, self.calls)
        for key in self.ids:
            self.assertEqual(self.runtime.delivery_receipt(key)['status'], 'delivered')

    def test_pending_text_kind_or_assets_edit_holds_exact_retry(self):
        self.start()
        self.complete()
        with self.runtime.db() as db:
            original = dict(db.execute('SELECT * FROM runtime_events WHERE id=?', (self.ids[0],)).fetchone())
            meta = db.execute('SELECT record FROM runtime_event_meta WHERE id=?', (self.ids[0],)).fetchone()[0]
        for change in ('text', 'kind', 'assets'):
            with self.subTest(change=change):
                self.agent_update(self.agent, nativeFailureHold=False, status='queued')
                with self.runtime.db() as db:
                    db.execute('UPDATE runtime_events SET text=?,kind=? WHERE id=?',
                               (original['text'], original['kind'], self.ids[0]))
                    db.execute('UPDATE runtime_event_meta SET record=? WHERE id=?', (meta, self.ids[0]))
                    if change == 'assets':
                        changed = json.loads(meta)
                        changed['assets'] = ['different-asset']
                        db.execute('UPDATE runtime_event_meta SET record=? WHERE id=?',
                                   (json.dumps(changed), self.ids[0]))
                    else:
                        db.execute('UPDATE runtime_events SET ' + change + '=? WHERE id=?',
                                   ('Changed fixture value', self.ids[0]))
                self.runtime.dispatch()
                self.assertTrue(self.runtime.agent(self.agent['id'])['nativeFailureHold'])
                self.assert_pending_batch()
                self.assertEqual(self.server.calls, self.calls)

    def test_changed_config_connection_generation_or_epoch_blocks_frozen_retry(self):
        self.server.supervisor_mode = True
        self.server.proc = SimpleNamespace(root=self.state.resolve(), handle='fixture-native', generation=1)
        self.start()
        self.complete()
        before = self.runtime.agent(self.agent['id'])
        connection = self.runtime.connection_ids['default']
        cases = ({'model': 'different-model'}, {'cwd': str(self.root)},
                 {'epoch': before['epoch'] + 1}, {'yoloMode': not before.get('yoloMode')},
                 {'accountTransferId': 'transfer'}, {'workspaceOperation': 'restore'},
                 {'connection': True}, {'generation': True})
        for change in cases:
            with self.subTest(change=change):
                restored = copy.deepcopy(before)
                restored['nativeFailureHold'] = False
                restored['status'] = 'queued'
                with self.runtime.lock, self.runtime.db() as db:
                    restored.update({key: value for key, value in change.items()
                                     if key not in {'connection', 'generation'}})
                    self.runtime.put(db, 'agents', restored)
                self.runtime.connection_ids['default'] = 'changed' if change.get('connection') else connection
                self.server.proc.generation = 2 if change.get('generation') else 1
                # Existing transfer/workspace gates keep their own state and block admission.
                if change.get('accountTransferId') or change.get('workspaceOperation'):
                    from codex_claude_input_recovery import retry_batch
                    with self.runtime.db() as db:
                        self.assertIsNone(retry_batch(self.runtime, db, restored))
                else:
                    self.runtime.dispatch()
                    self.assertTrue(self.runtime.agent(self.agent['id'])['nativeFailureHold'])
                self.assertEqual(self.server.calls, self.calls)

    def test_late_scope_change_before_submit_holds_without_native_write(self):
        import threading
        self.start()
        self.complete()
        entered, release = threading.Event(), threading.Event()
        prepare = self.runtime.prepare

        def gated(agent):
            entered.set()
            self.assertTrue(release.wait(3))
            return prepare(agent)

        with patch.object(self.runtime, 'prepare', side_effect=gated):
            self.runtime.dispatch()
            self.assertTrue(entered.wait(3))
            self.agent_update(self.agent, yoloMode=not self.agent.get('yoloMode'))
            release.set()
            f.eventually(lambda: self.runtime.agent(self.agent['id']).get('nativeFailureHold'))
        self.assert_pending_batch()
        self.assertEqual(self.server.calls, self.calls)

    def test_retry_lost_response_remains_unknown_without_third_start(self):
        self.start()
        self.complete()
        self.server.fail_start = True
        self.runtime.dispatch()
        f.eventually(lambda: all(self.runtime.delivery_receipt(key)['status'] == 'uncertain' for key in self.ids))
        self.runtime.dispatch()
        self.assertEqual(sum(method == 'turn/start' for method, _ in self.server.calls), 2)
        self.assertIsNone(self.runtime.agent(self.agent['id']).get('claudePreInputRetry'))
        for key in self.ids:
            self.assertEqual(self.runtime.delivery_receipt(key)['status'], 'uncertain')

    def test_stop_while_retry_prepares_preserves_stop_without_submission(self):
        import threading
        self.start()
        self.complete()
        entered, release = threading.Event(), threading.Event()
        prepare = self.runtime.prepare

        def gated(agent):
            entered.set()
            self.assertTrue(release.wait(3))
            return prepare(agent)

        with patch.object(self.runtime, 'prepare', side_effect=gated):
            self.runtime.dispatch()
            self.assertTrue(entered.wait(3))
            self.runtime.stop(self.agent['id'], False)
            release.set()
            f.eventually(lambda: self.runtime.agent(self.agent['id'])['status'] == 'paused'
                         and not self.runtime.agent(self.agent['id']).get('inFlight'))
        current = self.runtime.agent(self.agent['id'])
        self.assertFalse(current['autoWake'])
        self.assertGreater(current['epoch'], self.agent['epoch'])
        self.runtime.dispatch()
        self.assertEqual(sum(method == 'turn/start' for method, _ in self.server.calls), 1)
        for key in self.ids:
            self.assertEqual(self.runtime.delivery_receipt(key)['status'], 'cancelled')

    def stopped_unsent_retry(self):
        import threading
        self.start()
        self.complete()
        entered, release = threading.Event(), threading.Event()
        prepare = self.runtime.prepare

        def gated(agent):
            entered.set()
            self.assertTrue(release.wait(3))
            return prepare(agent)

        with patch.object(self.runtime, 'prepare', side_effect=gated):
            self.runtime.dispatch()
            self.assertTrue(entered.wait(3))
            attempt = copy.deepcopy(self.runtime.agent(self.agent['id'])['startAttempt'])
            self.runtime.stop(self.agent['id'], False)
            release.set()
            f.eventually(lambda: not self.runtime.agent(self.agent['id']).get('inFlight'))
        return attempt

    def test_stop_during_retry_prepare_then_manual_send_retires_exact_unsent_reservation(self):
        attempt = self.stopped_unsent_retry()
        self.assertIs(attempt['submitted'], False)
        for key in self.ids:
            self.assertEqual(self.runtime.delivery_receipt(key)['status'], 'reserved')
        self.runtime.send(self.agent['id'], 'The user resumes after preparation Stop', 'resume-after-prepare', manual=True)
        self.runtime.dispatch()
        f.eventually(lambda: self.runtime.delivery_receipt('resume-after-prepare')['status'] == 'delivered')
        current = self.runtime.agent(self.agent['id'])
        self.assertEqual(current['startAttempt']['events'], ['resume-after-prepare'])
        self.assertFalse(current.get('nativeFailureHold'))
        self.assertIsNone(current.get('claudePreInputRetry'))
        with self.runtime.db() as db:
            saved = json.loads(db.execute('SELECT record FROM runtime_execution_attempts WHERE id=?',
                                          (attempt['id'],)).fetchone()[0])
        self.assertIs(saved['submitted'], False)
        self.assertEqual(saved['executionOutcome'], 'unsent')
        self.assertEqual(saved['notSubmittedReason'], 'Cancelled by Stop before Claude retry input submission')
        starts = [params for method, params in self.server.calls if method == 'turn/start']
        self.assertEqual(len(starts), 2)
        self.assertEqual(starts[1]['clientUserMessageId'], 'resume-after-prepare')
        for key in self.ids:
            self.assertEqual(self.runtime.delivery_receipt(key)['status'], 'cancelled')

    def test_stopped_unknown_or_mismatched_retry_reservation_is_not_retired(self):
        self.stopped_unsent_retry()
        from codex_claude_input_recovery import retire_stopped_retry
        controls = ({'submitted': True}, {'nativeOperationId': 'unconfirmed-native-operation'},
                    {'observedTurnId': 'unconfirmed-turn'}, {'events': [self.ids[0]]},
                    {'claudeRetryOf': 'other-rejection'}, {'action': 'review'},
                    {'accountKey': 'other'}, {'executionOutcome': 'unknown'})
        for change in controls:
            with self.subTest(change=change), self.runtime.lock, self.runtime.db() as db:
                db.execute('SAVEPOINT negative_control')
                agent = self.runtime.agent(self.agent['id'], db)
                agent['startAttempt'].update(change)
                self.runtime.put(db, 'agents', agent)
                self.assertFalse(retire_stopped_retry(self.runtime, db, agent))
                self.assertTrue(agent.get('claudePreInputRetry'))
                for key in self.ids:
                    self.assertEqual(db.execute('SELECT status FROM runtime_events WHERE id=?', (key,)).fetchone()[0], 'reserved')
                db.execute('ROLLBACK TO negative_control')
                db.execute('RELEASE negative_control')
        with self.runtime.lock, self.runtime.db() as db:
            agent = self.runtime.agent(self.agent['id'], db)
            db.execute("UPDATE runtime_events SET status='uncertain' WHERE id=?", (self.ids[1],))
            self.assertFalse(retire_stopped_retry(self.runtime, db, agent))
            self.assertEqual(db.execute('SELECT status FROM runtime_events WHERE id=?', (self.ids[0],)).fetchone()[0], 'reserved')
            self.assertEqual(db.execute('SELECT status FROM runtime_events WHERE id=?', (self.ids[1],)).fetchone()[0], 'uncertain')
        self.assertEqual(sum(method == 'turn/start' for method, _ in self.server.calls), 1)

    def test_old_dispatch_frame_cannot_submit_a_merged_rejected_batch(self):
        import subprocess
        self.start()
        self.runtime.send(self.agent['id'], 'Private later input', 'later', manual=False)
        self.complete()
        source = getattr(self, 'older_dispatch_source', None)
        if source is None:
            source = subprocess.check_output(['git', 'show', '8302f1e0:scripts/codex_runtime.py'], cwd=ROOT)
        older, _ = source_function(source, ('Runtime', 'dispatch_candidates'), vars(codex_runtime), '<older-dispatch>')
        with patch.object(type(self.runtime), 'dispatch_candidates', older):
            self.runtime.dispatch()
            f.eventually(lambda: self.runtime.agent(self.agent['id']).get('nativeFailureHold'))
        self.assert_pending_batch()
        self.assertEqual(self.runtime.delivery_receipt('later')['status'], 'pending')
        self.assertEqual(sum(method == 'turn/start' for method, _ in self.server.calls), 1)

    def test_manual_resume_after_stop_starts_only_new_input(self):
        self.start()
        self.complete()
        self.runtime.stop(self.agent['id'], False)
        for key in self.ids:
            self.assertEqual(self.runtime.delivery_receipt(key)['status'], 'cancelled')
        self.runtime.send(self.agent['id'], 'The user resumes after Stop', 'new-after-stop', manual=True)
        self.runtime.dispatch()
        f.eventually(lambda: self.runtime.delivery_receipt('new-after-stop')['status'] == 'delivered')
        current = self.runtime.agent(self.agent['id'])
        self.assertFalse(current.get('nativeFailureHold'))
        self.assertIsNone(current.get('claudePreInputRetry'))
        self.assertEqual(current['startAttempt']['events'], ['new-after-stop'])
        starts = [params for method, params in self.server.calls if method == 'turn/start']
        self.assertEqual(len(starts), 2)
        self.assertEqual(starts[1]['clientUserMessageId'], 'new-after-stop')
        for key in self.ids:
            self.assertEqual(self.runtime.delivery_receipt(key)['status'], 'cancelled')


if __name__ == '__main__':
    unittest.main()
