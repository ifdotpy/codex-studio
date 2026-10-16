#!/usr/bin/env python3
"""Explicitly selected account transfer latency check; no native model calls."""

import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location(
    'lazy_transfer_contract', Path(__file__).with_name('account-transfer-lazy-contract.py'))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class LazyTransferLatency(unittest.TestCase):
    def test_300_idle_members_rebind_within_one_second_and_without_native_calls(self):
        case = fixture.LazyTransferContract('test_idle_members_rebind_without_native_calls')
        case.setUp()
        try:
            elapsed = case.check_idle_members_rebind(300)
            self.assertLess(elapsed, 1.0)
        finally:
            case.tearDown()


if __name__ == '__main__':
    unittest.main()
