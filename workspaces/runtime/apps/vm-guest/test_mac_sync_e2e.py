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

    async def test_both_directions_conflicts_and_quiet_rounds(self):
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
