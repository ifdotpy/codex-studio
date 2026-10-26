#!/usr/bin/env python3
"""Run the discoverable, isolated Python server test suites."""

import os
import runpy
import tempfile

from codex_layout import SERVER_TESTS_ROOT

with tempfile.TemporaryDirectory(
    prefix="codex-studio-test-cache-",
) as test_cache:
    os.environ["XDG_CACHE_HOME"] = test_cache
    runpy.run_path(
        str(SERVER_TESTS_ROOT / "server" / "run.py"),
        run_name="__main__",
    )
