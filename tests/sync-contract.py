#!/usr/bin/env python3
"""Compatibility entry point for the component-local SyncStore checks."""
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
from sync.tests.test_sync_store import SyncStoreTests

if __name__ == "__main__":
    suite = unittest.defaultTestLoader.loadTestsFromTestCase(SyncStoreTests)
    result = unittest.TextTestRunner(verbosity=2).run(suite)
    raise SystemExit(not result.wasSuccessful())
