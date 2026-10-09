#!/usr/bin/env python3
"""Exact-turn recovery reads complete receipts without unrelated item bodies."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import copy
import importlib.util
import json
import os
from pathlib import Path
import sys
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'scripts'))
from codex_connection_recovery import native_operations_settled
from codex_native_errors import NativeRpcError
from codex_turn_recovery import read_native_turn


def load_fixture(name, filename):
    spec = importlib.util.spec_from_file_location(name, ROOT / 'tests' / filename)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class Native:
    def __init__(self):
        self.turns = [{'id': 'newer-' + str(index), 'status': 'completed', 'items': [
            {'id': 'foreign-' + str(index), 'type': 'agentMessage', 'text': 'Unrelated output'}]}
            for index in range(31)] + [{'id': 'target', 'status': 'completed', 'error': None,
                'clientUserMessageId': 'exact-input', 'startOutcome': 'accepted', 'items': [
                    {'id': 'user', 'type': 'userMessage', 'clientId': 'exact-input', 'content': []},
                    {'id': 'command', 'type': 'commandExecution', 'status': 'completed',
                     'exitCode': 0, 'aggregatedOutput': 'Exact command result'},
                    {'id': 'answer', 'type': 'agentMessage', 'phase': 'final_answer',
                     'text': 'Exact final answer'}]}]
        self.entries = [{'turnId': 'target', 'item': copy.deepcopy(item)} for item in self.turns[-1]['items']]
        self.calls = []
        self.returned_foreign_items = 0
        self.item_page_size = 2
        self.item_error = None
        self.transform = None
        self.view_override = None

    def call(self, method, params, timeout=60):
        self.calls.append((method, copy.deepcopy(params), timeout))
        if method == 'thread/turns/list':
            offset = int(params.get('cursor', 0))
            end = offset + params['limit']
            turns = copy.deepcopy(self.turns[offset:end])
            for turn in turns:
                view = self.view_override or params['itemsView']
                if view == 'notLoaded':
                    turn['items'] = []
                elif view == 'summary':
                    turn['items'] = [item for item in turn['items'] if item['type'] in {'userMessage', 'agentMessage'}]
                if view != 'legacy':
                    turn['itemsView'] = view
                self.returned_foreign_items += sum(item['id'].startswith('foreign-') for item in turn['items'])
            result = {'data': turns, 'nextCursor': str(end) if end < len(self.turns) else None}
        elif method == 'thread/items/list':
            if params['threadId'] != 'thread' or params['turnId'] != 'target':
                raise AssertionError('The item read must keep the exact target scope')
            if self.item_error:
                raise self.item_error
            offset = int(params.get('cursor', 0))
            end = offset + self.item_page_size
            result = {'data': copy.deepcopy(self.entries[offset:end]),
                      'nextCursor': str(end) if end < len(self.entries) else None}
        else:
            raise AssertionError('Recovery must not submit a native mutation: ' + method)
        return self.transform(method, params, result) if self.transform else result


class NativeTurnItemsContract(unittest.TestCase):
    def test_later_target_reads_only_its_complete_items_and_receipts(self):
        native = Native()
        result = read_native_turn(native, 'thread', 'target')
        self.assertEqual(result['items'], native.turns[-1]['items'])
        self.assertEqual((result['clientUserMessageId'], result['startOutcome']), ('exact-input', 'accepted'))
        self.assertEqual(result['itemsView'], 'full')
        self.assertEqual(native.returned_foreign_items, 0)
        self.assertTrue(native_operations_settled(result))
        turn_calls = [params for method, params, _ in native.calls if method == 'thread/turns/list']
        self.assertEqual([params.get('cursor') for params in turn_calls], [None, '10', '20', '30', '30'])
        self.assertTrue(all(params['itemsView'] == 'notLoaded' for params in turn_calls))
        item_calls = [params for method, params, _ in native.calls if method == 'thread/items/list']
        self.assertEqual([params.get('cursor') for params in item_calls], [None, '2'])

    def test_summary_and_bare_items_keep_unsettled_native_receipt(self):
        native = Native()
        native.view_override = 'summary'
        native.turns[-1]['items'][1].update(status='inProgress', exitCode=None)
        native.entries = copy.deepcopy(native.turns[-1]['items'])
        result = read_native_turn(native, 'thread', 'target')
        self.assertEqual(result['items'], native.turns[-1]['items'])
        self.assertFalse(native_operations_settled(result))

    def test_full_or_legacy_provider_response_needs_no_item_api(self):
        for view in ('full', 'legacy'):
            with self.subTest(view=view):
                native = Native()
                native.view_override = view
                result = read_native_turn(native, 'thread', 'target')
                self.assertEqual(result['items'], native.turns[-1]['items'])
                self.assertFalse(any(method == 'thread/items/list' for method, _, _ in native.calls))

    def test_explicit_unsupported_reads_only_the_found_full_page(self):
        native = Native()
        native.item_error = NativeRpcError({'code': -32601, 'message': 'thread/items/list is not supported yet'})
        result = read_native_turn(native, 'thread', 'target')
        self.assertEqual(result['items'], native.turns[-1]['items'])
        full = [params for method, params, _ in native.calls
                if method == 'thread/turns/list' and params['itemsView'] == 'full']
        self.assertEqual(full, [{'threadId': 'thread', 'limit': 10, 'sortDirection': 'desc',
                                 'itemsView': 'full', 'cursor': '30'}])
        self.assertEqual(native.returned_foreign_items, 1)

    def test_timeout_and_other_rpc_errors_never_use_full_fallback(self):
        for error in (TimeoutError('The native read timed out'),
                      NativeRpcError({'code': -32000, 'message': 'thread/items/list is not supported yet'})):
            with self.subTest(error=error):
                native = Native()
                native.item_error = error
                with self.assertRaises(type(error)):
                    read_native_turn(native, 'thread', 'target')
                self.assertTrue(all(params.get('itemsView') != 'full' for _, params, _ in native.calls))

    def test_one_deadline_covers_item_pages_and_confirmation(self):
        native = Native()
        native.turns = native.turns[-1:]
        now = [100.0]
        def advance(method, _params, result):
            now[0] += 9
            return result
        native.transform = advance
        with patch('codex_turn_recovery.time.monotonic', side_effect=lambda: now[0]):
            with self.assertRaises(TimeoutError):
                read_native_turn(native, 'thread', 'target')
        self.assertEqual([timeout for _, _, timeout in native.calls], [10, 10, 2])
        self.assertEqual(len(native.calls), 3)

    def test_missing_exact_turn_never_reads_items(self):
        native = Native()
        self.assertIsNone(read_native_turn(native, 'thread', 'missing'))
        self.assertFalse(any(method == 'thread/items/list' for method, _, _ in native.calls))

    def test_malformed_turn_pages_and_identity_changes_fail_closed(self):
        for change in ('page', 'data', 'turn', 'id', 'thread', 'items', 'view', 'duplicate'):
            with self.subTest(change=change):
                native = Native()
                native.turns = native.turns[-1:]
                def corrupt(method, _params, result):
                    if method != 'thread/turns/list':
                        return result
                    if change == 'page': return None
                    if change == 'data': return {'data': None}
                    if change == 'turn': result['data'] = ['bad']
                    elif change == 'duplicate': result['data'] *= 2
                    else:
                        field, value = {'id': ('id', ''), 'thread': ('threadId', 'other'),
                                        'items': ('items', None), 'view': ('itemsView', 'unknown')}[change]
                        result['data'][0][field] = value
                    return result
                native.transform = corrupt
                with self.assertRaises(ValueError):
                    read_native_turn(native, 'thread', 'target')
                self.assertEqual(len(native.calls), 1)

    def test_malformed_item_pages_and_source_changes_fail_closed(self):
        for change in ('page', 'data', 'entry', 'turn', 'thread', 'item', 'id', 'type', 'bareTurn', 'bareThread'):
            with self.subTest(change=change):
                native = Native()
                def corrupt(method, _params, result):
                    if method != 'thread/items/list': return result
                    if change == 'page': return None
                    if change == 'data': return {'data': None}
                    if change == 'entry': result['data'] = ['bad']
                    elif change in ('turn', 'thread', 'item'):
                        field, value = {'turn': ('turnId', 'other'), 'thread': ('threadId', 'other'),
                                        'item': ('item', None)}[change]
                        result['data'][0][field] = value
                    else:
                        item = result['data'][0]['item']
                        field, value = {'id': ('id', ''), 'type': ('type', None),
                                        'bareTurn': ('turnId', 'other'), 'bareThread': ('threadId', 'other')}[change]
                        item[field] = value
                        if change.startswith('bare'): result['data'][0] = item
                    return result
                native.transform = corrupt
                with self.assertRaises(ValueError):
                    read_native_turn(native, 'thread', 'target')

    def test_identical_items_dedupe_but_conflicting_receipts_fail(self):
        native = Native()
        native.entries.insert(2, copy.deepcopy(native.entries[1]))
        self.assertEqual(read_native_turn(native, 'thread', 'target')['items'], native.turns[-1]['items'])
        native = Native()
        duplicate = copy.deepcopy(native.entries[1])
        duplicate['item']['exitCode'] = 1
        native.entries.insert(2, duplicate)
        with self.assertRaisesRegex(ValueError, 'conflicting item receipts'):
            read_native_turn(native, 'thread', 'target')

    def test_repeated_or_malformed_cursors_fail_closed(self):
        for method in ('thread/turns/list', 'thread/items/list'):
            for cursor in ('repeat', 42, ''):
                with self.subTest(method=method, cursor=cursor):
                    native = Native()
                    def corrupt(actual, _params, result):
                        if actual == method: result['nextCursor'] = '0' if cursor == 'repeat' else cursor
                        return result
                    native.transform = corrupt
                    with self.assertRaises(ValueError):
                        read_native_turn(native, 'thread', 'target')
                    self.assertLessEqual(len(native.calls), 6)

    def test_target_metadata_recheck_rejects_changed_outcome_error_or_identity(self):
        for change in ('status', 'error', 'id'):
            with self.subTest(change=change):
                native = Native()
                def corrupt(method, _params, result):
                    if method == 'thread/turns/list' and any(m == 'thread/items/list' for m, _, _ in native.calls):
                        result['data'][-1][change] = {'status': 'inProgress', 'error': {'message': 'changed'}, 'id': 'other'}[change]
                    return result
                native.transform = corrupt
                with self.assertRaisesRegex(ValueError, 'target turn changed'):
                    read_native_turn(native, 'thread', 'target')

    def test_unsupported_fallback_never_returns_a_missing_or_partial_turn(self):
        for change in ('id', 'itemsView'):
            with self.subTest(change=change):
                native = Native()
                native.item_error = NativeRpcError({'code': -32601, 'message': 'Method not found'})
                def corrupt(method, params, result):
                    if method == 'thread/turns/list' and params['itemsView'] == 'full':
                        result['data'][-1][change] = 'other' if change == 'id' else 'summary'
                    return result
                native.transform = corrupt
                with self.assertRaises(ValueError):
                    read_native_turn(native, 'thread', 'target')

    def test_real_reconcile_turn_caller_repairs_full_answer_once(self):
        fixture = load_fixture('target_turn_fixture', 'turn-recovery-contract.py')
        case = fixture.TurnRecoveryContract('test_lost_completion_repairs_partial_text_and_is_idempotent')
        case.setUp()
        self.addCleanup(case.tearDown)
        original = case.server.call
        def call(method, params, timeout=60):
            if method == 'thread/items/list':
                case.server.calls.append((method, params))
                self.assertEqual((params['threadId'], params['turnId']), (case.a['threadId'], case.turn))
                return {'data': [{'turnId': case.turn, 'item': item} for item in case.server.native['turns'][0]['items']],
                        'nextCursor': None}
            result = original(method, params, timeout)
            if method == 'thread/turns/list' and params['itemsView'] == 'notLoaded':
                result = copy.deepcopy(result)
                for turn in result['data']: turn.update(items=[], itemsView='notLoaded')
            return result
        case.server.call = call
        result = case.runtime.reconcile_turn(case.key)
        self.assertEqual(result['outcome'], 'completed')
        self.assertEqual(case.runtime.agent(case.key)['lastAnswer'], 'Full final answer')
        self.assertEqual(case.runtime.reconcile_turn(case.key)['status'], 'skipped')
        with case.runtime.db() as db:
            self.assertEqual(db.execute('SELECT count(*) FROM runtime_completed_turns').fetchone()[0], 1)

    def test_real_caller_rechecks_stop_epoch_account_and_connection_after_items(self):
        fixture = load_fixture('target_turn_race_fixture', 'turn-recovery-contract.py')
        for change in ('stop', 'epoch', 'account', 'connection'):
            with self.subTest(change=change):
                case = fixture.TurnRecoveryContract('test_lost_completion_repairs_partial_text_and_is_idempotent')
                case.setUp()
                try:
                    original = case.server.call
                    completed_after_change = []
                    def call(method, params, timeout=60):
                        if method == 'thread/items/list':
                            case.server.calls.append((method, params))
                            if change == 'stop':
                                case.runtime.stop(case.key, descendants=False)
                            elif change == 'connection':
                                with case.runtime.lock:
                                    case.runtime.connection_ids['default'] = 'replacement'
                            else:
                                with case.runtime.lock, case.runtime.db() as db:
                                    agent = case.runtime.agent(case.key, db)
                                    if change == 'epoch': agent['epoch'] += 1
                                    else: agent['accountKey'] = 'other'
                                    case.runtime.put(db, 'agents', agent)
                            with case.runtime.read_db() as db:
                                completed_after_change.append(db.execute('SELECT count(*) FROM runtime_completed_turns').fetchone()[0])
                            return {'data': [{'turnId': case.turn, 'item': item}
                                             for item in case.server.native['turns'][0]['items']], 'nextCursor': None}
                        result = original(method, params, timeout)
                        if method == 'thread/turns/list' and params['itemsView'] == 'notLoaded':
                            result = copy.deepcopy(result)
                            for turn in result['data']: turn.update(items=[], itemsView='notLoaded')
                        return result
                    case.server.call = call
                    starts = len([1 for method, _ in case.server.calls if method == 'turn/start'])
                    self.assertEqual(case.runtime.reconcile_turn(case.key)['status'], 'superseded')
                    self.assertEqual(len([1 for method, _ in case.server.calls if method == 'turn/start']), starts)
                    self.assertEqual(len(completed_after_change), 1)
                    self.assertNotIn('turnRecovery', case.runtime.agent(case.key))
                    with case.runtime.db() as db:
                        self.assertEqual(db.execute('SELECT count(*) FROM runtime_completed_turns').fetchone()[0], completed_after_change[0])
                finally:
                    case.tearDown()


@unittest.skipUnless(os.environ.get('CODEX_BIN'), 'Set CODEX_BIN to check an isolated native executable')
class NativeTurnItemsProtocol(unittest.TestCase):
    def test_exact_installed_protocol_pages_and_recovery_keep_full_items(self):
        fixture = load_fixture('target_native_protocol_fixture', 'native-primitives-integration.py')
        with fixture.native_server() as (server, thread, provider, notifications, _):
            provider.release.set()
            turns = []
            for index in range(12):
                turn = server.call('turn/start', {'threadId': thread,
                    'clientUserMessageId': 'local-input-' + str(index),
                    'input': [{'type': 'text', 'text': 'Isolated turn items fixture ' + str(index)}]})['turn']['id']
                fixture.n.until(lambda: any(event.get('method') == 'turn/completed'
                    and event['params']['turn']['id'] == turn for event in notifications), 'isolated fixture completion')
                turns.append(turn)
            pages, cursor = [], None
            while True:
                params = {'threadId': thread, 'turnId': turns[0], 'limit': 1, 'sortDirection': 'asc'}
                if cursor is not None: params['cursor'] = cursor
                page = server.call('thread/items/list', params, timeout=5)
                pages.extend(page['data'])
                cursor = page.get('nextCursor')
                if cursor is None: break
                self.assertLess(len(pages), 10)
            self.assertGreater(len(pages), 1)
            exact = [entry.get('item', entry) for entry in pages]
            for entry in pages:
                if 'item' in entry: self.assertEqual(entry['turnId'], turns[0])
            submit = server.call
            calls = []
            def observed(method, params, timeout=60):
                self.assertIn(method, {'thread/turns/list', 'thread/items/list'})
                calls.append((method, copy.deepcopy(params)))
                return submit(method, params, timeout)
            with patch.object(server, 'call', side_effect=observed):
                result = read_native_turn(server, thread, turns[0])
            self.assertEqual(result['items'], exact)
            self.assertEqual((result['id'], result['status'], result['itemsView']), (turns[0], 'completed', 'full'))
            self.assertTrue(all(params['itemsView'] == 'notLoaded' for method, params in calls
                                if method == 'thread/turns/list'))
            self.assertTrue(all(params['turnId'] == turns[0] for method, params in calls
                                if method == 'thread/items/list'))
            self.assertEqual(len(provider.requests), 12)
            self.assertEqual(provider.unexpected, [])
            print(json.dumps({'nativeTurnItemsPages': len(pages), 'exactItems': len(exact),
                              'recoveryReadCalls': len(calls), 'externalRequests': []}))


if __name__ == '__main__':
    unittest.main()
