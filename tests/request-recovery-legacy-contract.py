#!/usr/bin/env python3
"""Legacy recovery uses only a validated local server and a read-only database."""

from contextlib import redirect_stderr
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import io
import json
from pathlib import Path
import runpy
import sqlite3
import subprocess
import sys
import tempfile
import threading
import unittest
from unittest.mock import patch
import urllib.error
import urllib.parse

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'scripts'))
from codex_request_recovery import is_loopback_url, recover_legacy_requests


class LegacyRecovery(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix='studio legacy recovery ')
        self.addCleanup(self.tmp.cleanup)
        self.state = Path(self.tmp.name) / 'state with spaces'
        self.state.mkdir()
        self.database = self.state / 'canvas.sqlite3'
        with sqlite3.connect(self.database) as db:
            db.executescript('CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT); CREATE TABLE runtime_tool_results(id TEXT PRIMARY KEY,result TEXT);')
            for actor in [{'id': 'lead', 'threadId': 'thread-main', 'accountKey': 'default'},
                          {'id': 'other', 'threadId': 'thread-main', 'accountKey': 'account-other'},
                          {'id': 'separate', 'threadId': 'thread-separate', 'accountKey': 'default'},
                          {'id': 'blank', 'threadId': None, 'accountKey': 'default'},
                          {'id': 'child', 'parentId': 'lead', 'name': 'Worker', 'status': 'running', 'turnId': 'turn-child', 'model': 'fixture-model'}]:
                db.execute('INSERT INTO runtime_agents VALUES (?,?)', (actor['id'], json.dumps(actor)))
            db.execute('INSERT INTO runtime_tool_results VALUES (?,?)', ('thread-main:ok', json.dumps(self.result(True))))
            db.execute('INSERT INTO runtime_tool_results VALUES (?,?)', ('thread-main:error', json.dumps(self.result(False))))
            db.execute('INSERT INTO runtime_tool_results VALUES (?,?)', ('account-other:thread-main:secret', json.dumps(self.result(True, secret='Private account'))))
            db.execute('INSERT INTO runtime_tool_results VALUES (?,?)', ('thread-separate:secret', json.dumps(self.result(True, secret='Private team'))))
        self.paths = []
        self.api_status = 404
        self.desktop_redirect = None
        self.metadata = {'application': 'codex-agents', 'stateDir': str(self.state), 'pid': 1}
        outer = self
        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass
            def do_GET(self):
                outer.paths.append(self.path)
                if self.path == '/api/desktop':
                    if outer.desktop_redirect:
                        self.send_response(302)
                        self.send_header('Location', outer.desktop_redirect)
                        self.end_headers()
                        return
                    status, body = 200, outer.metadata
                elif self.path.startswith('/api/tool-requests'):
                    status, body = outer.api_status, {'error': 'Legacy endpoint unavailable'}
                else:
                    status, body = 500, {'error': 'Unexpected endpoint'}
                data = json.dumps(body).encode()
                self.send_response(status)
                self.send_header('Content-Type', 'application/json')
                self.send_header('Content-Length', str(len(data)))
                self.end_headers()
                self.wfile.write(data)
        self.server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.url = 'http://127.0.0.1:' + str(self.server.server_port)

    def tearDown(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(3)

    def result(self, success, **extra):
        return {'success': success, 'contentItems': [{'type': 'inputText', 'text': json.dumps({
            'agents': [{'id': 'child'}], 'error': None if success else 'model/list response timed out; outcome unknown', **extra})}]}

    def cli(self, *args):
        return subprocess.run([sys.executable, str(ROOT / 'scripts/codex-control'), '--url', self.url, 'requests', *args],
                              capture_output=True, text=True, timeout=5)

    def test_cli_old_endpoint_uses_exact_readonly_state_and_no_full_snapshot(self):
        before = self.database.read_bytes()
        stamp = self.database.stat().st_mtime_ns
        files = sorted(str(p) for p in self.state.iterdir())
        self.database.chmod(0o444)
        output = self.cli('lead', 'ok')
        self.assertEqual(output.returncode, 0, output.stderr)
        result = json.loads(output.stdout)
        self.assertEqual((result['legacy'], result['outcome']), (True, 'applied'))
        self.assertEqual(result['result'], self.result(True))
        self.assertEqual(result['agents'][0]['status'], 'running')
        self.assertEqual(self.paths, ['/api/tool-requests?agent=lead&request_id=ok', '/api/desktop'])
        self.assertEqual(self.database.read_bytes(), before)
        self.assertEqual(self.database.stat().st_mtime_ns, stamp)
        self.assertEqual(sorted(str(p) for p in self.state.iterdir()), files)
        with patch('codex_request_recovery.sqlite3.connect', wraps=sqlite3.connect) as connect:
            recover_legacy_requests(self.url, 'lead', 'ok')
        self.assertEqual(connect.call_args.args[0], self.database.as_uri() + '?mode=ro')
        self.assertIn('%20', connect.call_args.args[0])
        self.assertTrue(connect.call_args.kwargs['uri'])

    def test_canonical_raw_missing_and_foreign_scopes(self):
        for request_id in ['ok', 'thread-main:ok']:
            self.assertEqual(recover_legacy_requests(self.url, 'lead', request_id)['outcome'], 'applied')
        error = recover_legacy_requests(self.url, 'lead', 'error')
        self.assertEqual(error['outcome'], 'unknown')
        self.assertEqual(error['result'], self.result(False))
        for actor, request_id in [('lead', 'missing'), ('lead', 'account-other:thread-main:secret'),
                                  ('lead', 'thread-separate:secret'), ('other', 'thread-main:ok'), ('blank', 'ok')]:
            result = recover_legacy_requests(self.url, actor, request_id)
            self.assertEqual((result['stage'], result['outcome']), ('not_found', 'unknown'))
            self.assertNotIn('result', result)
        for actor, expected in [('lead', {'thread-main:ok', 'thread-main:error'}),
                                ('other', {'account-other:thread-main:secret'}),
                                ('separate', {'thread-separate:secret'}), ('blank', set())]:
            result = recover_legacy_requests(self.url, actor)
            self.assertEqual({r['id'] for r in result['requests']}, expected)
            self.assertTrue(all('result' not in r for r in result['requests']))
        with self.assertRaisesRegex(ValueError, 'Unknown managed agent'):
            recover_legacy_requests(self.url, 'missing-actor')

    def test_legacy_list_is_bounded_and_includes_ids_without_result_content(self):
        with sqlite3.connect(self.database) as db:
            for n in range(70):
                db.execute('INSERT INTO runtime_tool_results VALUES (?,?)', ('thread-main:call-' + str(n), json.dumps(self.result(True))))
        output = self.cli('lead')
        self.assertEqual(output.returncode, 0, output.stderr)
        result = json.loads(output.stdout)
        self.assertEqual(len(result['requests']), 50)
        self.assertEqual(result['requests'][0]['id'], 'thread-main:call-69')
        self.assertTrue(all(row['agentIds'] == ['child'] and 'result' not in row for row in result['requests']))
        self.assertNotIn('/api/state', self.paths)

    def test_other_http_errors_do_not_trigger_fallback(self):
        for status in [401, 403, 408, 500]:
            self.paths.clear()
            self.api_status = status
            output = self.cli('lead', 'ok')
            self.assertNotEqual(output.returncode, 0)
            self.assertEqual(self.paths, ['/api/tool-requests?agent=lead&request_id=ok'])
        self.assertNotIn('/api/desktop', self.paths)

    def test_remote_404_preserves_original_error_and_does_not_read_local_state(self):
        for base in ['https://example.invalid', self.url]:
            error = urllib.error.HTTPError('https://example.invalid/api/tool-requests?agent=lead', 404, 'Not found', {},
                                           io.BytesIO(b'{"error":"Original remote error"}'))
            stderr = io.StringIO()
            with patch.object(sys, 'argv', ['codex-control', '--url', base, 'requests', 'lead']), \
                    patch('urllib.request.urlopen', side_effect=error), \
                    patch('codex_request_recovery.recover_legacy_requests') as fallback, redirect_stderr(stderr):
                with self.assertRaises(SystemExit) as exit_code:
                    runpy.run_path(str(ROOT / 'scripts/codex-control'), run_name='__main__')
            self.assertEqual(exit_code.exception.code, 1)
            self.assertIn('Original remote error', stderr.getvalue())
            fallback.assert_not_called()
        with patch('urllib.request.build_opener') as opener:
            for url in ['https://example.invalid', 'http://127.0.0.2', 'file:///tmp/state', 'http://localhost.evil', 'http://user@localhost']:
                self.assertFalse(is_loopback_url(url))
                with self.assertRaises(ValueError):
                    recover_legacy_requests(url, 'lead')
            opener.assert_not_called()
        for url in ['http://localhost:4620', 'http://127.0.0.1', 'http://[::1]:4620']:
            self.assertTrue(is_loopback_url(url))

    def test_transport_timeout_does_not_read_local_state(self):
        with patch.object(sys, 'argv', ['codex-control', '--url', self.url, 'requests', 'lead']), \
                patch('urllib.request.urlopen', side_effect=TimeoutError('Receipt timeout')), \
                patch('codex_request_recovery.recover_legacy_requests') as fallback, redirect_stderr(io.StringIO()):
            with self.assertRaises(SystemExit) as exit_code:
                runpy.run_path(str(ROOT / 'scripts/codex-control'), run_name='__main__')
        self.assertEqual(exit_code.exception.code, 1)
        fallback.assert_not_called()

    def test_desktop_cross_origin_redirect_is_rejected_before_database_access(self):
        # A different port is another origin, even on the same loopback host.
        self.desktop_redirect = 'http://127.0.0.1:1/api/desktop'
        with patch('codex_request_recovery.sqlite3.connect') as connect:
            with self.assertRaisesRegex(ValueError, 'another origin'):
                recover_legacy_requests(self.url, 'lead', 'ok')
        connect.assert_not_called()

    def test_missing_database_never_creates_file_or_directory(self):
        missing = Path(self.tmp.name) / 'absent state'
        self.metadata['stateDir'] = str(missing)
        with self.assertRaisesRegex(ValueError, 'Cannot read legacy'):
            recover_legacy_requests(self.url, 'lead', 'ok')
        self.assertFalse(missing.exists())
        self.database.unlink()
        self.metadata['stateDir'] = str(self.state)
        with self.assertRaisesRegex(ValueError, 'Cannot read legacy'):
            recover_legacy_requests(self.url, 'lead', 'ok')
        self.assertFalse(self.database.exists())
        self.assertEqual(list(self.state.iterdir()), [])

    def test_running_database_wal_results_are_visible_without_immutable_snapshot(self):
        db = sqlite3.connect(self.database)
        try:
            self.assertEqual(db.execute('PRAGMA journal_mode=WAL').fetchone()[0], 'wal')
            db.execute('INSERT INTO runtime_tool_results VALUES (?,?)', ('thread-main:live-wal', json.dumps(self.result(True))))
            db.commit()
            files = sorted(str(p) for p in self.state.iterdir())
            result = recover_legacy_requests(self.url, 'lead', 'live-wal')
            self.assertEqual(result['outcome'], 'applied')
            self.assertEqual(sorted(str(p) for p in self.state.iterdir()), files)
        finally:
            db.close()


if __name__ == '__main__':
    unittest.main()
