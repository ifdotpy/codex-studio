#!/usr/bin/env python3
"""Reuse only an authoritative catalog's exact signature. No network calls."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import copy
import hashlib
import importlib.util
import json
from pathlib import Path
import sys
import tempfile
import threading
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'scripts'))
from codex_pricing import PricingCatalog
from codex_session_costs import SessionCostReader

spec = importlib.util.spec_from_file_location('pricing_snapshot_fixture',
                                            ROOT / 'tests/session-cost-snapshot-contract.py')
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


def catalog():
    result = fixture.FixedPricing().snapshot()
    result['providers']['other-provider'] = {'models': {'unicode-model': {
        'description': 'Точный каталог', 'cost': {'input': 3, 'output': 7},
        'metadata': {'enabled': False, 'tiers': [1, None, {'x': .125}]},
    }}}
    return result


def signature(value):
    if value is None:
        return None
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(',', ':'),
                                     ensure_ascii=False).encode('utf-8')).hexdigest()


class PricingSignatureContract(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix='studio-pricing-signature-')
        self.addCleanup(self.directory.cleanup)
        self.now = [1000.0]
        self.pricing = PricingCatalog(self.directory.name, fetch=catalog, clock=lambda: self.now[0])
        self.pricing._refresh()

    def test_existing_object_lazily_caches_exact_full_catalog_once(self):
        value = self.pricing.snapshot()
        expected = signature(value)
        original = json.dumps
        calls = []

        def observed(item, *args, **kwargs):
            if item is value:
                calls.append((args, kwargs))
            return original(item, *args, **kwargs)

        # The private cache is absent on both newly created and old live objects.
        self.assertNotIn('_signature_snapshot', vars(self.pricing))
        with patch('codex_pricing.json.dumps', side_effect=observed):
            for _ in range(5):
                selected, digest = self.pricing.snapshot_with_signature()
                self.assertIs(selected, value)
                self.assertEqual(digest, expected)
        self.assertEqual(len(calls), 1)
        self.assertEqual(calls[0][1], {'sort_keys': True, 'separators': (',', ':'), 'ensure_ascii': False})

    def test_replacement_including_other_provider_metadata_invalidates_signature(self):
        first, digest = self.pricing.snapshot_with_signature()
        changed = copy.deepcopy(first)
        changed['providers']['other-provider']['models']['unicode-model']['description'] = 'Изменённый каталог'
        self.pricing.fetch = lambda: changed
        self.pricing._refresh()
        selected, updated = self.pricing.snapshot_with_signature()
        self.assertIsNot(selected, first)
        self.assertEqual(updated, signature(changed))
        self.assertNotEqual(updated, digest)
        self.assertEqual(selected['providers']['openai'], first['providers']['openai'])

    def test_refresh_metadata_and_failed_refresh_retain_known_catalog_signature(self):
        value, digest = self.pricing.snapshot_with_signature()
        self.pricing.artifact['lastAttemptAt'] += 1
        self.pricing.artifact['fetchedAt'] = 'changed-metadata'
        self.pricing.fetch = lambda: (_ for _ in ()).throw(OSError('private offline fixture'))
        original = json.dumps

        def unchanged(item, *args, **kwargs):
            if item is value:
                raise AssertionError('The catalog must not encode again')
            return original(item, *args, **kwargs)

        with patch('codex_pricing.json.dumps', side_effect=unchanged), patch('codex_pricing.threading.Thread'):
            attempted, retained = self.pricing.snapshot_with_signature(ttl=0)
            self.assertIs(attempted, value)
            self.assertEqual(retained, digest)
            self.assertEqual(self.pricing.last_attempt, self.now[0])
            self.pricing._refresh()
            selected, retained = self.pricing.snapshot_with_signature()
        self.assertIs(selected, value)
        self.assertEqual(retained, digest)

    def test_none_remains_loading_without_encoding_or_network(self):
        pending = PricingCatalog(Path(self.directory.name) / 'loading',
                                 fetch=lambda: self.fail('No fetch is expected'), clock=lambda: 1000)
        with patch('codex_pricing.json.dumps', side_effect=AssertionError('None must not encode')):
            self.assertEqual(pending.snapshot_with_signature(), (None, None))

    def test_replacement_waits_for_exact_catalog_signature_pair(self):
        first = self.pricing.snapshot()
        replacement = copy.deepcopy(first)
        replacement['providers']['openai']['models']['gpt-6-luna']['cost']['input'] = 2
        first_digest, next_digest = signature(first), signature(replacement)
        entered, release = threading.Event(), threading.Event()
        original = json.dumps
        observed = []
        results = []
        errors = []

        def encode(value, *args, **kwargs):
            if value is first:
                observed.append(value)
                entered.set()
                if not release.wait(3):
                    raise AssertionError('The private encode gate was not released')
            return original(value, *args, **kwargs)

        def capture():
            try:
                results.append(self.pricing.snapshot_with_signature())
            except Exception as error:
                errors.append(error)

        self.pricing.fetch = lambda: replacement
        with patch('codex_pricing.json.dumps', side_effect=encode):
            reader = threading.Thread(target=capture)
            reader.start()
            self.assertTrue(entered.wait(3))
            writer = threading.Thread(target=self.pricing._refresh)
            writer.start()
            try:
                self.assertIs(self.pricing.artifact['catalog'], first)
            finally:
                release.set()
                reader.join(3)
                writer.join(3)
        self.assertFalse(reader.is_alive())
        self.assertFalse(writer.is_alive())
        self.assertEqual(errors, [])
        self.assertIs(results[0][0], first)
        self.assertEqual(results[0][1], first_digest)
        current, digest = self.pricing.snapshot_with_signature()
        self.assertIsNot(current, first)
        self.assertEqual(digest, next_digest)
        self.assertEqual(len(observed), 1)

    def test_fake_provider_in_place_edits_keep_uncached_signature(self):
        class MutablePricing:
            value = catalog()

            def snapshot(self):
                return self.value

        pricing = MutablePricing()
        reader = SessionCostReader(Path(self.directory.name) / 'unused.sqlite3', pricing)
        first, before = reader._pricing_snapshot()
        pricing.value['providers']['openai']['models']['gpt-6-luna']['cost']['input'] = 2
        second, after = reader._pricing_snapshot()
        self.assertIs(first, second)
        self.assertNotEqual(before, after)
        self.assertEqual(after, signature(second))

    def test_custom_catalog_subclass_keeps_uncached_signature(self):
        class MutableCatalog(PricingCatalog):
            def snapshot(self, *, ttl=None):
                return self.value

        pricing = MutableCatalog(Path(self.directory.name) / 'custom', clock=lambda: 1000)
        pricing.value = catalog()
        reader = SessionCostReader(Path(self.directory.name) / 'unused.sqlite3', pricing)
        _, before = reader._pricing_snapshot()
        pricing.value['providers']['other-provider']['models']['unicode-model']['description'] = 'changed'
        _, after = reader._pricing_snapshot()
        self.assertNotEqual(before, after)
        self.assertNotIn('_signature_snapshot', vars(pricing))

    def test_foreground_and_background_consumers_encode_once_per_catalog(self):
        base = fixture.SessionCostSnapshotContract('runTest')
        with base.fixture() as (reader, *_):
            reader.pricing = self.pricing
            value = self.pricing.snapshot()
            original = json.dumps
            calls = []

            def observed(item, *args, **kwargs):
                if item is value:
                    calls.append(True)
                return original(item, *args, **kwargs)

            with patch('codex_pricing.json.dumps', side_effect=observed):
                first = reader.snapshot('lead', wait=True)
                reader.snapshot('lead', wait=True)
                background = reader._compute('lead', 'lead')
            self.assertEqual(first['pricingState'], 'ready')
            self.assertEqual(background['totalUSD'], first['totalUSD'])
            self.assertEqual(len(calls), 1, 'Both cost callers must share one canonical catalog signature')
            replacement = copy.deepcopy(value)
            replacement['providers']['openai']['models']['gpt-6-luna']['cost']['input'] *= 2
            self.pricing.fetch = lambda: replacement
            self.pricing._refresh()
            updated = reader._compute('lead', 'lead')
            self.assertGreater(updated['totalUSD'], first['totalUSD'])
            self.assertNotEqual(updated['_cacheSource']['pricingSignature'], background['_cacheSource']['pricingSignature'])


if __name__ == '__main__':
    unittest.main()
