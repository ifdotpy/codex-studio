"""Opt-in end-to-end test of Mac folder sync: the Mac module, the guest module and a real
layr daemon (docs/vm-mac-sync.md). Run like test_layr_e2e.py, as root on Linux with btrfs,
with STUDIO_SERVER_SRC naming workspaces/runtime/apps/server/src:

    LAYR_E2E_ROOT=/mnt/btrfs/store STUDIO_SERVER_SRC=... python3 test_mac_sync_e2e.py

A temporary folder stands for the Mac project folder.
"""
from __future__ import annotations

import asyncio
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest
import uuid

sys.path.insert(0, str(Path(__file__).parent))
sys.path.insert(0, os.environ.get("STUDIO_SERVER_SRC", str(Path(__file__).resolve().parents[1] / "server/src")))
import test_layr_e2e as base


class GuestClient:
    """The broker calls a round makes, served by the guest modules in this process."""

    def __init__(self, loop, projects, sync, state_dir):
        self.loop, self.projects, self.sync, self.state_dir = loop, projects, sync, state_dir

    def call(self, method, params, **_options):
        async def run():
            if method == "project.ensure":
                return await self.projects.ensure(params["projectId"])
            return await self.sync.dispatch(uuid.uuid4().hex, method, params, None)
        return asyncio.run_coroutine_threadsafe(run(), self.loop).result(600)


@unittest.skipUnless(base.ROOT and os.geteuid() == 0 and sys.platform == "linux", "needs LAYR_E2E_ROOT, root and Linux")
class MacSyncRounds(base.LayrEndToEnd):
    test_lead_worker_merge_and_host_slot = None

    async def start(self):
        """A Mac folder with Git, imported as the project; returns the round helpers."""
        from codex_vm_mac_sync import ProjectSync
        from mac_sync import MacSync
        from upload import extract_tree

        mac = Path(tempfile.mkdtemp(prefix="mac-folder-"))
        self.addCleanup(shutil.rmtree, mac, True)
        subprocess.run(["git", "init", "-q", str(mac)], check=True)
        (mac / ".gitignore").write_text("build/\n.env\n")
        (mac / "app.txt").write_text("one\ntwo\nthree\n")
        (mac / "README.md").write_text("readme\n")
        shutil.copytree(mac, self.upload, dirs_exist_ok=True)
        shutil.rmtree(self.upload / ".git")
        imported = await self.projects.dispatch(uuid.uuid4().hex, "project.import",
                                                {"projectId": self.project, "source": str(self.upload)})
        sync = MacSync(self.state, self.root, self.projects)
        owner = self.projects.owner(self.project)["owner"]
        main = self.projects.folder(self.project) / "lines/main"

        class Sync(ProjectSync):
            def send_archive(self, archive, begin, operation):
                target = Path(begin["root"])
                target.mkdir(parents=True)
                extract_tree(archive, target)

        client = GuestClient(asyncio.get_running_loop(), self.projects, sync, self.state.parent / ("host-" + uuid.uuid4().hex[:6]))
        project = Sync(client, self.project, mac, import_state=imported["stateId"])

        async def round_():
            return await asyncio.to_thread(project.round)

        return mac, sync, owner, main, project, round_

    async def test_both_directions_conflicts_and_quiet_rounds(self):
        mac, sync, owner, main, project, round_ = await self.start()
        # Agents change the first line and add a file; the Mac changes the last line,
        # adds an ignored .env, a build folder and a dependency folder, and deletes README.
        (main / "app.txt").write_text("ONE\ntwo\nthree\n")
        (main / "agent.txt").write_text("from an agent\n")
        await sync.layr(["add", "-A"], cwd=main, owner=owner)
        await sync.layr(["commit", "-qm", "agent work"], cwd=main, owner=owner)
        (mac / "app.txt").write_text("one\ntwo\nTHREE\n")
        (mac / ".env").write_text("SECRET=local\n")
        (mac / "build").mkdir()
        (mac / "build/out.bin").write_bytes(b"\0" * 10)
        (mac / "node_modules/x").mkdir(parents=True)
        (mac / "node_modules/x/index.js").write_text("x\n")
        (mac / "README.md").unlink()

        status = await round_()
        self.assertEqual(status["state"], "synced", status)
        self.assertEqual((main / "app.txt").read_text(), "ONE\ntwo\nTHREE\n")
        self.assertEqual((main / ".env").read_text(), "SECRET=local\n")
        self.assertFalse((main / "README.md").exists())
        self.assertFalse((main / "build").exists())
        self.assertFalse((main / "node_modules").exists())
        self.assertEqual((mac / "app.txt").read_text(), "ONE\ntwo\nTHREE\n")
        self.assertEqual((mac / "agent.txt").read_text(), "from an agent\n")

        # Git lives on the Mac: agents may not push a synced project from the VM.
        rules = await self.projects.layr(["--project", self.project, "access"])
        self.assertRegex(rules, r"@studio-agents\tdeny [^\n]*push")
        head = await sync.dispatch("h1", "sync.mac.head", {"projectId": self.project}, None)
        self.assertEqual(head["mainStateId"], status["mainStateId"])

        # Nothing changed: a quiet round sends and writes nothing.
        quiet = await round_()
        self.assertEqual((quiet["state"], quiet["sent"], quiet["received"]), ("synced", 0, 0))

        # Both sides change the same line: a conflict, the Mac file stays as the user left it.
        (main / "app.txt").write_text("ONE\ntwo\nagent\n")
        await sync.layr(["commit", "-qam", "agent again"], cwd=main, owner=owner)
        (mac / "app.txt").write_text("ONE\ntwo\nmac\n")
        conflict = await round_()
        self.assertEqual(conflict["state"], "conflict", conflict)
        self.assertEqual(conflict["conflicts"], ["app.txt"])
        self.assertEqual((mac / "app.txt").read_text(), "ONE\ntwo\nmac\n")
        self.assertEqual((main / "app.txt").read_text(), "ONE\ntwo\nagent\n")
        # The held conflict is not sent again while the Mac file stays the same.
        again = await round_()
        self.assertEqual((again["state"], again["sent"]), ("conflict", 0))

        # Other files keep syncing during the conflict.
        (main / "agent.txt").write_text("from an agent, edited\n")
        await sync.layr(["commit", "-qam", "agent edit"], cwd=main, owner=owner)
        (mac / "notes.md").write_text("mac notes\n")
        moving = await round_()
        self.assertEqual((mac / "agent.txt").read_text(), "from an agent, edited\n")
        self.assertEqual((main / "notes.md").read_text(), "mac notes\n")
        self.assertEqual(moving["conflicts"], ["app.txt"])

        # Ask the agent: the Mac version waits in the mac-conflict line, and the Mac takes
        # the file the lead commits to main.
        handed = await asyncio.to_thread(project.resolve, "app.txt", "agent")
        line = self.projects.folder(self.project) / "lines/mac-conflict"
        self.assertEqual(handed["line"], "mac-conflict")
        self.assertEqual((line / "app.txt").read_text(), "ONE\ntwo\nmac\n")
        self.assertEqual((main / "app.txt").read_text(), "ONE\ntwo\nagent\n")
        waiting = await round_()
        self.assertEqual((waiting["state"], waiting["withAgent"], waiting["sent"]), ("waiting", ["app.txt"], 0))
        (main / "app.txt").write_text("ONE\ntwo\nagent and mac\n")
        await sync.layr(["commit", "-qam", "combine the Mac version"], cwd=main, owner=owner)
        combined = await round_()
        self.assertEqual((mac / "app.txt").read_text(), "ONE\ntwo\nagent and mac\n")
        self.assertEqual((combined["state"], combined["withAgent"], combined["conflicts"]), ("synced", [], []))

        # Keep the Mac version, and keep the VM version.
        (main / "agent.txt").write_text("agent v3\n")
        (main / "notes.md").write_text("vm notes\n")
        await sync.layr(["commit", "-qam", "agent v3"], cwd=main, owner=owner)
        (mac / "agent.txt").write_text("mac v3\n")
        (mac / "notes.md").write_text("mac notes 2\n")
        both = await round_()
        self.assertEqual(both["conflicts"], ["agent.txt", "notes.md"])
        await asyncio.to_thread(project.resolve, "agent.txt", "mac")
        await asyncio.to_thread(project.round, set())
        self.assertEqual((main / "agent.txt").read_text(), "mac v3\n")
        await asyncio.to_thread(project.resolve, "notes.md", "vm")
        self.assertEqual((mac / "notes.md").read_text(), "vm notes\n")
        settled = await round_()
        self.assertEqual((settled["state"], settled["conflicts"], settled["sent"]), ("synced", [], 0))


    async def test_links_modes_type_changes_and_a_moving_main(self):
        mac, sync, owner, main, project, round_ = await self.start()
        await round_()
        # Links sync as links in both directions, also to targets outside the folder.
        (mac / "to-app").symlink_to("app.txt")
        (mac / "to-etc").symlink_to("/etc")
        (main / "to-readme").symlink_to("README.md")
        await sync.layr(["add", "-A"], cwd=main, owner=owner)
        await sync.layr(["commit", "-qm", "agent link"], cwd=main, owner=owner)
        linked = await round_()
        self.assertEqual(linked["state"], "synced", linked)
        self.assertEqual(os.readlink(main / "to-app"), "app.txt")
        self.assertEqual(os.readlink(main / "to-etc"), "/etc")
        self.assertEqual(os.readlink(mac / "to-readme"), "README.md")
        # The executable bit syncs in both directions, and a Mac edit keeps main's bit.
        (mac / "README.md").chmod(0o755)
        (main / "app.txt").chmod(0o755)
        await sync.layr(["commit", "-qam", "agent chmod"], cwd=main, owner=owner)
        await round_()
        self.assertTrue(os.stat(main / "README.md").st_mode & 0o100)
        self.assertTrue(os.stat(mac / "app.txt").st_mode & 0o100)
        (mac / "app.txt").write_text("one\ntwo\nthree\nfour\n")
        await round_()
        self.assertTrue(os.stat(main / "app.txt").st_mode & 0o100)
        # A Mac file where main has a folder is a conflict; other paths still sync.
        (main / "data").mkdir()
        (main / "data/x.txt").write_text("agent data\n")
        await sync.layr(["add", "-A"], cwd=main, owner=owner)
        await sync.layr(["commit", "-qm", "agent data"], cwd=main, owner=owner)
        (mac / "data").write_text("mac file\n")
        (mac / "other.txt").write_text("other\n")
        typed = await round_()
        self.assertEqual(typed["conflicts"], ["data"])
        self.assertEqual((main / "data/x.txt").read_text(), "agent data\n")
        self.assertEqual((main / "other.txt").read_text(), "other\n")
        self.assertEqual((mac / "data").read_text(), "mac file\n")
        # main moves while the round folds: the fast-forward is refused, main gets no
        # conflict markers, and the next round folds the Mac edit onto the new main.
        original = sync.fold

        async def fold_while_main_moves(*args, **kwargs):
            folded = await original(*args, **kwargs)
            (main / "app.txt").write_text("ONE\ntwo\nthree\nfour\n")
            await sync.layr(["commit", "-qam", "lead edit during the round"], cwd=main, owner=owner)
            return folded
        sync.fold = fold_while_main_moves
        (mac / "app.txt").write_text("one\ntwo\nthree\nFOUR\n")
        refused = await round_()
        sync.fold = original
        self.assertEqual(refused["rescan"], ["app.txt"])
        self.assertEqual((main / "app.txt").read_text(), "ONE\ntwo\nthree\nfour\n")
        self.assertEqual((mac / "app.txt").read_text(), "one\ntwo\nthree\nFOUR\n")
        again = await asyncio.to_thread(project.round, set())
        self.assertEqual(again["rescan"], [])
        self.assertEqual((main / "app.txt").read_text(), "ONE\ntwo\nthree\nFOUR\n")
        self.assertEqual((mac / "app.txt").read_text(), "ONE\ntwo\nthree\nFOUR\n")

    async def test_a_mac_worktree_becomes_a_line(self):
        import codex_layr_migration as migration
        from upload import extract_tree
        mac, sync, owner, main, project, round_ = await self.start()
        lines = self.projects.folder(self.project) / "lines"

        async def move(worktree, name, subpath, operation):
            archive = Path(tempfile.mkdtemp()) / "worktree.tar.gz"
            migration.worktree_archive(worktree, archive)
            upload = Path("/var/lib/codex-studio/projects") / ("mac-wt-" + operation[4:28])
            if not upload.exists():
                upload.mkdir(parents=True)
                extract_tree(archive, upload)
            return await sync.dispatch(uuid.uuid4().hex, "sync.mac.worktree", {
                "projectId": self.project, "operationId": operation, "line": name, "subpath": subpath,
                "upload": str(upload), "message": "Worktree " + name}, None)

        # A worktree of the project repository: changed, new, ignored and deleted files.
        worktree = Path(tempfile.mkdtemp(prefix="mac-worktree-")) / "topic"
        subprocess.run(["git", "-C", str(mac), "add", "-A"], check=True)
        subprocess.run(["git", "-C", str(mac), "-c", "user.email=a@b", "-c", "user.name=a", "commit", "-qm", "base"],
                       check=True)
        subprocess.run(["git", "-C", str(mac), "worktree", "add", "-q", "-b", "topic", str(worktree)], check=True)
        (worktree / "app.txt").write_text("worktree version\n")
        (worktree / "new.txt").write_text("new in the worktree\n")
        (worktree / ".env").write_text("SECRET=worktree\n")
        (worktree / "README.md").unlink()
        (worktree / "node_modules/x").mkdir(parents=True)
        (worktree / "node_modules/x/index.js").write_text("not sent\n")
        result = await move(worktree, "wt-topic", ".", "mac-" + "1" * 32)
        line = lines / "wt-topic"
        self.assertEqual((line / "app.txt").read_text(), "worktree version\n")
        self.assertEqual((line / ".env").read_text(), "SECRET=worktree\n")
        self.assertFalse((line / "README.md").exists())
        self.assertFalse((line / "node_modules").exists())
        self.assertEqual((main / "app.txt").read_text(), "one\ntwo\nthree\n")
        self.assertTrue((main / "README.md").exists())
        shown = await sync.layr(["show", "--stat", "HEAD"], cwd=line, owner=owner)
        self.assertIn(".env", shown)
        # A repeated request after a lost reply returns the receipt.
        again = await move(worktree, "wt-topic", ".", "mac-" + "1" * 32)
        self.assertEqual(again["stateId"], result["stateId"])

        # A worktree of a nested repository replaces only that folder.
        (main / "sub").mkdir()
        (main / "sub/lib.txt").write_text("main lib\n")
        (main / "sub/old.txt").write_text("gone in the worktree\n")
        await sync.layr(["add", "-A"], cwd=main, owner=owner)
        await sync.layr(["commit", "-qm", "sub"], cwd=main, owner=owner)
        nested = Path(tempfile.mkdtemp(prefix="mac-nested-"))
        subprocess.run(["git", "init", "-q", str(nested)], check=True)
        (nested / "lib.txt").write_text("worktree lib\n")
        await move(nested, "wt-sub-fix", "sub", "mac-" + "2" * 32)
        fixed = lines / "wt-sub-fix"
        self.assertEqual((fixed / "sub/lib.txt").read_text(), "worktree lib\n")
        self.assertFalse((fixed / "sub/old.txt").exists())
        self.assertEqual((fixed / "app.txt").read_text(), "one\ntwo\nthree\n")
        self.assertTrue((fixed / "README.md").exists())
        with self.assertRaises(Exception):
            await move(nested, "wt-sub-fix", "sub", "mac-" + "3" * 32)

