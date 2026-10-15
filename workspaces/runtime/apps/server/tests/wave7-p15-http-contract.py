#!/usr/bin/env python3
"""P15 operation witnesses reuse the shared monitor HTTP contract unchanged."""
from __future__ import annotations

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import importlib.util
from pathlib import Path
import unittest

source = Path(__file__).with_name("rust-monitors-http-contract.py")
spec = importlib.util.spec_from_file_location("p15_monitor_contract", source)
assert spec is not None and spec.loader is not None
contract = importlib.util.module_from_spec(spec)
spec.loader.exec_module(contract)

if __name__ == "__main__":
    names = [
        "test_terminal_monitor_cancel_is_idempotent_and_input_is_inactive",
        "test_pending_monitor_cancel_expires_approval_and_records_exit",
        "test_running_monitor_cancel_persists_local_request_without_a_live_session",
    ]
    suite = unittest.TestSuite(contract.MonitorsHTTPContract(name) for name in names)
    result = unittest.TextTestRunner(verbosity=2).run(suite)
    raise SystemExit(not result.wasSuccessful())
