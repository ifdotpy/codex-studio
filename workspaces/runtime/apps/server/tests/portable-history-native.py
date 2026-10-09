#!/usr/bin/env python3
"""Read installed native history protocols in isolated state, with no model turn."""
import importlib.util
from concurrent.futures import Future
import json
import os
from pathlib import Path
import shutil
import tempfile
import unittest
from unittest.mock import patch
import uuid


def fixture(name, filename):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).with_name(filename))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


n = fixture('portable_native_fixture', 'native-primitives-integration.py')
f = fixture('portable_account_fixture', 'runtime-accounts-contract.py')
from codex_portable_history import export_history, history_context
from codex_runtime import AppServer

ROOT = Path(__file__).resolve().parents[1]


class PortableNative(unittest.TestCase):
    def detach(self, source, thread_id):
        snapshot = source.call('thread/read', {'threadId': thread_id, 'includeTurns': False})
        self.assertEqual(snapshot['thread']['id'], thread_id)
        source.call('thread/unsubscribe', {'threadId': thread_id})
        barrier = Future()
        source.after_events(lambda: barrier.set_result(None))
        barrier.result(timeout=10)

    def runtime(self, root):
        environment = patch.dict(os.environ, {'CODEX_HOME': str(root / 'isolated-home')})
        environment.start()
        self.addCleanup(environment.stop)
        rt = f.ControlledRuntime(root / 'state', f.AccountServer)
        self.addCleanup(rt.close)
        return rt

    def test_installed_codex_empty_thread_export_and_destination_params(self):
        with n.native_server() as (source, tid, provider, notifications, _):
            with tempfile.TemporaryDirectory(prefix='studio-portable-native-') as directory:
                root = Path(directory)
                rt = self.runtime(root)
                agent = rt.new_lead({'cwd': directory})
                long_output = 'Full native output marker\n' * 2000
                full_user = 'Historical user input marker\n' * 2000
                assets = [{'path': str(root / 'file "one".png'), 'name': 'file "one".png'}]
                with rt.lock, rt.db() as db:
                    rt.enqueue(db, agent, 'user', full_user, 'original-input')
                    rt.item(db, agent['id'], 'tool-output', 'output', long_output, 'Tool output')
                    rt.item(db, agent['id'], 'batch', 'user', full_user,
                            inputs=[{'id': 'original-input', 'kind': 'user', 'text': full_user, 'assets': assets}])
                agent.update(threadId=tid)
                methods = []
                submit = source.submit

                def checked_submit(method, params):
                    self.assertIn(method, {'thread/read', 'thread/turns/list', 'thread/start', 'thread/unsubscribe'})
                    methods.append(method)
                    return submit(method, params)

                with patch.object(source, 'submit', side_effect=checked_submit):
                    self.detach(source, tid)
                    archive = export_history(rt, agent, 'native-empty-transfer', source)
                    self.assertEqual(archive['counts']['native_unmaterialized'], 1)
                    records = [json.loads(line) for line in Path(archive['path']).read_text().splitlines()]
                    stored = [row for row in records if row['kind'] == 'studio_item']
                    self.assertEqual(len(stored), 2)
                    tool = next(row for row in stored if row['item']['role'] == 'output')
                    self.assertEqual(tool['fullText'], long_output)
                    batch = next(row for row in stored if row['item']['role'] == 'user')
                    self.assertEqual(batch['inputEvents'][0]['text'], full_user)
                    self.assertEqual(batch['item']['inputs'][0]['assets'], assets)
                    destination = {**agent, 'provider': 'codex', 'portableHistory': archive}
                    with patch('codex_browser.configure_browser'):
                        params = rt.new_thread_params(destination)
                    self.assertIn(archive['path'], params['developerInstructions'])
                    self.assertIn('not a new user request', params['developerInstructions'])
                    self.assertNotIn('input', params)
                    # The existing fixture directs all provider requests to
                    # loopback, even if a future regression accidentally starts one.
                    params['config']['model_provider'] = 'local-probe'
                    result = source.call('thread/start', params)
                    self.assertNotEqual(result['thread']['id'], tid)
                self.assertEqual(len(provider.requests), 0)
                self.assertNotIn('turn/start', methods)
                self.assertFalse(any(e['method'] == 'turn/started' for e in notifications))
                rt.close()

    def test_real_claude_bridge_pages_and_summary_items_without_cli_turn(self):
        with tempfile.TemporaryDirectory(prefix='studio-portable-claude-') as directory:
            root = Path(directory)
            rt = self.runtime(root)
            agent = rt.new_lead({'cwd': directory})
            native_root = root / 'bridge'
            sessions = native_root / 'sessions'
            sessions.mkdir(parents=True)
            tid = str(uuid.uuid4())
            turns = [{'id': str(uuid.uuid4()), 'status': 'completed', 'itemsView': 'full',
                      'items': [{'id': str(uuid.uuid4()), 'type': 'agentMessage',
                                 'text': f'Native Claude history item {i}'}]} for i in range(105)]
            # This is a bridge-owned logical history fixture, not a Claude CLI
            # session or a user session. Its summary forces the item-page API.
            turns[-1]['itemsView'] = 'summary'
            turns[-1]['items'] = [{'id': str(uuid.uuid4()), 'type': 'commandExecution',
                                   'aggregatedOutput': f'Native full tool result {i}'} for i in range(105)]
            logical = sessions / (tid + '.json')
            logical.write_text(json.dumps({'id': tid, 'cwd': directory, 'createdAt': 1, 'turns': turns}))
            original = logical.read_bytes()
            environment = os.environ.copy()
            environment.update(STUDIO_CLAUDE_BIN='/usr/bin/false', CLAUDE_CONFIG_DIR=str(root / 'claude-config'),
                               STUDIO_CLAUDE_ACCOUNT='fixture@example.test', STUDIO_CLAUDE_OPTIONS='{}')
            for name in ('ANTHROPIC_API_KEY', 'ANTHROPIC_AUTH_TOKEN', 'CLAUDE_CODE_OAUTH_TOKEN'):
                environment.pop(name, None)
            command = [shutil.which('node'), str(ROOT / 'scripts/claude_bridge/bridge.mjs'), str(native_root)]
            with patch('codex_claude.transport', return_value=(command, environment)):
                source = AppServer(native_root, lambda _: None, lambda _: None, lambda: None, provider='claude')
            try:
                methods = []
                submit = source.submit

                def checked_submit(method, params):
                    self.assertIn(method, {'thread/read', 'thread/turns/list', 'thread/turns/items/list', 'thread/start', 'thread/unsubscribe'})
                    methods.append(method)
                    return submit(method, params)

                with patch.object(source, 'submit', side_effect=checked_submit):
                    agent.update(provider='claude', threadId=tid)
                    self.detach(source, tid)
                    archive = export_history(rt, agent, 'real-claude-export', source)
                    self.assertEqual(archive['counts']['native_turn_page'], 2)
                    self.assertEqual(archive['counts']['native_item_page'], 2)
                    records = [json.loads(line) for line in Path(archive['path']).read_text().splitlines()]
                    exported = [turn for row in records if row['kind'] == 'native_turn_page' for turn in row['page']['data']]
                    self.assertEqual(exported, turns)
                    params = rt.new_thread_params({**agent, 'portableHistory': archive, 'model': 'default'})
                    self.assertIn(history_context(rt, archive), params['developerInstructions'])
                    target = source.call('thread/start', params)['thread']
                    self.assertNotEqual(target['id'], tid)
                    self.assertEqual(target['turns'], [])
                self.assertEqual(logical.read_bytes(), original)
                self.assertNotIn('turn/start', methods)
            finally:
                source.close()
                for stream in (source.proc.stdin, source.proc.stdout, source.proc.stderr):
                    if stream is not None:
                        stream.close()
                rt.close()


if __name__ == '__main__':
    unittest.main(verbosity=2)
