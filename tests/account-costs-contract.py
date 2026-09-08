#!/usr/bin/env python3
"""Cost profile isolation, shared scan admission, and real HTTP account routing."""
import concurrent.futures
import importlib.util
import json
from pathlib import Path
import sys
import tempfile
import threading
import time
import types
import unittest
from unittest.mock import patch
from urllib.request import urlopen
from urllib.error import HTTPError
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
from codex_costs import AccountCostReader, CostReader
from codex_canvas import Canvas, make_server
spec = importlib.util.spec_from_file_location('cost_fixture', Path(__file__).with_name('costs-contract.py'))
fixture = importlib.util.module_from_spec(spec); spec.loader.exec_module(fixture)

class Accounts:
    def __init__(self, root):
        self.rows = {k: {'id': k, 'accountId': 'native-' + k, 'home': str(root / k), 'status': 'ready'}
                     for k in ('default', 'work')}
    def get(self, key):
        if key not in self.rows: raise ValueError('Unknown Codex account')
        return dict(self.rows[key])

class AccountCosts(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(); self.root = Path(self.temp.name)
        self.accounts = Accounts(self.root)
        self.cli = self.root / 'scan.py'
        self.cli.write_text('''import json,pathlib,sys,time
home, cache, gate = map(pathlib.Path, sys.argv[1:])
cache.mkdir(parents=True,exist_ok=True)
with gate.open('x'):
 try:
  time.sleep(.04)
  with (home/'calls').open('a') as stream: stream.write('scan\\n')
  print((home/'payload.json').read_text())
 finally: gate.unlink()
''')
        for key, row in self.accounts.rows.items():
            home = Path(row['home']); home.mkdir()
            payload = fixture.report(); payload[0]['sessionCostUSD'] = 5 if key == 'default' else 17
            (home/'payload.json').write_text(json.dumps(payload))
        self.factory = lambda home, cache: [sys.executable, str(self.cli), str(home), str(cache), str(self.root/'active-scan')]
        self.reader = AccountCostReader(self.root, self.accounts, command_factory=self.factory)
    def tearDown(self):
        self.reader.close(); self.temp.cleanup()
    def ready(self, key):
        deadline = time.monotonic()+10
        while time.monotonic()<deadline:
            value = self.reader.snapshot(key)
            if not value['refreshing']: return value
            time.sleep(.01)
        self.fail('Local scanner did not finish')
    def test_accounts_get_separate_totals_and_one_shared_scan_slot(self):
        with concurrent.futures.ThreadPoolExecutor(max_workers=30) as pool:
            results = list(pool.map(self.reader.snapshot, ['default','work']*20))
        for key, usd in [('default',5),('work',17)]:
            value = self.ready(key)
            self.assertIsNone(value['error']); self.assertEqual(value['data']['todayUSD'],usd)
            self.assertEqual(value['accountKey'],key)
            self.assertEqual((Path(self.accounts.rows[key]['home'])/'calls').read_text(),'scan\n')
        self.assertEqual(len(self.reader.readers),2)
        self.assertEqual(len({str(r.path) for r in self.reader.readers.values()}),2)
    def test_restart_reuses_only_own_snapshot(self):
        self.ready('work'); self.reader.close()
        self.reader = AccountCostReader(self.root,self.accounts,command_factory=self.factory)
        self.assertEqual(self.reader.snapshot('work')['data']['todayUSD'],17)
        self.assertEqual((self.root/'work/calls').read_text(),'scan\n')
        self.assertEqual(self.ready('default')['data']['todayUSD'],5)
    def test_unknown_or_changed_account_never_scans(self):
        with self.assertRaises(ValueError): self.reader.snapshot('../default')
        self.accounts.rows['work']['status']='changed'
        with self.assertRaises(ValueError): self.reader.snapshot('work')
        self.assertEqual(self.reader.readers,{})
    def test_profile_replacement_cannot_reuse_old_costs(self):
        self.ready('work')
        self.accounts.rows['work']['accountId']='replacement'
        value = self.reader.snapshot('work')
        self.assertIsNone(value['data'])
        self.assertEqual(self.ready('work')['data']['accountId'],'replacement')
        self.assertEqual(len(self.reader.readers),2)
    def test_read_failure_does_not_return_other_accounts_costs(self):
        self.ready('default')
        (self.root/'work/payload.json').write_text('invalid')
        value = self.ready('work')
        self.assertIsNone(value['data']); self.assertTrue(value['error'])
        for _ in range(20): self.reader.snapshot('work')
        self.assertEqual((self.root/'work/calls').read_text(),'scan\n')
    def test_live_route_update_preserves_server_and_rejects_wrong_fingerprint(self):
        from codex_account_cost_update import apply
        from codex_efficiency_update import fingerprint
        canvas=Canvas(self.root/'update')
        canvas.runtime=types.SimpleNamespace(accounts=self.accounts,lock=threading.RLock(),closed=False)
        server=make_server(canvas)
        before=server.RequestHandlerClass.do_GET
        close=server.server_close
        try:
            with self.assertRaisesRegex(RuntimeError,'HTTP methods changed'):
                apply(server,'unknown',fingerprint(close))
            self.assertIs(server.RequestHandlerClass.do_GET,before)
            result=apply(server,fingerprint(before),fingerprint(close))
            self.assertEqual(result['status'],'applied')
            self.assertEqual(server.fileno()>=0,True)
            function=server.RequestHandlerClass.do_GET
            closure=dict(zip(function.__code__.co_freevars,[c.cell_contents for c in function.__closure__]))
            closure['manager'].command_factory=self.factory
            t=threading.Thread(target=server.serve_forever,daemon=True);t.start()
            try:
                url=f'http://127.0.0.1:{server.server_port}'
                value=json.load(urlopen(url+'/api/costs?account_key=work'))
                self.assertEqual(value['accountKey'],'work')
                self.assertIn('token',json.load(urlopen(url+'/api/session')))
                deadline=time.monotonic()+5
                while closure['manager'].snapshot('work')['refreshing'] and time.monotonic()<deadline: time.sleep(.01)
                self.assertEqual(closure['manager'].snapshot('work')['data']['todayUSD'],17)
            finally: server.shutdown();t.join()
        finally: server.server_close()

    def test_http_routes_account_and_rejects_unknown(self):
        canvas = Canvas(self.root/'server')
        canvas.runtime=types.SimpleNamespace(accounts=self.accounts,lock=threading.RLock())
        with patch('codex_costs.AccountCostReader',return_value=self.reader):
            server=make_server(canvas); t=threading.Thread(target=server.serve_forever,daemon=True);t.start()
            try:
                origin=f'http://127.0.0.1:{server.server_port}/api/costs'
                value=json.load(urlopen(origin+'?account_key=work'))
                self.assertEqual(value['accountKey'],'work')
                self.assertEqual(self.ready('work')['data']['todayUSD'],17)
                with self.assertRaises(HTTPError) as error: urlopen(origin+'?account_key=foreign')
                self.assertEqual(error.exception.code,400);error.exception.close()
            finally: server.shutdown();t.join();server.server_close()

if __name__=='__main__': unittest.main()
