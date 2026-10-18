"""Keep route inventory extraction on the ordinary Rust test profile."""

import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

import http_runtime_conformance_ledger as ledger


class RouteManifestTests(unittest.TestCase):
    def setUp(self):
        environment = patch.dict(os.environ, {ledger.P20_OPERATION_MANIFEST_ENV: ""})
        environment.start()
        self.addCleanup(environment.stop)
        self.operation = {"method": "GET", "path": "/api/manifest-fixture"}

    def test_manifest_uses_the_workspace_test_profile_and_preserves_rows(self):
        output = (
            "test harness banner\nRUST_HTTP_OPERATION_MANIFEST="
            + json.dumps([self.operation]) + "\n"
        )
        result = subprocess.CompletedProcess([], 0, output, "")
        with patch.object(ledger.subprocess, "run", return_value=result) as run:
            self.assertEqual(
                ledger.rust_operation_manifest(),
                {("GET", self.operation["path"]): self.operation},
            )
        command = run.call_args.args[0]
        self.assertEqual(command, [
            "cargo", "test", "--locked", "-p", "studio-runtime-http", "--lib",
            "print_operation_manifest", "--", "--nocapture",
        ])
        self.assertEqual(run.call_args.kwargs["cwd"], ledger.ROOT)

    def test_prebuilt_manifest_does_not_launch_cargo(self):
        with tempfile.TemporaryDirectory(prefix="studio-route-manifest-") as scratch:
            manifest = Path(scratch) / "manifest.json"
            manifest.write_text(json.dumps([self.operation]), encoding="utf-8")
            with (
                patch.dict(os.environ, {ledger.P20_OPERATION_MANIFEST_ENV: str(manifest)}),
                patch.object(ledger.subprocess, "run") as run,
            ):
                self.assertEqual(
                    ledger.rust_operation_manifest(),
                    {("GET", self.operation["path"]): self.operation},
                )
                run.assert_not_called()

    def test_compiler_failure_remains_visible(self):
        result = subprocess.CompletedProcess([], 1, "", "manifest compilation failed")
        with (
            patch.object(ledger.subprocess, "run", return_value=result),
            self.assertRaisesRegex(RuntimeError, "manifest compilation failed"),
        ):
            ledger.rust_operation_manifest()

    def test_missing_manifest_marker_fails_closed(self):
        result = subprocess.CompletedProcess([], 0, "no manifest record\n", "")
        with (
            patch.object(ledger.subprocess, "run", return_value=result),
            self.assertRaisesRegex(RuntimeError, "did not print its operation manifest"),
        ):
            ledger.rust_operation_manifest()


if __name__ == "__main__":
    unittest.main()
