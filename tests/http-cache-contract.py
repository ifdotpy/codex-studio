#!/usr/bin/env python3
"""HTTP compression and cache rules with an isolated server and static assets."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import gzip
from concurrent.futures import ThreadPoolExecutor
import http.client
import json
from pathlib import Path
import sys
import tempfile
import threading
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
from studio_api.testing import read_session_token
import codex_canvas
from codex_runtime import Runtime


class HttpCacheContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='studio-http-cache-')
        root = Path(self.temp.name)
        web = root / 'web'
        (web / 'assets').mkdir(parents=True)
        self.script = b'// repeated code for compression\n' * 1000
        (web / 'assets' / 'main-Abcd1234.js').write_bytes(self.script)
        (web / 'assets' / 'panel-ui.js').write_bytes(self.script)
        (web / 'index.html').write_bytes(b'<html>Current entry point</html>')
        self.web = patch.object(codex_canvas, 'WEB', web)
        self.web.start()
        canvas = codex_canvas.Canvas(root)
        self.runtime = Runtime(root)
        canvas.runtime = self.runtime
        self.server = codex_canvas.make_server(canvas)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        with self.runtime.lock, self.runtime.db() as db:
            for number in range(40):
                self.runtime.put(db, 'agents', {
                    'id': f'compression-agent-{number}', 'kind': 'agent',
                    'source': 'managed', 'status': 'idle',
                    'name': 'Repeated renderer-visible agent name ' * 8,
                })

    def tearDown(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join()
        self.runtime.close()
        self.web.stop()
        self.temp.cleanup()

    def get(self, path, headers=None):
        connection = http.client.HTTPConnection('127.0.0.1', self.server.server_port, timeout=5)
        try:
            connection.request('GET', path, headers=headers or {})
            response = connection.getresponse()
            return response.status, {key.lower(): value for key, value in response.getheaders()}, response.read()
        finally:
            connection.close()

    def test_json_gzip_preserves_content_and_does_not_cache_token(self):
        path = '/api/sync/pull?scope=state:entities:v1&limit=100'
        code, plain_headers, plain = self.get(path)
        code, headers, encoded = self.get(path, {'Accept-Encoding': 'gzip'})
        self.assertEqual(code, 200)
        self.assertEqual(gzip.decompress(encoded), plain)
        self.assertLess(len(encoded), len(plain) / 5)
        self.assertEqual(headers['content-length'], str(len(encoded)))
        self.assertEqual(headers['content-encoding'], 'gzip')
        self.assertEqual(headers['vary'], 'Accept-Encoding')
        self.assertEqual(headers['cache-control'], 'no-store')
        _, session_headers, session = self.get('/api/session')
        token = read_session_token(lambda path: json.loads(self.get(path)[2]))
        self.assertEqual(json.loads(session)['token'], token)
        self.assertNotIn(token, plain.decode())
        self.assertEqual(session_headers['cache-control'], 'no-store')
        self.assertLess(len(session), 100)

    def test_encoding_negotiation(self):
        for value in ['gzip;q=0, *;q=1', 'br', 'gzip;q=invalid', 'identity']:
            with self.subTest(encoding=value):
                _, headers, body = self.get('/assets/main-Abcd1234.js', {'Accept-Encoding': value})
                self.assertNotIn('content-encoding', headers)
                self.assertEqual(body, self.script)
        _, headers, body = self.get('/assets/main-Abcd1234.js', {'Accept-Encoding': 'br, gzip;q=0.5'})
        self.assertEqual(gzip.decompress(body), self.script)

    def test_only_fingerprinted_assets_are_cached(self):
        _, headers, body = self.get('/assets/main-Abcd1234.js', {'Accept-Encoding': 'gzip'})
        self.assertIn('immutable', headers['cache-control'])
        self.assertIn('private', headers['cache-control'])
        self.assertEqual(gzip.decompress(body), self.script)
        for path in ['/', '/assets/panel-ui.js', '/assets/missing-Abcd1234.js']:
            with self.subTest(path=path):
                _, headers, _ = self.get(path)
                self.assertEqual(headers['cache-control'], 'no-store')

    def test_parallel_module_requests_preserve_all_responses(self):
        barrier = threading.Barrier(32)
        def read_module(_):
            barrier.wait(timeout=5)
            return self.get('/assets/main-Abcd1234.js')
        with ThreadPoolExecutor(max_workers=32) as pool:
            replies = list(pool.map(read_module, range(32)))
        for code, headers, body in replies:
            self.assertEqual(code, 200)
            self.assertEqual(body, self.script)

    def test_origin_gate_precedes_asset_cache(self):
        code, headers, _ = self.get('/assets/main-Abcd1234.js', {'Origin': 'https://evil.example'})
        self.assertEqual(code, 403)
        self.assertEqual(headers['cache-control'], 'no-store')
        code, _, _ = self.get('/api/session', {'Origin': 'https://evil.example'})
        self.assertEqual(code, 403)


if __name__ == '__main__':
    unittest.main()
