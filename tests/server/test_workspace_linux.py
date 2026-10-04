"""Tests for the Linux overlayfs workspace backend."""

from __future__ import annotations

import json
import os
from pathlib import Path
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


@unittest.skipUnless(sys.platform.startswith("linux")
                     and os.environ.get("CODEX_WORKSPACE_LINUX_INTEGRATION") == "1",
                     "set CODEX_WORKSPACE_LINUX_INTEGRATION=1 on Linux")
class LinuxOverlayIntegrationTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="studio-ws-linux-")
        self.root = Path(self.temp.name)
        self.old_store = os.environ.get("CODEX_WORKSPACE_STORE")
        store_root = Path(os.environ.get("CODEX_WORKSPACE_LINUX_STORE_ROOT", Path.home()))
        self.store = store_root / f"studio-ws-test-{os.getpid()}"
        os.environ["CODEX_WORKSPACE_STORE"] = str(self.store)
        self.backend = linux.Backend()

    def run_in_namespace(self, *command):
        result = subprocess.run(self.backend.exec_prefix() + list(command), check=False,
                                text=True, capture_output=True)
        if result.returncode:
            raise AssertionError(result.stderr.strip())
        return result

    def tearDown(self):
        storage = self.store / "fixture"
        for agent in ("a", "b"):
            try:
                self.backend.unmount_workspace(storage / "mnt" / agent)
                self.backend.remove_layer(storage / "agents" / agent)
            except (OSError, RuntimeError):
                pass
        try:
            self.backend.remove_base_version(self.store / "bases" / "repo-key" / "v1")
        except (OSError, RuntimeError):
            pass
        if self.old_store is None:
            os.environ.pop("CODEX_WORKSPACE_STORE", None)
        else:
            os.environ["CODEX_WORKSPACE_STORE"] = self.old_store
        if self.store.exists():
            import shutil
            try:
                shutil.rmtree(self.store)
            except PermissionError:
                subprocess.run(["chmod", "-R", "u+rwx", str(self.store)], check=False)
                shutil.rmtree(self.store)
        self.temp.cleanup()

    def test_overlay_lifecycle_and_git_exec_prefix(self):
        source = self.root / "source"
        source.mkdir()
        subprocess.run(["git", "init", "-q", str(source)], check=True)
        subprocess.run(["git", "-C", str(source), "config", "user.name", "Studio Test"], check=True)
        subprocess.run(["git", "-C", str(source), "config", "user.email", "studio@example.test"], check=True)
        (source / "tracked.txt").write_text("base\n", encoding="utf-8")
        subprocess.run(["git", "-C", str(source), "add", "-A"], check=True)
        subprocess.run(["git", "-C", str(source), "commit", "-qm", "base"], check=True)

        storage = self.store / "fixture"
        staging = self.backend.open_base_staging(source, "repo-key", "v1")
        self.backend.copy_base_tree(source, staging["root"])
        sealed = self.backend.seal_base(staging)
        image = Path(sealed["image"])
        self.assertFalse((image / "repo" / ".git" / "objects").exists())
        layer_a = self.backend.clone_workspace(image, storage / "agents" / "a")
        layer_b = self.backend.clone_workspace(image, storage / "agents" / "b")
        mount_a, mount_b = storage / "mnt" / "a", storage / "mnt" / "b"
        self.backend.mount_workspace(layer_a, mount_a, base_image=image)
        self.backend.mount_workspace(layer_b, mount_b, base_image=image)
        self.backend.mount_workspace(layer_a, mount_a, base_image=image)
        repo_a, repo_b = mount_a / "repo", mount_b / "repo"
        self.run_in_namespace("python3", "-c",
                              "import pathlib,sys; p=pathlib.Path(sys.argv[1]); "
                              "p.parent.mkdir(parents=True,exist_ok=True); p.write_text(sys.argv[2])",
                              str(repo_a / ".git" / "objects" / "info" / "alternates"),
                              str(source / ".git" / "objects") + "\n")
        self.run_in_namespace("python3", "-c", "import pathlib,sys; pathlib.Path(sys.argv[1]).write_text('a')",
                              str(repo_a / "agent-only"))
        absent = subprocess.run(self.backend.exec_prefix() + ["test", "!", "-e", str(repo_b / "agent-only")],
                                check=False, capture_output=True)
        self.assertEqual(absent.returncode, 0)

        (source / "fresh-user-edit").write_text("fresh", encoding="utf-8")
        (source / "tracked.txt").unlink()
        (source / ".git" / "objects" / "not-a-git-object").write_text("skip", encoding="utf-8")
        self.backend.sync_delta(source, repo_a, None)
        self.assertEqual(self.run_in_namespace("cat", str(repo_a / "fresh-user-edit")).stdout, "fresh")
        for path in (repo_a / "tracked.txt", repo_a / ".git" / "objects" / "not-a-git-object"):
            absent = subprocess.run(self.backend.exec_prefix() + ["test", "!", "-e", str(path)],
                                    check=False, capture_output=True)
            self.assertEqual(absent.returncode, 0)

        # Git commands that address the mounted repository must enter its namespace.
        prefix = self.backend.exec_prefix()
        subprocess.run(prefix + ["git", "-C", str(repo_a), "status", "--short"], check=True,
                       capture_output=True, text=True)
        self.run_in_namespace("python3", "-c", "import pathlib,sys; pathlib.Path(sys.argv[1]).write_text('result')",
                              str(repo_a / "agent-result"))
        subprocess.run(prefix + ["git", "-C", str(repo_a), "add", "agent-result"], check=True)
        subprocess.run(prefix + ["git", "-C", str(repo_a), "-c", "user.name=Studio Test",
                                 "-c", "user.email=studio@example.test", "commit", "-m", "result"],
                       check=True, capture_output=True, text=True)
        self.assertEqual(self.run_in_namespace("cat", str(repo_a / "agent-result")).stdout, "result")
        self.assertGreater(self.backend.private_bytes(layer_a), 0)

        subprocess.run(prefix + ["git", "-C", str(repo_a), "branch", "codex-agent/test"], check=True)
        subprocess.run(prefix + ["git", "-C", str(source), "fetch", str(repo_a),
                                 "codex-agent/test:refs/studio/agents/test/raw"], check=True,
                       capture_output=True, text=True)
        raw_commit = subprocess.run(["git", "-C", str(source), "rev-parse",
                                     "refs/studio/agents/test/raw"], check=True,
                                    text=True, capture_output=True).stdout.strip()
        self.assertTrue(raw_commit)

        holder = linux._namespace_state_path()
        original_pid = int(json.loads(holder.read_text())["pid"])
        os.kill(original_pid, 9)
        for _ in range(100):
            if linux._proc_start_time(original_pid) is None:
                break
            import time
            time.sleep(0.02)
        self.backend.mount_workspace(layer_a, mount_a, base_image=image)
        restarted_pid = int(json.loads(holder.read_text())["pid"])
        self.assertNotEqual(original_pid, restarted_pid)
        self.assertEqual(self.run_in_namespace("cat", str(repo_a / "agent-result")).stdout, "result")

        self.backend.unmount_workspace(mount_a)
        self.backend.unmount_workspace(mount_b)
        self.backend.unmount_workspace(mount_a)
        self.backend.remove_layer(layer_a)
        self.backend.remove_layer(layer_b)
        self.backend.remove_layer(layer_a)
        self.assertFalse((storage / "agents" / "a").exists())
        self.backend.remove_base_version(image)
        self.backend.remove_base_version(image)
        self.assertFalse(image.exists())


if __name__ == "__main__":
    unittest.main(verbosity=2)
