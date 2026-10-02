#!/usr/bin/env python3
"""Worker account admission skips unavailable preferences without moving explicit work."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

from contextlib import contextmanager
import unittest

import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))

from codex_catalog import CatalogPending, CatalogUnavailable, DISPLAY_READ
from codex_worker_accounts import catalog, resolve


class Accounts:
    def __init__(self):
        self.rows = {
            'first': {'id': 'first', 'provider': 'codex', 'status': 'ready'},
            'second': {'id': 'second', 'provider': 'codex', 'status': 'ready'},
        }

    def list(self):
        return list(self.rows.values())

    def get(self, key):
        return self.rows[key]

    def default(self):
        return 'first'


class Runtime:
    def __init__(self, failures=None):
        self.accounts = Accounts()
        self.failures = failures or {}
        self.calls = []

    def agent(self, key):
        return {'id': key, 'rootId': key, 'accountKey': 'first', 'model': 'gpt-6-luna'}

    def worker_defaults(self, root):
        return {'model': None, 'accountKey': None}

    @contextmanager
    def db(self):
        class DB:
            def execute(self, *args):
                return None
        yield DB()

    def catalog(self, account):
        self.calls.append(account)
        error = self.failures.get(account)
        if error:
            raise error
        return {'data': [{'model': 'gpt-6-luna'}]}


class WorkerAccountsContract(unittest.TestCase):
    def test_unavailable_preferred_catalog_falls_back_to_ready_account(self):
        for error in (CatalogPending('catalog read is pending'),
                      CatalogUnavailable('account is offline')):
            with self.subTest(error=type(error).__name__):
                rt = Runtime({'first': error})
                account, catalog = resolve(rt, {'rootId': 'lead', 'accountKey': 'first'},
                                           {'model': 'gpt-6-luna'})
                self.assertEqual(account, 'second')
                self.assertEqual(catalog['data'][0]['model'], 'gpt-6-luna')
                self.assertEqual(rt.calls, ['first', 'second'])

    def test_explicit_account_does_not_fall_back(self):
        for error in (CatalogPending('catalog read is pending'),
                      CatalogUnavailable('account is offline')):
            with self.subTest(error=type(error).__name__):
                rt = Runtime({'first': error})
                with self.assertRaises(type(error)):
                    resolve(rt, {'rootId': 'lead', 'accountKey': 'first'},
                            {'model': 'gpt-6-luna', 'account_key': 'first'})
                self.assertEqual(rt.calls, ['first'])

    def test_no_matching_catalog_reports_skipped_accounts(self):
        rt = Runtime({'first': CatalogPending('first pending'),
                      'second': CatalogUnavailable('second offline')})
        with self.assertRaisesRegex(ValueError, 'first pending.*second offline'):
            resolve(rt, {'rootId': 'lead', 'accountKey': 'first'},
                    {'model': 'gpt-6-luna'})

    def test_display_partial_marks_only_pending_accounts(self):
        rt = Runtime({'first': CatalogPending('first pending')})
        display = DISPLAY_READ.set(True)
        try:
            value = catalog(rt, 'first')
        finally:
            DISPLAY_READ.reset(display)
        self.assertTrue(value['catalogPending'])
        self.assertEqual(value['data'][0]['model'], 'gpt-6-luna')
        self.assertEqual(value['unavailableAccounts'], [
            {'accountKey': 'first', 'error': 'first pending', 'catalogPending': True}])
        rt = Runtime({'first': CatalogUnavailable('first unavailable')})
        display = DISPLAY_READ.set(True)
        try:
            value = catalog(rt, 'first')
        finally:
            DISPLAY_READ.reset(display)
        self.assertNotIn('catalogPending', value)
        self.assertNotIn('catalogPending', value['unavailableAccounts'][0])

    def test_display_all_pending_remains_distinct_from_unavailable_and_admission(self):
        rt = Runtime({'first': CatalogPending('first pending'),
                      'second': CatalogUnavailable('second unavailable')})
        display = DISPLAY_READ.set(True)
        try:
            with self.assertRaises(CatalogPending):
                catalog(rt, 'first')
        finally:
            DISPLAY_READ.reset(display)
        with self.assertRaises(ValueError):
            catalog(rt, 'first')
        rt = Runtime({'first': CatalogUnavailable('first unavailable'),
                      'second': CatalogUnavailable('second unavailable')})
        display = DISPLAY_READ.set(True)
        try:
            with self.assertRaises(ValueError):
                catalog(rt, 'first')
        finally:
            DISPLAY_READ.reset(display)


if __name__ == '__main__':
    unittest.main()
