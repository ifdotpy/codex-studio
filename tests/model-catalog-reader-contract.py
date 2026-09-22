#!/usr/bin/env python3
"""The native metadata reader preserves account isolation and process boundaries."""
import concurrent.futures
import json
import os
from pathlib import Path
import sys
import tempfile
import time
import unittest
from types import SimpleNamespace
from unittest.mock import Mock, patch

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
import codex_model_catalog_reader as reader
from codex_catalog import ModelCatalogCache, CatalogPending, runtime_catalog


PROGRAM = r'''
import json, os, sys, time
from pathlib import Path
home = Path(os.environ['CODEX_HOME'])
(home / 'pid').write_text(str(os.getpid()))
(home / 'environment').write_text(json.dumps({
 'home': str(home), 'apiKey': 'OPENAI_API_KEY' in os.environ,
 'codexKey': 'CODEX_API_KEY' in os.environ, 'argv': sys.argv[1:], 'executable': sys.argv[0]}))
mode = (home / 'mode').read_text()
for line in sys.stdin:
 message = json.loads(line)
 with (home / 'requests').open('a') as log:
  log.write(json.dumps(message) + '\n')
 if 'id' not in message:
  continue
 if message['method'] == 'initialize':
  result = {}
 else:
  if mode == 'hang':
   time.sleep(20)
  if mode == 'delay':
   time.sleep(.12)
  if mode == 'stderr':
   os.write(2, b'x' * 500000)
  if mode == 'large':
   os.write(1, b'x' * 3000000)
   continue
  if mode == 'request':
   print(json.dumps({'id': 99, 'method': 'command/exec', 'params': {}}), flush=True)
   continue
  if mode == 'identity':
   message['id'] += 1
  if mode == 'error':
   print(json.dumps({'id': message['id'], 'error': {'message': 'secret'}}), flush=True)
   continue
  cursor = message['params'].get('cursor')
  result = {'data': [{'model': 'two' if cursor else 'one',
                     'availableAccessPrograms': {'cyber': ['standard', 'daybreakBlue']}}],
            'nextCursor': ('two' if not cursor or mode == 'cycle' else None)}
 print(json.dumps({'id': message['id'], 'result': result}), flush=True)
'''


class ReaderContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.home = Path(self.temp.name)
        self.binary = self.home / 'native'
        self.binary.write_text('#!' + sys.executable + '\n' + PROGRAM)
        self.binary.chmod(0o700)
        (self.home / 'mode').write_text('normal')
        env = patch.dict(os.environ, {'OPENAI_API_KEY': 'inherited', 'CODEX_API_KEY': 'inherited'})
        env.start()
        self.addCleanup(env.stop)
        selector = patch('codex_native_runtime.executable_for', return_value={'path': str(self.binary), 'sha256': 'approved'})
        self.select_executable = selector.start()
        self.addCleanup(selector.stop)

    def read(self, **kwargs):
        return reader.read_model_catalog(self.home, isolated=True, executable=str(self.binary), **kwargs)

    def assert_reaped(self):
        pid = int((self.home / 'pid').read_text())
        with self.assertRaises(ChildProcessError):
            os.waitpid(pid, os.WNOHANG)
        with self.assertRaises(ProcessLookupError):
            os.kill(pid, 0)

    def test_native_pages_capabilities_and_isolated_environment(self):
        result = self.read()
        self.assertEqual([row['model'] for row in result['data']], ['one', 'two'])
        self.assertEqual(result['data'][0]['availableAccessPrograms']['cyber'], ['standard', 'daybreakBlue'])
        environment = json.loads((self.home / 'environment').read_text())
        self.assertEqual(environment['home'], str(self.home))
        self.assertEqual(environment['executable'], str(self.binary))
        self.assertFalse(environment['apiKey'])
        self.assertFalse(environment['codexKey'])
        self.assertIn('cli_auth_credentials_store="file"', environment['argv'])
        requests = [json.loads(line) for line in (self.home / 'requests').read_text().splitlines()]
        self.assertEqual([row['method'] for row in requests], ['initialize', 'initialized', 'model/list', 'model/list'])
        self.assertTrue(requests[0]['params']['capabilities']['experimentalApi'])
        self.assertTrue(requests[2]['params']['includeHidden'])
        self.assertEqual(requests[3]['params']['cursor'], 'two')
        self.assert_reaped()

    def test_default_account_preserves_native_auth_environment(self):
        reader.read_model_catalog(self.home, isolated=False, executable=str(self.binary))
        environment = json.loads((self.home / 'environment').read_text())
        self.assertTrue(environment['apiKey'])
        self.assertTrue(environment['codexKey'])
        self.assertNotIn('cli_auth_credentials_store="file"', environment['argv'])
        self.assert_reaped()

    def test_stderr_is_drained_without_blocking(self):
        (self.home / 'mode').write_text('stderr')
        self.assertEqual(len(self.read(timeout=3)['data']), 2)
        self.assert_reaped()

    def test_timeout_terminates_and_reaps_only_owned_process(self):
        (self.home / 'mode').write_text('hang')
        started = time.monotonic()
        with self.assertRaises(TimeoutError):
            self.read(timeout=.2)
        self.assertLess(time.monotonic() - started, 2)
        self.assert_reaped()

    def test_protocol_failures_never_return_partial_catalog(self):
        for mode in ('cycle', 'identity', 'error', 'large', 'request'):
            with self.subTest(mode=mode):
                (self.home / 'mode').write_text(mode)
                with self.assertRaises((RuntimeError, ValueError)) as caught:
                    self.read(timeout=3)
                self.assertNotIn('secret', str(caught.exception))
                self.assert_reaped()

    def test_changed_connection_stops_reader(self):
        calls = 0

        def current():
            nonlocal calls
            calls += 1
            return calls < 4

        with self.assertRaisesRegex(RuntimeError, 'connection changed'):
            self.read(current=current)
        self.assert_reaped()

    def test_cache_reuses_exact_late_native_read(self):
        (self.home / 'mode').write_text('delay')
        cache = ModelCatalogCache(wait_seconds=.01)
        server = object()
        submitted = []

        def submit(method, params):
            future = reader.submit_model_catalog(self.home, isolated=True, current=lambda: True, executable=str(self.binary))
            submitted.append(future)
            return future

        for _ in range(2):
            with self.assertRaises(CatalogPending):
                cache.read('a', server, 'one', lambda: True, submit=submit)
        self.assertEqual(len(submitted), 1)
        submitted[0].result(timeout=3)
        self.assertEqual(len(cache.read('a', server, 'one', lambda: True, submit=submit)['data']), 2)
        self.assertEqual(len(submitted), 1)
        self.assert_reaped()

    def test_reader_requires_the_callers_selected_executable(self):
        with self.assertRaises(TypeError):
            reader.read_model_catalog(self.home, isolated=True)
        with self.assertRaises(TypeError):
            reader.submit_model_catalog(self.home, isolated=True, current=lambda: True)
        self.assertFalse((self.home / 'pid').exists())

    def runtime(self, provider='codex'):
        from codex_runtime import AppServer
        server = Mock()
        accounts = Mock()
        accounts.get.return_value = {'provider': provider}
        accounts.home.return_value = self.home
        return SimpleNamespace(factory=AppServer, accounts=accounts,
                               connect=Mock(return_value=server), closed=False,
                               servers={'secondary': server}, connection_ids={'secondary': 'one'},
                               connection_current=lambda account, connection: connection == 'one')

    def test_runtime_uses_registered_account_home(self):
        runtime = self.runtime()
        result = runtime_catalog(runtime, 'secondary')
        self.assertEqual(len(result['data']), 2)
        runtime.accounts.home.assert_called_once_with('secondary')
        self.select_executable.assert_called_once_with(runtime)
        runtime.servers['secondary'].submit.assert_not_called()
        environment = json.loads((self.home / 'environment').read_text())
        self.assertFalse(environment['apiKey'])
        self.assertEqual(environment['executable'], str(self.binary))
        self.assert_reaped()

    def test_runtime_preserves_claude_catalog(self):
        runtime = self.runtime('claude')
        future = concurrent.futures.Future()
        future.set_result({'data': [{'model': 'claude'}]})
        runtime.servers['secondary'].submit.return_value = future
        self.assertEqual(runtime_catalog(runtime, 'secondary'), {'data': [{'model': 'claude'}]})
        runtime.accounts.home.assert_not_called()
        self.select_executable.assert_not_called()
        self.assertFalse((self.home / 'pid').exists())

    def test_runtime_refuses_metadata_when_no_executable_is_approved(self):
        runtime = self.runtime()
        self.select_executable.side_effect = RuntimeError('No executable passed the protocol checks')
        with self.assertRaisesRegex(RuntimeError, 'protocol checks'):
            runtime_catalog(runtime, 'secondary')
        runtime.servers['secondary'].submit.assert_not_called()
        self.assertFalse((self.home / 'pid').exists())

    def test_runtime_rejects_changed_account_before_metadata_process(self):
        runtime = self.runtime()
        runtime.accounts.home.side_effect = ValueError('account changed')
        with self.assertRaisesRegex(ValueError, 'account changed'):
            runtime_catalog(runtime, 'secondary')
        self.assertFalse((self.home / 'pid').exists())


if __name__ == '__main__':
    unittest.main()
