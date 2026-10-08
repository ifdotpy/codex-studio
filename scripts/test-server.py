#!/usr/bin/env python3
"""Run the discoverable, isolated Python server test suites."""

from pathlib import Path
import os
import runpy
import tempfile

configured_tmp_root = os.environ.get("CODEX_SERVER_TEST_TMP_ROOT")
test_tmp_root = (
    Path(configured_tmp_root).expanduser()
    if configured_tmp_root
    else Path.home() / ".cache" / "cs" / "st"
)
test_tmp_root.mkdir(parents=True, exist_ok=True)
if configured_tmp_root:
    os.environ["CODEX_SERVER_TEST_TMP_ROOT"] = str(test_tmp_root)
os.environ["TMPDIR"] = str(test_tmp_root)
tempfile.tempdir = str(test_tmp_root)

with tempfile.TemporaryDirectory(prefix="runner-") as test_cache:
    os.environ["XDG_CACHE_HOME"] = test_cache
    runpy.run_path(
        str(Path(__file__).resolve().parents[1] / "tests/server/run.py"),
        run_name="__main__",
    )
