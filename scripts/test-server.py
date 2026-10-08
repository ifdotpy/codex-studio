#!/usr/bin/env python3
"""Run the discoverable, isolated Python server test suites."""

from pathlib import Path
import os
import runpy
import tempfile

with tempfile.TemporaryDirectory(
    prefix="codex-studio-test-cache-",
) as test_cache:
    os.environ["XDG_CACHE_HOME"] = test_cache
    runpy.run_path(
        str(Path(__file__).resolve().parents[1] / "tests/server/run.py"),
        run_name="__main__",
    )
