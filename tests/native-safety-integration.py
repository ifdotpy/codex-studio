#!/usr/bin/env python3
"""Use real Codex JSON-RPC and a loopback provider. No OpenAI requests."""
import importlib.util
from pathlib import Path
import sys
import unittest
from unittest.mock import patch
sys.dont_write_bytecode = True

def load(name, file):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).with_name(file))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module

native = load('native_primitives', 'native-primitives-integration.py')
contracts = load('safety_contracts', 'native-safety-contract.py')

class SafetyProvider(native.n.ResponsesHandler):
    def event(self, event_type, **fields):
        super().event(event_type, **fields)
        if event_type == 'response.created' and len(self.server.requests) == 1:
            super().event('response.metadata', metadata={
                'type': 'safety_buffering', 'model': 'gpt-5.6-sol',
                'use_cases': ['cyber'], 'reasons': ['review'], 'retry_model': 'gpt-5.6-sol'})

class SafetyNative(unittest.TestCase):
    def test_native_fork_before_turn_preserves_context_and_submits_once(self):
        with native.native_server(SafetyProvider) as (server, tid, provider, notifications, _):
            f = contracts.Safety()
            f.setUp()
            try:
                a = f.runtime.agent(f.key)
                with f.runtime.lock, f.runtime.db() as db:
                    a.update(threadId=tid, turnId=None, inFlight=True, status="starting", autoWake=True, turnEpoch=a["epoch"])
                    a.pop('startAttempt', None)
                    f.runtime.put(db, 'agents', a)
                    f.runtime.servers['default'] = server
                # Keep the native provider isolated. The runtime normally provides
                # Studio-specific instructions on fork; the test needs none.
                base_notify = server.notification
                server.notification = lambda message: (base_notify(message), f.runtime.notification(message, 'default', f.connection))
                result = server.call('turn/start', {'threadId': tid, 'input': [{'type': 'text', 'text': 'Original input'}]})
                turn = result['turn']['id']
                native.n.until(provider.started.is_set, 'local provider started')
                f.turn = turn
                f.a = f.runtime.agent(f.key)
                with f.runtime.lock, f.runtime.db() as db:
                    a = f.runtime.agent(f.key, db)
                    a.update(turnId=turn, inFlight=True, status='running', autoWake=True)
                    f.runtime.put(db, 'agents', a)
                native.n.until(lambda: f.runtime.agent(f.key).get('nativeSafetyBuffering'), 'native safety buffering notification')
                with patch.object(f.runtime, 'new_thread_params', return_value={'approvalPolicy': 'never', 'sandbox': 'read-only'}):
                    f.action()
                    native.n.until(lambda: f.receipt().get('stage') in {'running', 'failed', 'unknown'}, 'native retry receipt')
                    self.assertEqual(f.receipt()['stage'], 'running', f.receipt())
                native.n.until(lambda: len(provider.requests) == 2, 'one retry request')
                native.n.until(lambda: not f.runtime.agent(f.key).get('inFlight'), 'retry completed')
                a = f.runtime.agent(f.key)
                self.assertNotEqual(a['threadId'], tid)
                self.assertEqual(provider.requests[-1]['model'], 'gpt-5.6-sol')
                f.action()
                self.assertEqual(len(provider.requests), 2)
                self.assertEqual(provider.unexpected, [])
            finally:
                f.runtime.servers['default'] = f.server
                provider.release.set()
                f.tearDown()

if __name__ == '__main__': unittest.main()
