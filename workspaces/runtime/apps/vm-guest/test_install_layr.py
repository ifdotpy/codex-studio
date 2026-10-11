"""install-layr.sh with stub tools in a temporary root: a rebuilt layr restarts its daemon, and the
vault writer layr-desk is built from its own vendored source."""
from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

SCRIPT = Path(__file__).with_name("install-layr.sh")


class InstallLayrTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="install-layr-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.identity = self.vendor("layr", b"[package]\n")
        self.desk_identity = self.vendor("layr-desk", b"[workspace]\n")
        guest = self.root / "opt/codex-studio/vm/guest"
        guest.mkdir(parents=True)
        for name in ("layr", "layr-admin", "share", "share-bridge"):
            (guest / f"codex-studio-{name}.service").write_text("[Unit]\n")
        for folder in ("var/lib/codex-studio", "etc/systemd/system", "usr/local/bin", "bin"):
            (self.root / folder).mkdir(parents=True, exist_ok=True)
        stubs = {
            "id": "echo 0",
            "findmnt": "echo btrfs",
            "systemctl": f'echo "$*" >> {self.root}/systemctl.log',
            "timeout": 'shift 2; exec "$@"',
        }
        for name, body in stubs.items():
            self.stub(self.root / "bin" / name, body)
        # The build: cargo writes the release binary into the target folder.
        self.stub(self.root / "opt/codex-studio/rust/bin/cargo",
                  f'echo "$CARGO_TARGET_DIR $*" >> {self.root}/cargo.log\n'
                  'mkdir -p "$CARGO_TARGET_DIR/release"\n'
                  'case "$*" in *layr-desk*) echo desk > "$CARGO_TARGET_DIR/release/layr-desk" ;;'
                  ' *) echo new > "$CARGO_TARGET_DIR/release/layr" ;; esac')

    def vendor(self, name, data):
        """A vendored source with one file and its manifest; returns its identity."""
        source = self.root / "opt/codex-studio/vm" / name
        source.mkdir(parents=True)
        (source / "Cargo.toml").write_bytes(data)
        files = {"Cargo.toml": hashlib.sha256(data).hexdigest()}
        total = hashlib.sha256(json.dumps(files, sort_keys=True, separators=(",", ":")).encode()).hexdigest()
        (source / "manifest.json").write_text(json.dumps({"files": files, "sourceSha256": total}))
        return total

    @staticmethod
    def stub(path, body):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("#!/bin/bash\n" + body + "\n")
        path.chmod(0o755)

    def run_script(self, code=0):
        text = SCRIPT.read_text()
        for absolute in ("/opt/codex-studio", "/var/lib/codex-studio", "/etc/systemd/system", "/usr/local/bin"):
            text = text.replace(absolute, str(self.root) + absolute)
        (self.root / "systemctl.log").write_text("")
        result = subprocess.run(["bash", "-c", text], capture_output=True, text=True, timeout=30,
                                env={**os.environ, "PATH": f"{self.root}/bin:{os.environ['PATH']}"})
        self.assertEqual(result.returncode, code, result.stderr)
        self.stderr = result.stderr
        return (self.root / "systemctl.log").read_text().splitlines()

    def test_a_rebuilt_layr_restarts_its_daemon_and_an_unchanged_one_does_not(self):
        calls = self.run_script()
        self.assertEqual((self.root / "var/lib/codex-studio/layr-build").read_text().strip(), self.identity)
        self.assertEqual(calls[-1], "restart codex-studio-layr.service")
        self.assertNotIn("restart codex-studio-layr.service", self.run_script())

    def test_layr_desk_is_built_once_for_each_source_and_a_bad_manifest_stops_the_install(self):
        self.run_script()
        builds = (self.root / "cargo.log").read_text().splitlines()
        self.assertEqual(len(builds), 2)
        self.assertIn("/layr-build-cache/layr-desk-target build --manifest-path "
                      f"{self.root}/opt/codex-studio/vm/layr-desk/Cargo.toml --release --locked --jobs 2 -p layr-desk",
                      builds[1])
        self.assertEqual((self.root / "usr/local/bin/layr-desk").read_text(), "desk\n")
        self.assertEqual((self.root / "var/lib/codex-studio/layr-desk-build").read_text().strip(), self.desk_identity)
        self.run_script()
        self.assertEqual(len((self.root / "cargo.log").read_text().splitlines()), 2, "an unchanged source is kept")
        # A file that differs from the manifest stops the install before any build.
        (self.root / "opt/codex-studio/vm/layr-desk/Cargo.toml").write_text("[changed]\n")
        (self.root / "var/lib/codex-studio/layr-desk-build").unlink()
        self.run_script(code=1)
        self.assertIn("AssertionError: Cargo.toml", self.stderr)
        self.assertEqual(len((self.root / "cargo.log").read_text().splitlines()), 2)


if __name__ == "__main__":
    unittest.main()
