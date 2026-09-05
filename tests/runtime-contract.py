#!/usr/bin/env python3
"""Behavioral orchestration tests. No model service, no user state."""
import base64
from concurrent.futures import ThreadPoolExecutor
import json
import os
from pathlib import Path
import sys
import tempfile
import threading
import time
import unittest

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
from codex_runtime import Runtime


def eventually(predicate, timeout=8):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        if predicate():
            return
        time.sleep(.01)
    raise AssertionError('Condition did not become true')


class FakeServer:
    def __init__(self, root, notify, request, died):
        self.notify, self.request, self.died = notify, request, died
        self.calls, self.responses = [], []
        self.seq = 0
        self.gate = threading.Event()
        self.closed = False
        self.fail_start = False
        self.finish_before_reply = False
        self.start_gate = None

    def call(self, method, params, timeout=60):
        self.calls.append((method, params))
        if method in ('thread/start', 'thread/resume'):
            self.seq += 1
            assert params['config']['features.multi_agent'] is False
            if method == 'thread/start':
                assert all(t['type'] == 'function' for t in params['dynamicTools'])
            return {'thread': {'id': params.get('threadId', f'thread-{self.seq}')}, 'model': 'test-model',
                    'sandbox': {'type': 'readOnly'}, 'approvalPolicy': 'on-request'}
        if method == 'turn/start':
            if self.start_gate is not None:
                self.start_gate.wait(5)
            if self.fail_start:
                raise RuntimeError('response timed out; outcome unknown')
            self.seq += 1
            turn = {'id': f'turn-{self.seq}', 'status': 'inProgress'}
            self.notify({'method': 'turn/started', 'params': {'threadId': params['threadId'], 'turn': turn}})
            if self.finish_before_reply:
                self.complete(params['threadId'], turn['id'])
            return {'turn': turn}
        if method == 'command/exec':
            assert params['sandboxPolicy'] == {'type': 'readOnly'}
            self.notify({'method': 'command/exec/outputDelta', 'params': {'processId': params['processId'],
                'deltaBase64': base64.b64encode(b'early output\n').decode(), 'stream': 'stdout'}})
            self.gate.wait(5)
            return {'exitCode': 7, 'stdout': '', 'stderr': ''}
        if method == 'command/exec/terminate':
            self.gate.set()
            return {}
        if method == 'turn/interrupt':
            self.notify({'method': 'turn/completed', 'params': {'threadId': params['threadId'],
                'turn': {'id': params['turnId'], 'status': 'interrupted'}}})
            return {}
        if method == 'model/list':
            return {'data': [{'model': 'test-model'}]}
        raise AssertionError(method)

    def write(self, value):
        self.responses.append(value)

    def submit(self, method, params):
        import concurrent.futures
        future = concurrent.futures.Future()
        def work():
            try: future.set_result(self.call(method, params))
            except Exception as error: future.set_exception(error)
        threading.Thread(target=work, daemon=True).start()
        return future

    def wait(self, future, timeout=60):
        return future.result(timeout)

    def complete(self, tid, turn, text='Result with evidence'):
        self.notify({'method': 'item/completed', 'params': {'threadId': tid,
            'item': {'id': turn + '-answer', 'type': 'agentMessage', 'text': text}}})
        self.notify({'method': 'turn/completed', 'params': {'threadId': tid,
            'turn': {'id': turn, 'status': 'completed'}}})

    def close(self):
        self.closed = True
        self.gate.set()


class RuntimeContract(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        self.runtime = Runtime(self.root, FakeServer)

    def tearDown(self):
        self.runtime.close()
        self.tmp.cleanup()

    def lead(self, **extra):
        a = self.runtime.create({'name': 'Lead', 'cwd': str(self.root), 'prompt': 'Coordinate the work', **extra})
        eventually(lambda: self.runtime.agent(a['id'])['status'] == 'running')
        return self.runtime.agent(a['id'])

    def complete(self, a):
        self.runtime.server.complete(a['threadId'], a['turnId'])

    def test_forty_children_respect_limit_and_wake_finished_parent(self):
        lead = self.lead(concurrency=5)
        self.runtime.server.request({'id': 100, 'method': 'item/tool/call', 'params': {
            'threadId': lead['threadId'], 'callId': 'batch', 'tool': 'orchestration_spawn',
            'arguments': {'agents': [{'name': f'Review {i}', 'prompt': f'Review file {i}', 'role': 'reviewer'} for i in range(40)]}}})
        eventually(lambda: bool(self.runtime.server.responses))
        self.assertTrue(self.runtime.server.responses[0]['result']['success'])
        eventually(lambda: len(self.runtime.snapshot()['agents']) == 41)
        self.complete(lead)
        peak = 0
        for _ in range(1000):
            agents = self.runtime.snapshot()['agents']
            running = [a for a in agents if a['status'] in ('running', 'starting', 'approval')]
            peak = max(peak, len(running))
            self.assertLessEqual(len(running), 5)
            for a in running:
                if a['status'] == 'running':
                    self.complete(a)
            if all(a['status'] == 'completed' for a in self.runtime.snapshot()['agents']):
                break
            time.sleep(.01)
        else:
            self.fail('Team did not finish')
        self.assertGreaterEqual(peak, 2)
        starts = [p for m,p in self.runtime.server.calls if m == 'turn/start' and p['threadId'] == lead['threadId']]
        self.assertGreater(len(starts), 1)
        text = '\n'.join(p['input'][0]['text'] for p in starts[1:])
        self.assertIn('child_result', text)
        self.assertIn('Review 39', text)
        self.assertIn('Result with evidence', text)

    def test_monitor_output_does_not_call_model_and_exit_wakes_parent(self):
        lead = self.lead()
        self.complete(lead)
        before = sum(m == 'turn/start' for m,p in self.runtime.server.calls)
        m = self.runtime.monitor(lead['id'], {'command': 'example-command'}, approved=True)
        eventually(lambda: bool(self.runtime.snapshot()['monitors'][0]['tail']))
        time.sleep(.12)
        self.assertEqual(sum(method == 'turn/start' for method,p in self.runtime.server.calls), before)
        self.runtime.server.gate.set()
        eventually(lambda: self.runtime.agent(lead['id'])['status'] == 'running')
        watch = self.runtime.snapshot()['monitors'][0]
        self.assertEqual(watch['exitCode'], 7)
        self.assertEqual(watch['status'], 'failed')
        self.assertEqual(Path(watch['log']).read_text(), 'early output\n')
        turns = [p for method,p in self.runtime.server.calls if method == 'turn/start']
        self.assertIn('monitor_exit', turns[-1]['input'][0]['text'])

    def test_stop_cancels_descendants_and_suppresses_late_wake(self):
        lead = self.lead()
        child = self.runtime.create({'name': 'Child', 'prompt': 'Review', 'role': 'reviewer'}, lead['id'])
        eventually(lambda: self.runtime.agent(child['id'])['status'] == 'running')
        child = self.runtime.agent(child['id'])
        self.runtime.stop(lead['id'])
        before = len([m for m,p in self.runtime.server.calls if m == 'turn/start'])
        self.complete(child)
        time.sleep(.15)
        self.assertEqual(before, len([m for m,p in self.runtime.server.calls if m == 'turn/start']))
        self.assertTrue(all(not a['autoWake'] for a in self.runtime.snapshot()['agents']))
        self.runtime.send(lead['id'], 'Resume only the lead')
        eventually(lambda: self.runtime.agent(lead['id'])['status'] == 'running')
        self.assertFalse(self.runtime.agent(child['id'])['autoWake'])

    def test_duplicate_completion_delivers_once(self):
        lead = self.lead()
        child = self.runtime.create({'name': 'Child', 'prompt': 'Review', 'role': 'reviewer'}, lead['id'])
        eventually(lambda: self.runtime.agent(child['id'])['status'] == 'running')
        child = self.runtime.agent(child['id'])
        self.complete(child)
        self.complete(child)
        events = [e for e in self.runtime.snapshot()['events'] if e['kind'] == 'child_result']
        self.assertEqual(len(events), 1)

    def test_restart_keeps_pending_events_without_replaying_unknown_work(self):
        lead = self.lead()
        self.runtime.send(lead['id'], 'Pending result', 'stable-event')
        self.runtime.close()
        self.runtime = Runtime(self.root, FakeServer)
        self.assertEqual(self.runtime.agent(lead['id'])['status'], 'interrupted')
        self.assertIsNone(self.runtime.server)
        self.assertEqual(len([e for e in self.runtime.snapshot()['events'] if e['id'] == 'stable-event']), 1)
        self.runtime.send(lead['id'], 'Review interrupted work and continue')
        eventually(lambda: self.runtime.agent(lead['id'])['status'] == 'running')
        self.assertTrue(any(m == 'thread/resume' for m,p in self.runtime.server.calls))

    def test_only_one_runtime_owns_state(self):
        with self.assertRaisesRegex(RuntimeError, 'owns'):
            Runtime(self.root, FakeServer)

    def test_command_requires_approval_and_stop_invalidates_it(self):
        lead = self.lead()
        m = self.runtime.monitor(lead['id'], {'command': 'example-command'})
        self.assertEqual(m['status'], 'approval')
        self.assertFalse(any(method == 'command/exec' for method,p in self.runtime.server.calls))
        r = self.runtime.snapshot()['requests'][0]
        self.runtime.stop(lead['id'])
        with self.assertRaisesRegex(ValueError, 'stopped'):
            self.runtime.answer(r['id'], {'decision': 'accept'})

    def test_completion_before_rpc_response_stays_terminal(self):
        self.runtime.connect().finish_before_reply = True
        a = self.runtime.create({'name': 'Lead', 'cwd': str(self.root), 'prompt': 'Finish'})
        eventually(lambda: self.runtime.agent(a['id'])['status'] == 'completed')
        time.sleep(.1)
        self.assertEqual(self.runtime.agent(a['id'])['status'], 'completed')

    def test_budget_stops_team_and_new_budget_allows_explicit_resume(self):
        lead = self.lead(tokenBudget=100)
        self.runtime.notification({'method': 'thread/tokenUsage/updated', 'params': {
            'threadId': lead['threadId'], 'tokenUsage': {'total': {'totalTokens': 101}}}})
        eventually(lambda: not self.runtime.agent(lead['id'])['autoWake'])
        with self.assertRaisesRegex(ValueError, 'budget'):
            self.runtime.send(lead['id'], 'Continue')
        self.runtime.configure(lead['id'], {'tokenBudget': 1000, 'concurrency': 2})
        self.runtime.send(lead['id'], 'Continue')
        eventually(lambda: self.runtime.agent(lead['id'])['status'] == 'running')

    def test_invalid_batch_creates_no_workers(self):
        lead = self.lead(maxAgents=2)
        self.runtime.server.request({'id': 100, 'method': 'item/tool/call', 'params': {
            'threadId': lead['threadId'], 'callId': 'batch', 'tool': 'orchestration_spawn',
            'arguments': {'agents': [{'name': 'A', 'prompt': 'Review', 'role': 'reviewer'},
                                    {'name': 'B', 'prompt': 'Review', 'role': 'reviewer'}]}}})
        eventually(lambda: bool(self.runtime.server.responses))
        self.assertFalse(self.runtime.server.responses[0]['result']['success'])
        self.assertEqual(len(self.runtime.snapshot()['agents']), 1)

    def test_implementer_uses_isolated_committed_worktree(self):
        import subprocess
        repo = self.root / 'project'; repo.mkdir()
        def git(*args):
            return subprocess.run(['git', '-C', str(repo), *args], check=True, capture_output=True, text=True)
        git('init'); git('config', 'user.name', 'Test'); git('config', 'user.email', 'test@example.invalid')
        (repo / 'file.txt').write_text('committed')
        git('add', 'file.txt'); git('commit', '-m', 'fixture')
        (repo / 'file.txt').write_text('parent uncommitted change')
        lead = self.lead(cwd=str(repo))
        child = self.runtime.create({'name': 'Implementer', 'prompt': 'Change file.txt'}, lead['id'])
        eventually(lambda: self.runtime.agent(child['id'])['status'] in ('running', 'failed'))
        child = self.runtime.agent(child['id'])
        self.assertEqual(child['status'], 'running', child['error'])
        self.assertNotEqual(child['cwd'], str(repo))
        self.assertEqual((Path(child['cwd']) / 'file.txt').read_text(), 'committed')
        self.assertEqual((repo / 'file.txt').read_text(), 'parent uncommitted change')

    def test_monitor_cancel_preserves_exit_history_and_prevents_wake(self):
        lead = self.lead(); self.complete(lead)
        m = self.runtime.monitor(lead['id'], {'command': 'example-command'}, approved=True)
        eventually(lambda: self.runtime.snapshot()['monitors'][0]['status'] == 'running')
        self.runtime.cancel_monitor(m['id'])
        time.sleep(.1)
        self.assertEqual(self.runtime.snapshot()['monitors'][0]['status'], 'cancelled')
        self.assertFalse(any(e['kind'] == 'monitor_exit' for e in self.runtime.snapshot()['events']))

    def test_managed_chat_delivery_and_offline_membership(self):
        from codex_canvas import Canvas
        import uuid
        c = Canvas(self.root); c.runtime = self.runtime
        lead = self.lead()
        room = str(uuid.uuid4())
        c.create_chat('Team', [lead['id']], room)
        with ThreadPoolExecutor(max_workers=4) as pool:
            futures = [pool.submit(c.post, room, f'Instruction {i}', str(uuid.uuid4())) for i in range(8)]
            results = [f.result(5) for f in futures]
        self.assertTrue(all(x['deliveries'][lead['id']] == 'queued' for x in results))
        reader = Canvas(self.root, read_only=True)
        self.assertTrue(any(a['id'] == lead['id'] for a in reader.threads()))
        self.assertEqual(len(reader.messages(room)), 8)

    def test_approval_and_question_responses_reach_codex(self):
        lead = self.lead()
        self.runtime.request({'id': 101, 'method': 'item/commandExecution/requestApproval',
                              'params': {'threadId': lead['threadId'], 'command': 'git status'}})
        request = self.runtime.snapshot()['requests'][0]
        self.runtime.answer(request['id'], {'decision': 'decline'})
        self.assertEqual(self.runtime.server.responses[-1], {'id': 101, 'result': {'decision': 'decline'}})
        self.runtime.request({'id': 102, 'method': 'item/tool/requestUserInput',
                              'params': {'threadId': lead['threadId'], 'questions': [{'id': 'q', 'question': 'Which file?'}]}})
        request = self.runtime.snapshot()['requests'][0]
        answers = {'q': {'answers': ['file.txt']}}
        self.runtime.answer(request['id'], {'answers': answers})
        self.assertEqual(self.runtime.server.responses[-1]['result'], {'answers': answers})

    def test_stop_then_resume_does_not_overlap_pending_start(self):
        lead = self.lead(); self.complete(lead)
        self.runtime.server.start_gate = threading.Event()
        self.runtime.send(lead['id'], 'Old pending instruction')
        eventually(lambda: sum(m == 'turn/start' for m,p in self.runtime.server.calls) == 2)
        self.runtime.stop(lead['id'])
        self.runtime.send(lead['id'], 'New instruction after Stop')
        time.sleep(.1)
        self.assertEqual(sum(m == 'turn/start' for m,p in self.runtime.server.calls), 2)
        self.runtime.server.start_gate.set()
        eventually(lambda: sum(m == 'turn/start' for m,p in self.runtime.server.calls) == 3)
        eventually(lambda: self.runtime.agent(lead['id'])['status'] == 'running')
        calls = [p for m,p in self.runtime.server.calls if m == 'turn/start']
        self.assertEqual(calls[-1]['input'][0]['text'], 'New instruction after Stop')
        self.assertTrue(any(m == 'turn/interrupt' for m,p in self.runtime.server.calls))

    def test_old_turn_cannot_spawn_or_monitor_after_resume(self):
        lead = self.lead(); self.runtime.stop(lead['id'])
        self.runtime.send(lead['id'], 'Resume with a new task')
        with self.assertRaisesRegex(ValueError, 'turn was stopped'):
            self.runtime.create({'name': 'Late child', 'prompt': 'Old work', 'role': 'reviewer'},
                                lead['id'], parent_epoch=lead['epoch'])
        with self.assertRaisesRegex(ValueError, 'turn was stopped'):
            self.runtime.monitor(lead['id'], {'command': 'old command'}, epoch=lead['epoch'])

    def test_child_waiting_for_monitor_reports_only_after_final_result(self):
        lead = self.lead()
        child = self.runtime.create({'name': 'Child', 'prompt': 'Review', 'role': 'reviewer'}, lead['id'])
        eventually(lambda: self.runtime.agent(child['id'])['status'] == 'running')
        child = self.runtime.agent(child['id'])
        self.runtime.monitor(child['id'], {'command': 'example-command'}, approved=True)
        eventually(lambda: self.runtime.snapshot()['monitors'][0]['status'] == 'running')
        self.complete(child)
        self.assertEqual(self.runtime.agent(child['id'])['status'], 'waiting')
        self.assertFalse(any(e['kind'] == 'child_result' for e in self.runtime.snapshot()['events']))
        self.runtime.server.gate.set()
        eventually(lambda: self.runtime.agent(child['id'])['status'] == 'running')
        self.complete(self.runtime.agent(child['id']))
        self.assertEqual(len([e for e in self.runtime.snapshot()['events'] if e['kind'] == 'child_result']), 1)

    def test_timeout_does_not_retry_model_call(self):
        self.runtime.connect().fail_start = True
        a = self.runtime.create({'name': 'Lead', 'cwd': str(self.root), 'prompt': 'Finish'})
        eventually(lambda: self.runtime.agent(a['id'])['status'] == 'failed')
        time.sleep(.15)
        self.assertEqual(sum(m == 'turn/start' for m,p in self.runtime.server.calls), 1)
        self.assertEqual(self.runtime.snapshot()['events'][0]['status'], 'uncertain')

if __name__ == '__main__':
    unittest.main(verbosity=2)
