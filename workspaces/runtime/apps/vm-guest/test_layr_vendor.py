"""The committed vendor includes each local Cargo member and excludes worktree edits."""
from __future__ import annotations

import hashlib
import importlib.util
import json
from pathlib import Path
import subprocess
import tempfile
import unittest


HELPER = Path(__file__).resolve().parents[3] / "tooling/apps/repository-checks/update-vm-layr.py"


class Vendor(unittest.TestCase):
    def test_archive_contains_workspace_crates_and_hashes_only_committed_files(self):
        spec = importlib.util.spec_from_file_location("update_vm_layr", HELPER)
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        with tempfile.TemporaryDirectory() as folder:
            source, destination = Path(folder) / "source", Path(folder) / "vendor"
            source.mkdir()
            def git(*args):
                return subprocess.check_output(["git", "-C", str(source), *args], stderr=subprocess.PIPE)
            git("init", "-q")
            content = {"Cargo.toml": b'[workspace]\nmembers = ["crates/model"]\n',
                       "Cargo.lock": b"locked", "LICENSE": b"MIT", "src/main.rs": b"fn main() {}",
                       "crates/model/Cargo.toml": b'[package]\nname = "model"\n',
                       "crates/model/src/lib.rs": b"pub struct Model;"}
            for name, data in content.items():
                path = source / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(data)
            git("add", ".")
            git("-c", "user.name=Test", "-c", "user.email=test@example.com", "commit", "-qm", "Fixture")
            (source / "src/main.rs").write_bytes(b"uncommitted")
            destination.mkdir()
            (destination / "obsolete.rs").touch()
            (destination / "keep.txt").touch()
            (destination / "manifest.json").write_text(json.dumps({"files": {"obsolete.rs": "old"}}))
            module.update(source, "HEAD", destination)
            manifest = json.loads((destination / "manifest.json").read_text())
            self.assertEqual(set(manifest["files"]), set(content))
            for name, data in content.items():
                self.assertEqual((destination / name).read_bytes(), data)
                self.assertEqual(manifest["files"][name], hashlib.sha256(data).hexdigest())
            identity = hashlib.sha256(json.dumps(manifest["files"], sort_keys=True, separators=(",", ":")).encode()).hexdigest()
            self.assertEqual(manifest["sourceSha256"], identity)
            self.assertFalse((destination / "obsolete.rs").exists())
            self.assertTrue((destination / "keep.txt").exists())


if __name__ == "__main__":
    unittest.main()
