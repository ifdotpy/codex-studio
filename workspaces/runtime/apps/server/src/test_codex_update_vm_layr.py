"""update-vm-layr.py vendors one committed revision: the selected paths only, never the working
tree, with a manifest; files of the previous manifest that the new revision lacks go away."""
from __future__ import annotations

import hashlib
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
from typing import Any
import unittest

SCRIPT = Path(__file__).with_name("update-vm-layr.py")
spec = importlib.util.spec_from_file_location("update_vm_layr", SCRIPT)
assert spec and spec.loader
update_vm_layr = importlib.util.module_from_spec(spec)
spec.loader.exec_module(update_vm_layr)


class UpdateVmLayrTests(unittest.TestCase):
    def setUp(self) -> None:
        temp = tempfile.TemporaryDirectory(prefix="update-vm-layr-")
        self.addCleanup(temp.cleanup)
        self.source = Path(temp.name) / "layr-desk"
        self.destination = Path(temp.name) / "vendored"
        self.source.mkdir()
        self.git("init", "-q")
        self.git("config", "user.email", "test@example.com")
        self.git("config", "user.name", "Test")

    def git(self, *args: str) -> str:
        return subprocess.run(["git", "-C", str(self.source), *args], check=True, capture_output=True,
                              text=True, timeout=30).stdout.strip()

    def commit(self, files: dict[str, str]) -> str:
        for name, text in files.items():
            path = self.source / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text)
        self.git("add", "-A")
        self.git("commit", "-qm", "change")
        return self.git("rev-parse", "HEAD")

    def vendor(self, revision: str) -> dict[str, Any]:
        subprocess.run([sys.executable, str(SCRIPT), "--name", "layr-desk", "--source", str(self.source),
                        "--revision", revision, "--destination", str(self.destination)],
                       check=True, capture_output=True, timeout=60)
        manifest: dict[str, Any] = json.loads((self.destination / "manifest.json").read_text())
        return manifest

    def test_one_revision_with_its_manifest_and_without_the_files_it_dropped(self) -> None:
        first = self.commit({"Cargo.toml": "[workspace]\n", "Cargo.lock": "# lock\n", "LICENSE": "MIT\n",
                             "crates/desk-core/src/lib.rs": "// core\n", "crates/desk-core/src/old.rs": "// old\n",
                             "crates/layr-desk/src/main.rs": "fn main() {}\n", "js/vault/index.mjs": "// js\n",
                             "README.md": "# desk\n"})
        # A working-tree change is never read.
        (self.source / "crates/desk-core/src/lib.rs").write_text("// not committed\n")
        manifest = self.vendor("HEAD")
        self.assertEqual(manifest["revision"], first)
        self.assertEqual(manifest["tree"], self.git("rev-parse", first + "^{tree}"))
        self.assertEqual(sorted(manifest["files"]), ["Cargo.lock", "Cargo.toml", "LICENSE", "crates/desk-core/src/lib.rs",
                                                     "crates/desk-core/src/old.rs", "crates/layr-desk/src/main.rs"])
        self.assertEqual((self.destination / "crates/desk-core/src/lib.rs").read_text(), "// core\n")
        self.assertEqual(manifest["files"]["crates/desk-core/src/lib.rs"], hashlib.sha256(b"// core\n").hexdigest())
        identity = hashlib.sha256(json.dumps(manifest["files"], sort_keys=True, separators=(",", ":")).encode()).hexdigest()
        self.assertEqual(manifest["sourceSha256"], identity)
        self.assertFalse((self.destination / "js").exists())
        self.assertFalse((self.destination / "README.md").exists())

        (self.destination / "notes.txt").write_text("not vendored\n")
        self.git("checkout", "-q", "--", ".")
        self.git("rm", "-q", "crates/desk-core/src/old.rs")
        second = self.commit({"crates/desk-core/src/new.rs": "// new\n"})
        manifest = self.vendor(second)
        self.assertEqual(manifest["revision"], second)
        self.assertIn("crates/desk-core/src/new.rs", manifest["files"])
        self.assertNotIn("crates/desk-core/src/old.rs", manifest["files"])
        self.assertFalse((self.destination / "crates/desk-core/src/old.rs").exists())
        self.assertTrue((self.destination / "notes.txt").exists(), "only files of the previous manifest go away")
        # The same revision again gives the same manifest.
        self.assertEqual(self.vendor(first)["revision"], first)
        self.assertEqual(self.vendor(second), manifest)

    def test_layr_keeps_its_paths(self) -> None:
        self.assertEqual(update_vm_layr.SOURCES["layr"], ("src", "Cargo.toml", "Cargo.lock", "LICENSE"))
        self.assertEqual(update_vm_layr.SOURCES["layr-desk"],
                         ("Cargo.toml", "Cargo.lock", "crates/desk-core", "crates/layr-desk", "LICENSE"))


if __name__ == "__main__":
    unittest.main()
