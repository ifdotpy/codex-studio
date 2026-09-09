#!/usr/bin/env python3
"""Exercise installed app-server primitives with a loopback provider and isolated state."""
from contextlib import contextmanager
import importlib.util
import json
import os
from pathlib import Path
import tempfile
import threading
import unittest
from unittest.mock import patch


def fixture(name, filename):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).with_name(filename))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


n = fixture('native', 'workspace-native-turn.py')
s = fixture('shell', 'monitor-shell-native.py')
w = fixture('workspace', 'workspace-contract.py')


@contextmanager
def native_server(handler=n.ResponsesHandler):
    with tempfile.TemporaryDirectory(prefix='studio-native-primitives-') as directory:
        root = Path(directory)
        home = root / 'home'
        home.mkdir()
        provider = n.Provider()
        provider.RequestHandlerClass = handler
        provider.command = 'printf native-background-ready; exec sleep 120'
        threading.Thread(target=provider.serve_forever, daemon=True).start()
        endpoint = f'http://127.0.0.1:{provider.server_port}'
        (home / 'config.toml').write_text(f'''model = "gpt-5.6-sol"
model_provider = "local-probe"
[features]
plugins = false
remote_plugin = false
apps = false
skip_host_skill_discovery = true
[model_providers.local-probe]
name = "Local primitive fixture"
base_url = "{endpoint}/v1"
wire_api = "responses"
requires_openai_auth = false
request_max_retries = 0
stream_max_retries = 0
''')
        env = {key: endpoint for key in ('HTTP_PROXY', 'HTTPS_PROXY', 'ALL_PROXY',
                                         'http_proxy', 'https_proxy', 'all_proxy')}
        env.update(CODEX_HOME=str(home), OPENAI_API_KEY='', CODEX_API_KEY='',
                   NO_PROXY='127.0.0.1,localhost', no_proxy='127.0.0.1,localhost')
        notifications, servers = [], []
        def start():
            server = n.AppServer(root, notifications.append, lambda _: None, lambda: None,
                                 home=home, isolated=True)
            servers.append(server)
            return server
        try:
            with patch.dict(os.environ, env):
                server = start()
                thread = server.call('thread/start', {'cwd': str(root), 'model': 'gpt-5.6-sol',
                    'config': {**n.THREAD_CONFIG, 'model_provider': 'local-probe'},
                    'approvalPolicy': 'never', 'sandbox': 'danger-full-access'})['thread']['id']
                yield server, thread, provider, notifications, start
        finally:
            for server in servers:
                server.close()
                for stream in (server.proc.stdin, server.proc.stdout, server.proc.stderr):
                    if stream is not None:
                        stream.close()
            provider.release.set()
            provider.shutdown()
            provider.server_close()


class NativePrimitives(unittest.TestCase):
    def test_studio_stops_real_native_process_after_turn_without_model_request(self):
        with native_server(s.ShellHandler) as (server, tid, provider, notifications, _):
            turn = server.call('turn/start', {'threadId': tid,
                'input': [{'type': 'text', 'text': 'Run the fixed local process.'}]})['turn']['id']
            n.until(lambda: any(e.get('method') == 'turn/completed' and
                    e['params']['turn']['id'] == turn for e in notifications), 'native turn completion')
            terminals = server.call('thread/backgroundTerminals/list', {'threadId': tid})['data']
            self.assertEqual(len(terminals), 1)
            terminal = terminals[0]
            model_requests = len(provider.requests)
            f = w.WorkspaceContract()
            f.setUp()
            try:
                a = f.agent_update(f.lead(), threadId=tid)
                task = {'id': a['id'] + ':' + terminal['itemId'], 'agent': a['id'],
                        'itemId': terminal['itemId'], 'processId': terminal['processId'],
                        'kind': 'command', 'status': 'running'}
                with f.runtime.db() as db:
                    f.runtime.put(db, 'tasks', task)
                with patch.object(f.runtime, 'connect', return_value=server), \
                        patch.object(f.runtime, 'send', side_effect=AssertionError('Unexpected model input')):
                    self.assertEqual(f.runtime.native_command_action({'id': task['id'], 'action': 'cancel'}),
                                     {'terminated': True})
                self.assertEqual(server.call('thread/backgroundTerminals/list', {'threadId': tid})['data'], [])
                completion = n.until(lambda: next((e for e in notifications if e.get('method') == 'item/completed'
                    and e['params']['item'].get('id') == terminal['itemId']), None), 'native process exit')
                self.assertIsInstance(completion['params']['item']['exitCode'], int)
                self.assertNotEqual(completion['params']['item']['exitCode'], 0)
                self.assertEqual(len(provider.requests), model_requests)
                print(json.dumps({'nativeStop': True, 'modelRequestsForStop': 0,
                                  'exitCode': completion['params']['item']['exitCode']}))
            finally:
                f.tearDown()

    def test_native_voice_requires_v1_or_v3_for_webrtc(self):
        with native_server() as (server, _, provider, notifications, _):
            tid = server.call('thread/start', {'cwd': str(Path(__file__).parent.resolve()),
                'model': 'gpt-5.6-sol', 'approvalPolicy': 'never', 'sandbox': 'read-only',
                'config': {**n.THREAD_CONFIG, 'model_provider': 'local-probe',
                           'features.realtime_conversation': True}})['thread']['id']
            server.call('thread/realtime/start', {'threadId': tid, 'version': 'v2',
                'outputModality': 'audio', 'clientManagedHandoffs': True,
                'includeStartupContext': False,
                'transport': {'type': 'webrtc', 'sdp': 'v=0\r\n'}})
            event = n.until(lambda: next((e for e in notifications
                if e.get('method') == 'thread/realtime/error'
                and e['params']['threadId'] == tid), None), 'native voice compatibility error')
            self.assertEqual(event['params']['message'], 'AVAS realtime calls require realtime v1 or v3')
            self.assertEqual(provider.requests, [])
            self.assertEqual(provider.unexpected, [])

    def test_idle_native_thread_can_enable_voice_without_restart(self):
        with native_server() as (server, tid, provider, notifications, _):
            provider.release.set()
            server.call('turn/start', {'threadId': tid, 'input': [{'type': 'text', 'text': 'Materialize this local fixture thread.'}]})
            n.until(lambda: any(e.get('method') == 'turn/completed' and e['params']['threadId'] == tid for e in notifications), 'fixture turn')
            params = {'threadId': tid, 'version': 'v3', 'outputModality': 'audio',
                      'includeStartupContext': False,
                      'transport': {'type': 'webrtc', 'sdp': 'v=0\r\n'}}
            with self.assertRaisesRegex(Exception, 'does not support realtime conversation'):
                server.call('thread/realtime/start', params)
            server.call('thread/unsubscribe', {'threadId': tid})
            server.call('thread/resume', {'threadId': tid, 'excludeTurns': True,
                'config': {'features.realtime_conversation': True}})
            # The local provider cannot create real audio. We test feature admission,
            # then the expected native transport error without any external request.
            server.call('thread/realtime/start', params)
            event = n.until(lambda: next((e for e in notifications
                if e.get('method') == 'thread/realtime/error'
                and e['params']['threadId'] == tid), None), 'native transport error')
            self.assertNotIn('does not support realtime conversation', event['params']['message'])
            self.assertEqual(len(provider.requests), 1)

    def test_native_queue_persistence_and_same_client_id_behavior(self):
        with native_server() as (server, tid, provider, notifications, restart):
            server.call('turn/start', {'threadId': tid,
                'input': [{'type': 'text', 'text': 'Hold the local response.'}]})
            self.assertTrue(provider.started.wait(10))
            request = {'threadId': tid, 'clientUserMessageId': 'same-client-id',
                       'input': [{'type': 'text', 'text': 'Queued once by the user.'}]}
            first = server.call('thread/queue/add', request)['queuedSubmission']
            second = server.call('thread/queue/add', request)['queuedSubmission']
            # Characterize 0.153.4: client identity does not deduplicate add.
            self.assertNotEqual(first['id'], second['id'])
            server.close()
            provider.release.set()
            server = restart()
            page = server.call('thread/queue/list', {'threadId': tid})
            self.assertEqual([i['id'] for i in page['data']], [first['id'], second['id']])
            server.call('thread/queue/reorder', {'threadId': tid,
                'queuedSubmissionIds': [second['id'], first['id']]})
            updated = server.call('thread/queue/update', {'threadId': tid,
                'queuedSubmissionId': first['id'], 'input': [{'type': 'text', 'text': 'Edited.'}]})
            self.assertEqual(updated['queuedSubmission']['clientUserMessageId'], 'same-client-id')
            for item in (first, second):
                self.assertTrue(server.call('thread/queue/delete', {'threadId': tid,
                    'queuedSubmissionId': item['id']})['deleted'])
            self.assertEqual(server.call('thread/queue/list', {'threadId': tid})['data'], [])
            self.assertEqual(len(provider.requests), 1)
            print(json.dumps({'nativeQueueSurvivesRestart': True, 'sameClientIdCreatesEntries': 2,
                              'queueCrudModelRequests': 0}))


if __name__ == '__main__':
    unittest.main(verbosity=2)
