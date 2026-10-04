#!/usr/bin/env python3
"""Live request and cost changes preserve callbacks and source guards."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

from contextlib import contextmanager
import copy
import importlib.util
import io
import json
from pathlib import Path
import queue
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time
from types import FunctionType, ModuleType
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'scripts'))
import codex_request_cost_update as update
import codex_runtime
import codex_session_costs
from codex_source import signature, source_function

spec = importlib.util.spec_from_file_location('stale_ack_contract', ROOT / 'tests/stale-tool-request-ack-contract.py')
stale_ack = importlib.util.module_from_spec(spec)
spec.loader.exec_module(stale_ack)


@contextmanager
def reviewed_fixture():
    for name, *_ in update.DEPENDENCIES:
        importlib.import_module(name)
    with tempfile.TemporaryDirectory(prefix='studio-request-cost-update-') as folder:
        scripts = Path(folder)
        modules = {}
        for name, real in (('codex_runtime', codex_runtime), ('codex_session_costs', codex_session_costs)):
            path = scripts / (name + '.py')
            path.write_bytes((ROOT / 'scripts' / path.name).read_bytes())
            module = ModuleType(name)
            vars(module).update(vars(real))
            module.__file__ = str(path)
            baseline = subprocess.check_output(['git', 'show', '36f1143d:scripts/' + path.name], cwd=ROOT)
            for entry in update.FUNCTIONS:
                source, owner, method, *_ = entry
                if source != name:
                    continue
                if owner not in module.__dict__ or getattr(module, owner) is getattr(real, owner):
                    setattr(module, owner, type(owner, (getattr(real, owner),), {}))
                old, _ = source_function(baseline, [owner, method], vars(module), '<baseline-36f1143d>')
                setattr(getattr(module, owner), method, old)
            modules[name] = module
        # Keep AppServer callbacks in the isolated module namespace as well.
        app = modules['codex_runtime'].AppServer
        current = codex_runtime.AppServer.dispatch
        app.dispatch = FunctionType(current.__code__, vars(modules['codex_runtime']),
                                    current.__name__, current.__defaults__)
        runtime = modules['codex_runtime'].Runtime.__new__(modules['codex_runtime'].Runtime)
        runtime.lock = threading.RLock()
        runtime.closed = False
        runtime.changed = threading.Event()
        runtime.connection_ids = {'default': 'new'}
        runtime.offline_accounts = set()
        runtime.servers = {}
        with (patch.dict(sys.modules, modules),
              patch.object(update, '__file__', str(scripts / 'codex_request_cost_update.py'))):
            yield runtime, modules, scripts


@contextmanager
def actual_runtime_fixture():
    with reviewed_fixture() as (_, modules, scripts):
        case = stale_ack.StaleToolRequestAckContract()
        case.setUp()
        case.runtime.__class__ = modules['codex_runtime'].Runtime
        try:
            yield case, modules, scripts
        finally:
            case.doCleanups()


def native_dispatcher(runtime, frames, account_key='default', connection_id='old'):
    app = sys.modules['codex_runtime'].AppServer
    server = app.__new__(app)
    server.lock = threading.RLock()
    server.callback_lock = threading.RLock()
    server.callbacks = queue.Queue()
    server.reader_done = threading.Event()
    server.reader_done.set()
    server.dispatcher_done = threading.Event()
    server.closed = False
    server.transport_error = None
    server.supervisor_mode = True
    server.supervisor_event_applied = None
    server.supervisor_commit = None
    server.proc = stale_ack.JournalProxy(frames)
    server.proc._ack_lock = threading.RLock()
    server.request = lambda message: runtime.request(message, account_key, connection_id)
    server.notification = lambda _message: None
    server.release_slot = lambda _message: False
    server.close_log_if_idle = lambda: None
    server.errors = []
    server.protocol_error = lambda error: server.errors.append(error)
    server.died = lambda: None
    server.log = io.BytesIO()
    for sequence, message in frames.remaining():
        message['_studioSupervisorSequence'] = sequence
        server.callbacks.put((server.request if 'id' in message else server.notification, message))
    return server


def wait_for_frame(thread, code):
    deadline = time.monotonic() + 2
    while time.monotonic() < deadline:
        frame = sys._current_frames().get(thread.ident)
        while frame is not None:
            if frame.f_code is code:
                return
            frame = frame.f_back
        if not thread.is_alive():
            break
        threading.Event().wait(.005)
    raise AssertionError('The gated old frame did not reach its boundary')


def finish_dispatch(thread, server):
    thread.join(2)
    if thread.is_alive() or not server.dispatcher_done.is_set():
        raise AssertionError('The isolated native dispatcher did not finish')


class RequestCostUpdateContract(unittest.TestCase):
    def test_old_dynamic_preclaim_cannot_execute_across_stop_and_resume(self):
        for change_after_apply in (False, True):
            with self.subTest(change_after_apply=change_after_apply), actual_runtime_fixture() as (case, _, _):
                runtime = case.runtime
                runtime.reserve_tool_request(case.message, 'default', 'old')
                old = runtime.dynamic.__func__
                # A historical callback is not the current function object.
                historical = FunctionType(old.__code__.replace(co_filename='<old dynamic patch>'),
                                          old.__globals__, old.__name__, old.__defaults__)
                thread = threading.Thread(target=historical, args=(runtime, case.message, 'default', 'old'))
                with runtime.lock:
                    thread.start()
                    wait_for_frame(thread, runtime.reserve_tool_request.__func__.__code__)
                    if not change_after_apply:
                        case.update_actor(epoch=4, turnEpoch=4, autoWake=True)
                    self.assertEqual(update.apply(runtime)['status'], 'applied')
                    if change_after_apply:
                        case.update_actor(epoch=4, turnEpoch=4, autoWake=True)
                thread.join(2)
                self.assertFalse(thread.is_alive())
                self.assertEqual(case.operations, 0)
                record = runtime.tool_request(case.key)
                self.assertEqual((record['epoch'], record['stage'], record['outcome']),
                                 (3, 'failed', 'not_applied'))
                self.assertEqual(record['result']['contentItems'][0]['text'],
                                 'This tool call belongs to an earlier turn')
                self.assertEqual(case.sink.messages[-1]['result'], record['result'])

    def test_code_swap_without_claim_fence_executes_the_historical_dynamic_frame(self):
        with actual_runtime_fixture() as (case, _, _):
            runtime = case.runtime
            runtime.reserve_tool_request(case.message, 'default', 'old')
            thread = threading.Thread(target=runtime.dynamic, args=(case.message, 'default', 'old'))
            with runtime.lock:
                thread.start()
                wait_for_frame(thread, runtime.reserve_tool_request.__func__.__code__)
                with patch.object(update._OldDynamicClaimFence, 'install', lambda _fence: None):
                    self.assertEqual(update.apply(runtime)['status'], 'applied')
                case.update_actor(epoch=4, turnEpoch=4, autoWake=True)
            thread.join(2)
            self.assertFalse(thread.is_alive())
            self.assertEqual(case.operations, 1)
            self.assertEqual(runtime.tool_request(case.key)['epoch'], 3)
            self.assertEqual(runtime.tool_request(case.key)['outcome'], 'applied')

    def test_old_dynamic_preserves_unknown_running_operation_and_saved_receipts(self):
        for evidence in ('unknown', 'started', 'running', 'operation', 'result'):
            with self.subTest(evidence=evidence), actual_runtime_fixture() as (case, _, _):
                runtime = case.runtime
                record = runtime.reserve_tool_request(case.message, 'default', 'old')
                if evidence == 'unknown':
                    record['outcome'] = 'unknown'
                elif evidence == 'started':
                    record['started'] = 1
                elif evidence == 'running':
                    record.update(stage='running', started=1, outcome='unknown')
                with runtime.lock, runtime.db() as db:
                    runtime.put(db, 'tool_requests', record)
                    if evidence == 'operation':
                        db.execute('CREATE TABLE IF NOT EXISTS runtime_operation_receipts '
                                   '(id TEXT PRIMARY KEY,signature TEXT,result TEXT)')
                        db.execute('INSERT INTO runtime_operation_receipts(id,signature,result) VALUES (?,?,?)',
                                   (case.key, 'fixture-operation', '{"accepted":true}'))
                    elif evidence == 'result':
                        saved = {'success': True, 'contentItems': [{'type': 'inputText', 'text': 'Exact saved result'}]}
                        db.execute('INSERT INTO runtime_tool_results VALUES (?,?)', (case.key, json.dumps(saved)))
                original = copy.deepcopy(runtime.tool_request(case.key))
                thread = threading.Thread(target=runtime.dynamic, args=(case.message, 'default', 'old'))
                with runtime.lock:
                    thread.start()
                    wait_for_frame(thread, runtime.reserve_tool_request.__func__.__code__)
                    self.assertEqual(update.apply(runtime)['status'], 'applied')
                    case.update_actor(epoch=4, turnEpoch=4, autoWake=True)
                thread.join(2)
                self.assertFalse(thread.is_alive())
                self.assertEqual(case.operations, 0)
                self.assertEqual(runtime.tool_request(case.key), original)
                if evidence == 'result':
                    self.assertEqual(case.sink.messages[-1]['result'], saved)

    def test_old_dispatch_code_after_request_return_is_recognized(self):
        for copied_globals in (False, True):
            with self.subTest(copied_globals=copied_globals), actual_runtime_fixture() as (case, _, _):
                runtime = case.runtime
                runtime.connection_ids['default'] = 'new'
                frames = stale_ack.RetainedFrames(case.message)
                server = native_dispatcher(runtime, frames)
                current = sys.modules['codex_runtime'].AppServer.dispatch
                namespace = dict(current.__globals__) if copied_globals else current.__globals__
                historical = FunctionType(current.__code__.replace(co_filename='<prior hotpatch>'),
                                          namespace, current.__name__, current.__defaults__)
                self.assertIsNot(historical.__code__, current.__code__)
                thread = threading.Thread(target=historical, args=(server,))
                with server.proc._ack_lock:
                    thread.start()
                    wait_for_frame(thread, server.proc.ack.__func__.__code__)
                    self.assertEqual(update.apply(runtime)['status'], 'applied')
                finish_dispatch(thread, server)
                self.assertEqual(frames.acknowledged, 6)
                self.assertEqual(server.proc.ack_calls, [])
                self.assertIsNone(runtime.tool_request(case.key))

    def test_historical_dispatch_for_another_runtime_gets_no_ack_guard(self):
        with actual_runtime_fixture() as (case, modules, _):
            other = stale_ack.StaleToolRequestAckContract()
            other.setUp()
            other.runtime.__class__ = modules['codex_runtime'].Runtime
            try:
                other.runtime.connection_ids['default'] = 'new'
                frames = stale_ack.RetainedFrames(other.message)
                server = native_dispatcher(other.runtime, frames)
                current = sys.modules['codex_runtime'].AppServer.dispatch
                historical = FunctionType(current.__code__.replace(co_filename='<prior hotpatch>'),
                                          current.__globals__, current.__name__, current.__defaults__)
                thread = threading.Thread(target=historical, args=(server,))
                with server.proc._ack_lock:
                    thread.start()
                    wait_for_frame(thread, server.proc.ack.__func__.__code__)
                    self.assertEqual(update.apply(case.runtime)['status'], 'applied')
                    self.assertNotIn('_studio_request_ack_guard', vars(server.proc))
                    self.assertNotIn('ack', vars(server.proc))
                    self.assertNotIn('call', vars(server.proc))
                    self.assertNotIn('begin_tool_request', vars(other.runtime))
                finish_dispatch(thread, server)
            finally:
                other.doCleanups()

    def test_transport_published_before_swap_is_protected_by_the_second_scan(self):
        with actual_runtime_fixture() as (case, _, _):
            runtime = case.runtime
            frames = [stale_ack.RetainedFrames(case.message) for _ in range(2)]
            servers = [native_dispatcher(runtime, ledger) for ledger in frames]
            threads = [threading.Thread(target=server.dispatch) for server in servers]
            original_protect = update._RequestAckBarrier.protect
            calls = 0
            def publish_transport(barrier):
                nonlocal calls
                calls += 1
                original_protect(barrier)
                if calls == 1:
                    threads[1].start()
                    wait_for_frame(threads[1], runtime.reserve_tool_request.__func__.__code__)
                elif calls == 2:
                    runtime.connection_ids['default'] = 'new'
            with runtime.lock:
                threads[0].start()
                wait_for_frame(threads[0], runtime.reserve_tool_request.__func__.__code__)
                with patch.object(update._RequestAckBarrier, 'protect', publish_transport):
                    self.assertEqual(update.apply(runtime)['status'], 'applied')
            for thread, server, ledger in zip(threads, servers, frames):
                finish_dispatch(thread, server)
                self.assertEqual(ledger.acknowledged, 6)
                self.assertIn('_studio_request_ack_guard', vars(server.proc))
            self.assertIsNone(runtime.tool_request(case.key))
            self.assertEqual(case.operations, 0)

    def test_read_frame_after_normal_old_return_is_retained(self):
        with actual_runtime_fixture() as (case, _, _):
            runtime = case.runtime
            message = copy.deepcopy(case.message)
            message['params'].update(callId='fixture-read', tool='orchestration_request',
                                     arguments={'action': 'list'})
            key = runtime.tool_request_key(message)
            runtime.connection_ids['default'] = 'new'
            frames = stale_ack.RetainedFrames(message)
            server = native_dispatcher(runtime, frames)
            thread = threading.Thread(target=server.dispatch)
            with server.proc._ack_lock:
                thread.start()
                wait_for_frame(thread, server.proc.ack.__func__.__code__)
                self.assertEqual(update.apply(runtime)['status'], 'applied')
            finish_dispatch(thread, server)
            self.assertEqual(frames.acknowledged, 6)
            self.assertIsNone(runtime.tool_request(key))
            self.assertEqual(case.operations, 0)

    def test_retained_old_frame_does_not_override_stop_or_account_transfer(self):
        for change in ({'epoch': 4, 'turnEpoch': 4, 'autoWake': False},
                       {'accountKey': 'different-account'}):
            with self.subTest(change=change), actual_runtime_fixture() as (case, _, _):
                runtime = case.runtime
                frames = stale_ack.RetainedFrames(case.message)
                server = native_dispatcher(runtime, frames)
                thread = threading.Thread(target=server.dispatch)
                with runtime.lock:
                    thread.start()
                    wait_for_frame(thread, runtime.reserve_tool_request.__func__.__code__)
                    case.update_actor(**change)
                    runtime.connection_ids['default'] = 'new'
                    self.assertEqual(update.apply(runtime)['status'], 'applied')
                finish_dispatch(thread, server)
                self.assertEqual(frames.acknowledged, 6)
                self.assertEqual(case.operations, 0)
                replacement = native_dispatcher(runtime, frames, connection_id='new')
                replacement.dispatch()
                self.assertEqual(frames.acknowledged, 7)
                self.assertEqual(case.operations, 0)
                actor = runtime.agent(case.actor['id'])
                self.assertTrue(all(actor[key] == value for key, value in change.items()))

    def test_busy_ack_rejects_before_the_code_or_callback_changes(self):
        with actual_runtime_fixture() as (case, modules, _):
            runtime = case.runtime
            runtime.connection_ids['default'] = 'new'
            server = native_dispatcher(runtime, stale_ack.RetainedFrames(case.message))
            runtime._late_servers = [server]
            entered, release = threading.Event(), threading.Event()
            def hold_ack():
                with server.proc._ack_lock:
                    entered.set()
                    release.wait(3)
            thread = threading.Thread(target=hold_ack)
            thread.start()
            self.assertTrue(entered.wait(1))
            before = {entry[:3]: signature(getattr(getattr(modules[entry[0]], entry[1]), entry[2]))
                      for entry in update.FUNCTIONS}
            started = time.monotonic()
            try:
                with self.assertRaisesRegex(RuntimeError, 'ACK is active'):
                    update.apply(runtime)
                self.assertLess(time.monotonic() - started, 1.5)
                self.assertNotIn('ack', vars(server.proc))
                self.assertNotIn('call', vars(server.proc))
                self.assertFalse(runtime.changed.is_set())
                for (name, owner, method), expected in before.items():
                    self.assertEqual(signature(getattr(getattr(modules[name], owner), method)), expected)
            finally:
                release.set()
                thread.join(2)
                runtime._late_servers = []
            self.assertFalse(thread.is_alive())

    def test_late_barrier_failure_restores_every_function_and_proxy_callback(self):
        with actual_runtime_fixture() as (case, modules, _):
            runtime = case.runtime
            runtime.connection_ids['default'] = 'new'
            frames = stale_ack.RetainedFrames(case.message)
            server = native_dispatcher(runtime, frames)
            thread = threading.Thread(target=server.dispatch)
            before = {entry[:3]: signature(getattr(getattr(modules[entry[0]], entry[1]), entry[2]))
                      for entry in update.FUNCTIONS}
            original_protect = update._RequestAckBarrier.protect
            def fail_after_swap(barrier):
                original_protect(barrier)
                if signature(runtime.request.__func__) == update.FUNCTIONS[0][4]:
                    raise RuntimeError('Fixture failure after the request code swap')
            with server.proc._ack_lock:
                thread.start()
                wait_for_frame(thread, server.proc.ack.__func__.__code__)
                with patch.object(update._RequestAckBarrier, 'protect', fail_after_swap):
                    with self.assertRaisesRegex(RuntimeError, 'Fixture failure'):
                        update.apply(runtime)
                self.assertNotIn('ack', vars(server.proc))
                self.assertNotIn('call', vars(server.proc))
                self.assertNotIn('_studio_request_ack_guard', vars(server.proc))
                self.assertNotIn('begin_tool_request', vars(runtime))
                self.assertNotIn('_studio_old_dynamic_claim_guard', vars(runtime))
                for (name, owner, method), expected in before.items():
                    self.assertEqual(signature(getattr(getattr(modules[name], owner), method)), expected)
                self.assertFalse(runtime.changed.is_set())
            finish_dispatch(thread, server)

    def test_old_reservation_frame_is_retained_and_replacement_executes_once(self):
        with actual_runtime_fixture() as (case, modules, _):
            runtime = case.runtime
            frames = stale_ack.RetainedFrames(case.message)
            server = native_dispatcher(runtime, frames)
            thread = threading.Thread(target=server.dispatch)
            callback = runtime.request
            with runtime.lock:
                thread.start()
                wait_for_frame(thread, runtime.reserve_tool_request.__func__.__code__)
                runtime.connection_ids['default'] = 'new'
                self.assertEqual(update.apply(runtime)['status'], 'applied')
                self.assertIs(runtime.request.__func__, callback.__func__)
            finish_dispatch(thread, server)
            self.assertEqual(frames.acknowledged, 6)
            self.assertEqual(case.operations, 0)
            self.assertIsNone(runtime.tool_request(case.key))
            self.assertEqual(server.proc.ack_pending, set())
            self.assertTrue(server.errors)
            # The retired proxy exists only in the actual old dispatcher frame.
            self.assertNotIn(server, runtime.servers.values())
            replacement = native_dispatcher(runtime, frames, connection_id='new')
            replacement.dispatch()
            self.assertEqual(frames.acknowledged, 7)
            self.assertEqual(case.operations, 1)
            self.assertEqual(runtime.tool_request(case.key)['outcome'], 'applied')
            self.assertEqual(update.apply(runtime)['status'], 'already_applied')

    def test_code_swap_without_the_barrier_loses_the_actual_old_frame(self):
        with actual_runtime_fixture() as (case, _, _):
            runtime = case.runtime
            frames = stale_ack.RetainedFrames(case.message)
            server = native_dispatcher(runtime, frames)
            thread = threading.Thread(target=server.dispatch)
            with runtime.lock:
                thread.start()
                wait_for_frame(thread, runtime.reserve_tool_request.__func__.__code__)
                runtime.connection_ids['default'] = 'new'
                with patch.object(update._RequestAckBarrier, 'protect', lambda _barrier: None):
                    self.assertEqual(update.apply(runtime)['status'], 'applied')
            finish_dispatch(thread, server)
            self.assertEqual(frames.acknowledged, 7)
            self.assertIsNone(runtime.tool_request(case.key))
            self.assertEqual(case.operations, 0)

    def test_old_ack_frame_after_request_return_cannot_cross_the_held_sequence(self):
        with actual_runtime_fixture() as (case, _, _):
            runtime = case.runtime
            runtime.connection_ids['default'] = 'new'
            following = {'method': 'fixture/notification', 'params': {}}
            frames = stale_ack.RetainedFrames(case.message, following)
            server = native_dispatcher(runtime, frames)
            old_ack_code = server.proc.ack.__func__.__code__
            thread = threading.Thread(target=server.dispatch)
            with server.proc._ack_lock:
                thread.start()
                wait_for_frame(thread, old_ack_code)
                self.assertEqual(update.apply(runtime)['status'], 'applied')
            finish_dispatch(thread, server)
            self.assertEqual(frames.acknowledged, 6)
            self.assertEqual(server.proc.ack_calls, [])
            self.assertEqual(server.proc.ack_pending, {8})
            self.assertIsNone(runtime.tool_request(case.key))
            replacement = native_dispatcher(runtime, frames, connection_id='new')
            replacement.dispatch()
            self.assertEqual(frames.acknowledged, 8)
            self.assertEqual(case.operations, 1)

    def test_exact_unknown_admission_is_acked_without_replaying_the_mutation(self):
        with actual_runtime_fixture() as (case, _, _):
            runtime = case.runtime
            record = runtime.reserve_tool_request(case.message, 'default', 'old')
            record.update(stage='interrupted', outcome='unknown', error='Unknown fixture receipt')
            with runtime.lock, runtime.db() as db:
                runtime.put(db, 'tool_requests', record)
            original = copy.deepcopy(runtime.tool_request(case.key))
            runtime.connection_ids['default'] = 'new'
            frames = stale_ack.RetainedFrames(case.message)
            server = native_dispatcher(runtime, frames)
            thread = threading.Thread(target=server.dispatch)
            with server.proc._ack_lock:
                thread.start()
                wait_for_frame(thread, server.proc.ack.__func__.__code__)
                self.assertEqual(update.apply(runtime)['status'], 'applied')
            finish_dispatch(thread, server)
            self.assertEqual(frames.acknowledged, 7)
            self.assertEqual(runtime.tool_request(case.key), original)
            self.assertEqual(case.operations, 0)
            self.assertEqual(server.errors, [])

    def test_another_receipt_identity_does_not_admit_the_held_frame(self):
        for field in ('signature', 'accountKey', 'rpcId', 'threadId'):
            with self.subTest(field=field), actual_runtime_fixture() as (case, _, _):
                runtime = case.runtime
                record = runtime.reserve_tool_request(case.message, 'default', 'old')
                record.update(stage='interrupted', outcome='unknown')
                record[field] = 'different-receipt-identity'
                with runtime.lock, runtime.db() as db:
                    runtime.put(db, 'tool_requests', record)
                original = copy.deepcopy(runtime.tool_request(case.key))
                runtime.connection_ids['default'] = 'new'
                frames = stale_ack.RetainedFrames(case.message)
                server = native_dispatcher(runtime, frames)
                thread = threading.Thread(target=server.dispatch)
                with server.proc._ack_lock:
                    thread.start()
                    wait_for_frame(thread, server.proc.ack.__func__.__code__)
                    self.assertEqual(update.apply(runtime)['status'], 'applied')
                finish_dispatch(thread, server)
                self.assertEqual(frames.acknowledged, 6)
                self.assertEqual(runtime.tool_request(case.key), original)
                self.assertEqual(case.operations, 0)

    def test_a_saved_approval_keeps_its_ack_and_saved_identity(self):
        with actual_runtime_fixture() as (case, _, _):
            runtime = case.runtime
            message = {'id': 'fixture-approval', 'method': 'item/tool/requestUserInput',
                       'params': {'threadId': case.actor['threadId'], 'questions': []}}
            runtime.request(message, 'default', 'old')
            with runtime.read_db() as db:
                original = db.execute('SELECT id,record FROM runtime_requests').fetchall()
            self.assertEqual(len(original), 1)
            runtime.connection_ids['default'] = 'new'
            frames = stale_ack.RetainedFrames(message)
            server = native_dispatcher(runtime, frames)
            thread = threading.Thread(target=server.dispatch)
            with server.proc._ack_lock:
                thread.start()
                wait_for_frame(thread, server.proc.ack.__func__.__code__)
                self.assertEqual(update.apply(runtime)['status'], 'applied')
            finish_dispatch(thread, server)
            self.assertEqual(frames.acknowledged, 7)
            with runtime.read_db() as db:
                self.assertEqual(db.execute('SELECT id,record FROM runtime_requests').fetchall(), original)
            self.assertEqual(case.operations, 0)

    def test_old_compute_frame_can_use_the_new_projection(self):
        with reviewed_fixture() as (runtime, modules, scripts):
            cost = modules['codex_session_costs'].SessionCostReader
            old = cost._compute
            old_frame = FunctionType(old.__code__, old.__globals__, old.__name__, old.__defaults__)
            spec = importlib.util.spec_from_file_location('cost_prices', ROOT / 'tests/pricing-session-cost-contract.py')
            prices = importlib.util.module_from_spec(spec)
            spec.loader.exec_module(prices)
            path = scripts / 'fixture.sqlite3'
            with sqlite3.connect(path) as db:
                db.executescript('''CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT);
                    CREATE TABLE analytics_agents(id TEXT PRIMARY KEY,record TEXT);
                    CREATE TABLE analytics_usage(seq INTEGER PRIMARY KEY,agent TEXT,root TEXT,
                        thread TEXT,turn TEXT,at REAL,record TEXT);''')
                db.execute('INSERT INTO analytics_agents VALUES (?,?)', ('lead', json.dumps({'rootId': 'lead'})))
                record = {'model': 'gpt-6-luna', 'responseId': 'response',
                          'delta': {'inputTokens': 1000, 'outputTokens': 0,
                                    'cachedInputTokens': 0, 'cacheWriteInputTokens': 0}}
                db.execute('INSERT INTO analytics_usage VALUES (1,?,?,?,?,?,?)',
                           ('lead', 'lead', 'thread', 'turn', 1, json.dumps(record)))
            reader = cost(path, prices.FixedPricing())
            self.assertEqual(update.apply(runtime)['status'], 'applied')
            ongoing = old_frame(reader, 'lead', 'lead')
            fresh = reader._compute('lead', 'lead')
            self.assertEqual(ongoing, fresh)
            self.assertEqual(fresh['pricedSamples'], 1)
            self.assertAlmostEqual(fresh['totalUSD'], .0001)

    def test_existing_callback_retains_the_unadmitted_frame_after_update(self):
        with reviewed_fixture() as (runtime, modules, _):
            callback = runtime.request
            before_function = callback.__func__
            message = {'id': 'native-request', 'method': 'currentTime/read'}
            self.assertIsNone(callback(message, connection_id='old'))
            self.assertEqual(update.apply(runtime)['status'], 'applied')
            self.assertIs(runtime.request.__func__, before_function)
            with self.assertRaisesRegex(ConnectionError, 'not admitted'):
                callback(message, connection_id='old')
            self.assertTrue(runtime.changed.is_set())
            for name, owner, method, _, after in update.FUNCTIONS:
                self.assertEqual(signature(getattr(getattr(modules[name], owner), method)), after)
            self.assertEqual(update.apply(runtime)['status'], 'already_applied')

    def test_changed_guard_rejects_the_whole_update(self):
        with reviewed_fixture() as (runtime, modules, _):
            before = {entry[:3]: signature(getattr(getattr(modules[entry[0]], entry[1]), entry[2]))
                      for entry in update.FUNCTIONS}
            modules['codex_runtime'].Runtime.connection_current = lambda *_args: True
            with self.assertRaisesRegex(RuntimeError, 'guard differs: connection_current'):
                update.apply(runtime)
            for (name, owner, method), expected in before.items():
                self.assertEqual(signature(getattr(getattr(modules[name], owner), method)), expected)
            self.assertFalse(runtime.changed.is_set())

    def test_changed_source_rejects_the_whole_update(self):
        with reviewed_fixture() as (runtime, modules, scripts):
            path = scripts / 'codex_session_costs.py'
            path.write_bytes(path.read_bytes() + b'\n# changed\n')
            with self.assertRaisesRegex(RuntimeError, 'reviewed request or cost source differs'):
                update.apply(runtime)
            self.assertEqual(signature(modules['codex_runtime'].Runtime.request), update.FUNCTIONS[0][3])
            self.assertFalse(runtime.changed.is_set())


if __name__ == '__main__':
    unittest.main()
