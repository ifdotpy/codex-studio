#!/usr/bin/env python3
"""Run the discoverable, isolated Python server test suites."""

from pathlib import Path
import runpy

runpy.run_path(str(Path(__file__).resolve().parents[1] / "tests/server/run.py"), run_name="__main__")
