"""Tests for the Linux overlayfs folder-copy backend."""

from __future__ import annotations

import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "scripts"))
import codex_workspace_linux as linux  # noqa: E402


class LinuxBackendUnitTests(unittest.TestCase):
    def test_supported_is_false_on_macos(self):
        with patch.object(linux.sys, "platform", "darwin"):
            self.assertEqual(linux.Backend().supported(Path(".")),
                             (False, "Linux overlay workspaces require Linux"))

    def test_import_has_no_linux_only_import_side_effects(self):
        self.assertTrue(callable(linux.Backend().supported))

    def test_private_bytes_counts_upper_files_and_symlinks(self):
        with tempfile.TemporaryDirectory() as temp:
            upper = Path(temp) / "u"
            upper.mkdir()
            (upper / "data").write_bytes(b"abc")
            (upper / "link").symlink_to("data")
            self.assertEqual(linux.Backend().private_bytes(Path(temp)), 7)

    def test_copy_folder_includes_git_data_and_only_excludes_requested_paths(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            source, target = root / "source", root / "target"
            (source / ".git" / "objects" / "aa").mkdir(parents=True)
            (source / "node_modules" / ".cache").mkdir(parents=True)
            (source / ".worktrees" / "legacy").mkdir(parents=True)
            (source / ".git" / "objects" / "aa" / "pack").write_bytes(b"objects")
            (source / "node_modules" / ".cache" / "cache").write_bytes(b"cache")
            (source / ".worktrees" / "legacy" / "data").write_bytes(b"exclude")
            linux._copy_folder(source, target, (".worktrees",))
            self.assertEqual((target / ".git" / "objects" / "aa" / "pack").read_bytes(), b"objects")
            self.assertEqual((target / "node_modules" / ".cache" / "cache").read_bytes(), b"cache")
            self.assertFalse((target / ".worktrees").exists())

    def test_sync_delta_does_not_add_git_specific_filters(self):
        backend = linux.Backend()
        with tempfile.TemporaryDirectory() as temp:
            source = Path(temp) / "source"
            source.mkdir()
            target = Path(temp) / "target"
            output = subprocess.CompletedProcess([], 0, "", "")
            with patch.object(backend, "_ensure_namespace", return_value=123), \
                    patch.object(linux, "_run", return_value=output) as run:
                backend.sync_delta(source, target, None, excludes=(".worktrees",))
        args = run.call_args.args[0]
        self.assertIn("--exclude=/.worktrees/***", args)
        self.assertFalse(any(".git/objects" in value for value in args if value.startswith("--exclude")))


@unittest.skipUnless(sys.platform.startswith("linux")
                     and os.environ.get("CODEX_WORKSPACE_LINUX_INTEGRATION") == "1",
                     "set CODEX_WORKSPACE_LINUX_INTEGRATION=1 on Linux")
class LinuxOverlayIntegrationTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="studio-ws-linux-")
        self.root = Path(self.temp.name)
        self.source = self.root / "source"
        self.source.mkdir()
        self.old_store = os.environ.get("CODEX_WORKSPACE_STORE")
        store_root = Path(os.environ.get("CODEX_WORKSPACE_LINUX_STORE_ROOT", Path.home()))
        self.store = store_root / f"studio-ws-test-{os.getpid()}"
        os.environ["CODEX_WORKSPACE_STORE"] = str(self.store)
        self.backend = linux.Backend()
        self.image = None
        self.mounts = []
        self.layers = []

    def run_in_namespace(self, *command):
        result = subprocess.run(self.backend.exec_prefix() + list(command), check=False,
                                text=True, capture_output=True)
        if result.returncode:
            raise AssertionError(result.stderr.strip())
        return result

    def namespace_path_exists(self, path: Path) -> bool:
        return subprocess.run(self.backend.exec_prefix() + ["test", "-e", str(path)],
                              check=False).returncode == 0

    def namespace_read_bytes(self, path: Path) -> bytes:
        result = self.run_in_namespace("cat", str(path))
        return result.stdout.encode()

    def tearDown(self):
        for mount in self.mounts:
            try:
                self.backend.unmount_workspace(mount)
            except (OSError, RuntimeError):
                pass
        for layer in self.layers:
            try:
                self.backend.remove_layer(layer)
            except (OSError, RuntimeError):
                pass
        if self.image is not None:
            try:
                self.backend.remove_base_version(self.image)
            except (OSError, RuntimeError):
                pass
        if self.old_store is None:
            os.environ.pop("CODEX_WORKSPACE_STORE", None)
        else:
            os.environ["CODEX_WORKSPACE_STORE"] = self.old_store
        if self.store.exists():
            try:
                shutil.rmtree(self.store)
            except PermissionError:
                subprocess.run(["chmod", "-R", "u+rwx", str(self.store)], check=False)
                shutil.rmtree(self.store)
        self.temp.cleanup()

    def test_base_copy_delta_and_overlay_isolation(self):
        subprocess.run(["git", "-C", str(self.source), "init", "-q"], check=True)
        subprocess.run(["git", "-C", str(self.source), "config", "user.name", "Test"], check=True)
        subprocess.run(["git", "-C", str(self.source), "config", "user.email", "test@example.com"], check=True)
        git = self.source / ".git"
        (git / "objects" / "aa").mkdir(parents=True, exist_ok=True)
        (git / "objects" / "aa" / "base-object").write_bytes(b"base object")
        (self.source / "tracked.txt").write_text("base\n", encoding="utf-8")
        subprocess.run(["git", "-C", str(self.source), "add", "tracked.txt"], check=True)
        (self.source / "untracked.txt").write_text("untracked\n", encoding="utf-8")
        (self.source / "ignored-output").write_text("ignored\n", encoding="utf-8")
        (self.source / "node_modules" / ".cache").mkdir(parents=True)
        (self.source / "node_modules" / ".cache" / "cache").write_bytes(b"cache")
        (self.source / ".worktrees" / "legacy").mkdir(parents=True)
        (self.source / ".worktrees" / "legacy" / "data").write_bytes(b"exclude")
        excludes = (".worktrees",)

        staging = self.backend.open_base_staging(self.source, "repo-key", "v1")
        self.backend.copy_base_tree(self.source, staging["root"], excludes=excludes)
        sealed = self.backend.seal_base(staging)
        self.image = Path(sealed["image"])
        self.assertTrue(linux._btrfs_path(self.image.parent), "integration store must use btrfs")
        base = self.image / "repo"
        self.assertEqual((base / ".git" / "objects" / "aa" / "base-object").read_bytes(),
                         b"base object")
        self.assertEqual((base / ".git" / "index").read_bytes(), (git / "index").read_bytes())
        self.assertEqual((base / "node_modules" / ".cache" / "cache").read_bytes(), b"cache")
        self.assertFalse((base / ".worktrees").exists())

        storage = self.store / "agents"
        layer_a = self.backend.clone_workspace(self.image, storage / "a")
        layer_b = self.backend.clone_workspace(self.image, storage / "b")
        self.layers.extend([layer_a, layer_b])
        mount_a, mount_b = self.store / "mnt" / "a", self.store / "mnt" / "b"
        self.mounts.extend([mount_a, mount_b])
        self.backend.mount_workspace(layer_a, mount_a, base_image=self.image)
        self.backend.mount_workspace(layer_b, mount_b, base_image=self.image)
        repo_a, repo_b = mount_a / "repo", mount_b / "repo"

        (self.source / "tracked.txt").write_text("changed\n", encoding="utf-8")
        (self.source / ".git" / "objects" / "aa" / "delta-object").write_bytes(b"delta object")
        (self.source / "new-ignored-or-untracked").write_bytes(b"new file")
        (self.source / ".worktrees" / "new").mkdir()
        (self.source / ".worktrees" / "new" / "data").write_bytes(b"excluded")
        delta = self.backend.sync_delta(self.source, repo_a, sealed["token"], excludes=excludes)
        self.assertIn("tracked.txt", delta["changedPaths"])
        self.assertIn(".git/objects/aa/delta-object", delta["changedPaths"])
        self.assertEqual(self.namespace_read_bytes(repo_a / "tracked.txt"), b"changed\n")
        self.assertEqual(self.namespace_read_bytes(
            repo_a / ".git" / "objects" / "aa" / "delta-object"), b"delta object")
        self.assertFalse(self.namespace_path_exists(repo_a / ".worktrees" / "new"))
        self.assertFalse(self.namespace_path_exists(
            repo_b / ".git" / "objects" / "aa" / "delta-object"))
        self.run_in_namespace("sh", "-c", "printf 'agent A' > \"$1\"", "sh",
                              str(repo_a / "agent-only"))
        self.assertTrue(self.namespace_path_exists(repo_a / "agent-only"))
        self.assertFalse(self.namespace_path_exists(repo_b / "agent-only"))
        self.assertFalse((self.source / "agent-only").exists())

        prefix = self.backend.exec_prefix()
        result = subprocess.run(prefix + ["git", "-C", str(repo_a), "status", "--short"],
                                check=False, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)


if __name__ == "__main__":
    unittest.main(verbosity=2)
