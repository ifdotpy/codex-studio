#!/usr/bin/env python3
"""The backend and the supervisor raise the soft open-file limit for native children."""
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

from pathlib import Path
import resource
import subprocess
import sys
import unittest
from unittest import mock

SCRIPTS = SERVER_SOURCE_ROOT
PROBE = (
    "import sys, resource, subprocess; sys.path.insert(0, %r); "
    "from codex_open_file_limit import raise_open_file_limit; "
    "print(raise_open_file_limit()); "
    "print(subprocess.run([sys.executable, '-c', "
    "'import resource; print(resource.getrlimit(resource.RLIMIT_NOFILE)[0])'], "
    "capture_output=True, text=True, check=True).stdout.strip())"
) % str(SCRIPTS)


def run_with(soft, hard):
    def limit():
        resource.setrlimit(resource.RLIMIT_NOFILE, (soft, hard))
    result = subprocess.run([sys.executable, "-B", "-c", PROBE], preexec_fn=limit,
                            capture_output=True, text=True, check=True, timeout=60)
    return [int(line) for line in result.stdout.split()]


class OpenFileLimit(unittest.TestCase):
    def test_launchd_default_is_raised_and_inherited(self):
        hard = resource.getrlimit(resource.RLIMIT_NOFILE)[1]
        own, child = run_with(256, hard)
        expected = 65536 if hard == resource.RLIM_INFINITY else min(65536, hard)
        self.assertEqual((own, child), (expected, expected))

    def test_low_hard_limit_is_respected(self):
        own, child = run_with(256, 1024)
        self.assertEqual((own, child), (1024, 1024))

    def test_higher_soft_limit_is_not_lowered(self):
        hard = resource.getrlimit(resource.RLIMIT_NOFILE)[1]
        self.assertGreaterEqual(hard, 100000, 'The host hard limit must permit this contract')
        own, _ = run_with(100000, hard)
        self.assertEqual(own, 100000)


class EntryPoints(unittest.TestCase):
    """Both processes that start native app-servers raise the limit first."""

    def assert_raises_first(self, module_name):
        sys.path.insert(0, str(SCRIPTS))
        try:
            module = __import__(module_name)
        finally:
            sys.path.remove(str(SCRIPTS))
        sentinel = RuntimeError("limit raised")
        with mock.patch.object(module, "raise_open_file_limit", side_effect=sentinel), \
                mock.patch.object(sys, "argv", [module_name, "--state", "/nonexistent"]):
            with self.assertRaises(RuntimeError) as caught:
                module.main()
        self.assertIs(caught.exception, sentinel)

    def test_supervisor_raises_limit_before_serving(self):
        # The supervisor owns the native app-server processes (2026-10-04: the
        # app-server hit 256 files because only the backend raised its limit).
        self.assert_raises_first("codex_process_supervisor")

    def test_backend_raises_limit_before_serving(self):
        self.assert_raises_first("codex_canvas")


if __name__ == "__main__":
    unittest.main(verbosity=2)
