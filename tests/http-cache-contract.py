#!/usr/bin/env python3
"""HTTP compression and cache rules with an isolated server and static assets."""
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
import codex_canvas


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
        canvas.snapshot = lambda: {'stateDir': str(root), 'example': 'repeat ' * 20000}
        self.server = codex_canvas.make_server(canvas)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    def tearDown(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join()
        self.web.stop()
        self.temp.cleanup()

    def get(self, path, headers=None):
        connection = http.client.HTTPConnection('127.0.0.1', self.server.server_port, timeout=5)
        try:
            connection.request('GET', path, headers=headers or {})
            response = connection.getresponse()
            return response.status, dict(response.getheaders()), response.read()
        finally:
            connection.close()

    def test_json_gzip_preserves_content_and_does_not_cache_token(self):
        code, plain_headers, plain = self.get('/api/state')
        code, headers, encoded = self.get('/api/state', {'Accept-Encoding': 'gzip'})
        self.assertEqual(code, 200)
        self.assertEqual(gzip.decompress(encoded), plain)
        self.assertLess(len(encoded), len(plain) / 5)
        self.assertEqual(headers['Content-Length'], str(len(encoded)))
        self.assertEqual(headers['Content-Encoding'], 'gzip')
        self.assertEqual(headers['Vary'], 'Accept-Encoding')
        self.assertEqual(headers['Cache-Control'], 'no-store')
        code, session_headers, session = self.get('/api/session')
        self.assertEqual(json.loads(session)['token'], json.loads(plain)['token'])
        self.assertEqual(session_headers['Cache-Control'], 'no-store')
        self.assertLess(len(session), 100)

    def test_encoding_negotiation(self):
        for value in ['gzip;q=0, *;q=1', 'br', 'gzip;q=invalid', 'identity']:
            with self.subTest(encoding=value):
                _, headers, body = self.get('/assets/main-Abcd1234.js', {'Accept-Encoding': value})
                self.assertNotIn('Content-Encoding', headers)
                self.assertEqual(body, self.script)
        _, headers, body = self.get('/assets/main-Abcd1234.js', {'Accept-Encoding': 'br, gzip;q=0.5'})
        self.assertEqual(gzip.decompress(body), self.script)

    def test_only_fingerprinted_assets_are_cached(self):
        _, headers, body = self.get('/assets/main-Abcd1234.js', {'Accept-Encoding': 'gzip'})
        self.assertIn('immutable', headers['Cache-Control'])
        self.assertIn('private', headers['Cache-Control'])
        self.assertEqual(gzip.decompress(body), self.script)
        for path in ['/', '/assets/panel-ui.js', '/assets/missing-Abcd1234.js']:
            with self.subTest(path=path):
                _, headers, _ = self.get(path)
                self.assertEqual(headers['Cache-Control'], 'no-store')

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
        self.assertEqual(headers['Cache-Control'], 'no-store')
        code, _, _ = self.get('/api/session', {'Origin': 'https://evil.example'})
        self.assertEqual(code, 403)


if __name__ == '__main__':
    unittest.main()
