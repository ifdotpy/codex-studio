"""install-layr.sh with stub tools in a temporary root: a rebuilt layr restarts its daemon."""
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
        source = self.root / "opt/codex-studio/vm/layr"
        source.mkdir(parents=True)
        (source / "Cargo.toml").write_text("[package]\n")
        files = {"Cargo.toml": hashlib.sha256(b"[package]\n").hexdigest()}
        total = hashlib.sha256(json.dumps(files, sort_keys=True, separators=(",", ":")).encode()).hexdigest()
        (source / "manifest.json").write_text(json.dumps({"files": files, "sourceSha256": total}))
        self.identity = total
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
                  'mkdir -p "$CARGO_TARGET_DIR/release" && echo new > "$CARGO_TARGET_DIR/release/layr"')

    @staticmethod
    def stub(path, body):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("#!/bin/bash\n" + body + "\n")
        path.chmod(0o755)

    def run_script(self):
        text = SCRIPT.read_text()
        for absolute in ("/opt/codex-studio", "/var/lib/codex-studio", "/etc/systemd/system", "/usr/local/bin"):
            text = text.replace(absolute, str(self.root) + absolute)
        (self.root / "systemctl.log").write_text("")
        subprocess.run(["bash", "-c", text], check=True, timeout=30,
                       env={**os.environ, "PATH": f"{self.root}/bin:{os.environ['PATH']}"})
        return (self.root / "systemctl.log").read_text().splitlines()

    def test_a_rebuilt_layr_restarts_its_daemon_and_an_unchanged_one_does_not(self):
        calls = self.run_script()
        self.assertEqual((self.root / "var/lib/codex-studio/layr-build").read_text().strip(), self.identity)
        self.assertEqual(calls[-1], "restart codex-studio-layr.service")
        self.assertNotIn("restart codex-studio-layr.service", self.run_script())


if __name__ == "__main__":
    unittest.main()
