#!/usr/bin/env python3
"""Worker account admission skips unavailable preferences without moving explicit work."""

# Support direct execution without runner-provided PYTHONPATH.
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

from contextlib import contextmanager
import unittest

import sys
from pathlib import Path
sys.path.insert(0, str(SERVER_SOURCE_ROOT))

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


class Pricing:
    def snapshot(self, **_options):
        model = lambda i, o: {'cost': {'input': i, 'output': o}}
        return {'providers': {
            'openai': {'models': {'gpt-6-sol': model(5, 40), 'gpt-6-luna': model(1.25, 10),
                                  'gpt-6-astra': model(0.25, 2), 'gpt-6-odd': model(0.5, 50)}},
            'anthropic': {'models': {'claude-opus-5-5': model(15, 75), 'claude-sonnet-5-5': model(3, 15)}}}}


class ChoiceRuntime(Runtime):
    """The person chose Sol for subagents on Automatic; prices come from the cache."""

    def __init__(self, default='gpt-6-sol', pricing=True):
        super().__init__()
        self.default = default
        if pricing:
            self._worker_pricing = Pricing()

    def worker_defaults(self, root):
        return {'model': self.default, 'accountKey': None}

    def catalog(self, account):
        self.calls.append(account)
        return {'data': [{'model': name} for name in ('gpt-6-sol', 'gpt-6-luna', 'gpt-6-astra', 'gpt-6-odd',
                                                      'gpt-6-new')]}


class AgentModelChoice(unittest.TestCase):
    """An agent may pick a subagent model of the chosen provider that costs no more."""

    parent = {'rootId': 'lead', 'accountKey': 'first'}

    def test_default_and_cheaper_models_are_allowed(self):
        for model in (None, 'gpt-6-sol', 'gpt-6-luna', 'gpt-6-astra'):
            with self.subTest(model=model):
                account, _ = resolve(ChoiceRuntime(), self.parent, {'model': model} if model else {})
                self.assertEqual(account, 'first')

    def test_a_more_expensive_model_is_refused(self):
        rt = ChoiceRuntime(default='gpt-6-luna')
        with self.assertRaisesRegex(ValueError, 'gpt-6-sol costs more than the default subagent model gpt-6-luna'):
            resolve(rt, self.parent, {'model': 'gpt-6-sol'})
        # Cheaper input does not excuse dearer output.
        with self.assertRaisesRegex(ValueError, 'costs more'):
            resolve(rt, self.parent, {'model': 'gpt-6-odd'})

    def test_another_provider_and_unknown_prices_are_refused(self):
        with self.assertRaisesRegex(ValueError, 'Choose a Codex model for subagents'):
            resolve(ChoiceRuntime(), self.parent, {'model': 'claude-sonnet-5-5'})
        with self.assertRaisesRegex(ValueError, 'cannot compare the price of gpt-6-new'):
            resolve(ChoiceRuntime(), self.parent, {'model': 'gpt-6-new'})
        # Before the first price list the provider rule still holds, and the choice stands.
        with self.assertRaisesRegex(ValueError, 'Choose a Codex model'):
            resolve(ChoiceRuntime(pricing=False), self.parent, {'model': 'claude-sonnet-5-5'})
        self.assertEqual(resolve(ChoiceRuntime(pricing=False), self.parent, {'model': 'gpt-6-new'})[0], 'first')

    def test_a_review_keeps_its_own_default(self):
        rt = ChoiceRuntime(default='gpt-6-luna')
        self.assertEqual(resolve(rt, self.parent, {'model': 'gpt-6-sol'}, limit_choice=False)[0], 'first')


if __name__ == '__main__':
    unittest.main()
