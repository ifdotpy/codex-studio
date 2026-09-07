#!/usr/bin/env python3
"""Request recovery through the production HTTP adapter and command client."""

import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import threading
import unittest
from unittest.mock import patch
import urllib.error
import urllib.parse
import urllib.request

spec = importlib.util.spec_from_file_location('request_http_fixture', Path(__file__).with_name('spawn-request-recovery-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)
from codex_canvas import Canvas, make_server


class ToolRequestHTTP(unittest.TestCase):
    message = f.SpawnRequestRecovery.message
    response = f.SpawnRequestRecovery.response
    agent_update = f.SpawnRequestRecovery.agent_update
    lead = f.SpawnRequestRecovery.lead

    def setUp(self):
        f.SpawnRequestRecovery.setUp(self)
        self.canvas = Canvas(self.state)
        self.canvas.runtime = self.runtime
        self.server = make_server(self.canvas)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.base = 'http://127.0.0.1:' + str(self.server.server_port)

    def tearDown(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(3)
        f.SpawnRequestRecovery.tearDown(self)

    def get(self, actor=None, request_id=None, headers=None):
        query = {'agent': actor or self.actor['id']}
        if request_id is not None:
            query['request_id'] = request_id
        request = urllib.request.Request(self.base + '/api/tool-requests?' + urllib.parse.urlencode(query), headers=headers or {})
        with urllib.request.urlopen(request, timeout=3) as response:
            return json.load(response)

    def cancel(self, request_id, headers):
        request = urllib.request.Request(self.base + '/api/tool-requests/cancel',
            data=json.dumps({'agent': self.actor['id'], 'request_id': request_id}).encode(),
            headers={'Content-Type': 'application/json', **headers})
        with urllib.request.urlopen(request, timeout=3) as response:
            return json.load(response)

    def test_list_get_scope_missing_and_current_child_registry(self):
        self.runtime.dynamic(self.message())
        result = self.get(request_id='batch-1')
        self.assertEqual(result['outcome'], 'applied')
        self.assertEqual({a['id'] for a in result['agents']}, set(result['agentIds']))
        self.assertTrue(all(a['status'] == 'queued' and a['deleted'] is False for a in result['agents']))
        self.assertEqual(len(self.get()['requests']), 1)
        self.assertNotIn('result', self.get()['requests'][0])
        first, second = result['agentIds']
        self.agent_update({'id': first}, status='running', turnId='current-child-turn')
        self.agent_update({'id': second}, status='paused', deletedAt=1500000000)
        changed = self.get(request_id=result['id'])
        states = {a['id']: a for a in changed['agents']}
        self.assertEqual(states[first]['status'], 'running')
        self.assertEqual(states[first]['turnId'], 'current-child-turn')
        self.assertTrue(states[second]['deleted'])
        self.assertEqual(changed['result'], result['result'])
        self.assertGreaterEqual(changed['registryObservedAt'], result['registryObservedAt'])
        other = self.lead('Other')
        self.assertEqual(self.get(actor=other['id'])['requests'], [])
        self.assertEqual(self.get(actor=other['id'], request_id=result['id'])['stage'], 'not_found')
        self.assertEqual(self.get(request_id='missing')['outcome'], 'unknown')
        with self.assertRaises(urllib.error.HTTPError) as rejected:
            self.get(headers={'Origin': 'https://outside.invalid'})
        self.assertEqual(rejected.exception.code, 403)

    def test_missing_and_foreign_registry_ids_do_not_expose_other_agents(self):
        self.runtime.dynamic(self.message())
        receipt = self.get(request_id='batch-1')
        first, second = receipt['agentIds']
        with self.runtime.db() as db:
            db.execute('DELETE FROM runtime_agents WHERE id=?', (first,))
            changed = self.runtime.agent(second, db)
            changed.update(parentId='not-this-actor', name='Private name', turnId='Private turn')
            self.runtime.put(db, 'agents', changed)
        found = self.get(request_id='batch-1')
        self.assertEqual(found['agents'], [{'id': first, 'absent': True}, {'id': second, 'absent': True}])
        self.assertNotIn('Private', json.dumps(found['agents']))

    def test_cancel_requires_csrf_token_and_allowed_origin(self):
        self.runtime.reserve_tool_request(self.message())
        with urllib.request.urlopen(self.base + '/api/state', timeout=3) as response:
            token = json.load(response)['token']
        for headers in [{}, {'X-Canvas-Token': 'wrong'},
                        {'X-Canvas-Token': token, 'Origin': 'https://outside.invalid'}]:
            with self.assertRaises(urllib.error.HTTPError) as rejected:
                self.cancel('batch-1', headers)
            self.assertEqual(rejected.exception.code, 403)
            self.assertEqual(self.get(request_id='batch-1')['stage'], 'queued')
        cancelled = self.cancel('batch-1', {'X-Canvas-Token': token, 'Origin': self.base})
        self.assertEqual((cancelled['stage'], cancelled['outcome']), ('cancelled', 'not_applied'))
        self.assertFalse(self.runtime.begin_tool_request(cancelled['id']))

    def test_cli_list_and_get_do_not_fetch_full_state(self):
        self.runtime.dynamic(self.message())
        executable = Path(__file__).resolve().parents[1] / 'scripts' / 'codex-control'
        with patch.object(self.canvas, 'snapshot', side_effect=AssertionError('The full state endpoint must not be used')) as snapshot:
            for extra in [[], ['batch-1']]:
                output = subprocess.run([sys.executable, str(executable), '--url', self.base, 'requests', self.actor['id'], *extra],
                                        capture_output=True, text=True, timeout=5)
                self.assertEqual(output.returncode, 0, output.stderr)
                value = json.loads(output.stdout)
                if extra:
                    self.assertEqual(value['outcome'], 'applied')
                    self.assertEqual(len(value['agents']), 2)
                else:
                    self.assertEqual(len(value['requests']), 1)
            snapshot.assert_not_called()


if __name__ == '__main__':
    unittest.main()
