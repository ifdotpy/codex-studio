"""Opt-in end-to-end test of the guest layr handlers with a real layr daemon.

Run as root on Linux with btrfs, layr at /usr/local/bin/layr and `layr daemon` serving
/run/layr/layr.sock for the store at LAYR_E2E_ROOT:

    LAYR_E2E_ROOT=/mnt/btrfs/store python3 test_layr_e2e.py

It goes through a project import, a lead and a worker, a worker commit and turn save, the
lead's reviewed merge, the project rules, a host_exec slot there and back, and a line removal.
"""
from __future__ import annotations

import asyncio
import base64
import os
from pathlib import Path
import shutil
import sys
import unittest
import uuid

sys.path.insert(0, str(Path(__file__).parent))

ROOT = os.environ.get("LAYR_E2E_ROOT")


class Share:
    async def export(self, project):
        return None

    async def status(self):
        return {"state": "test"}


@unittest.skipUnless(ROOT and os.geteuid() == 0 and sys.platform == "linux", "needs LAYR_E2E_ROOT, root and Linux")
class LayrEndToEnd(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        # The guest units run with UMask=0077; folders shared with agent users must not depend on umask.
        self.umask = os.umask(0o077)
        self.addCleanup(os.umask, self.umask)
        import layr_agents
        from host_exec_slot import HostSlotHandlers
        from layr_projects import Projects
        self.root = Path(ROOT)
        self.state = self.root.parent / ("e2e-state-" + uuid.uuid4().hex[:8])
        self.state.mkdir(mode=0o700)
        self.agents = layr_agents.AgentHandlers(self.state, self.root)
        self.projects = Projects(self.state, self.root, Share(), layr_agents)
        self.slots = HostSlotHandlers(self.state, self.root, self.agents)
        self.project = "e2e" + uuid.uuid4().hex[:8]
        upload = Path("/var/lib/codex-studio/projects") / self.project
        upload.mkdir(parents=True)
        (upload / "app.txt").write_text("one\ntwo\nthree\n")
        (upload / ".gitignore").write_text("target/\n")
        self.upload = upload

    async def asyncTearDown(self):
        shutil.rmtree(self.upload, ignore_errors=True)

    async def call(self, handler, method, params):
        return await handler.dispatch(uuid.uuid4().hex, method, params, None)

    async def test_lead_worker_merge_and_host_slot(self):
        imported = await self.projects.dispatch(uuid.uuid4().hex, "project.import",
                                                {"projectId": self.project, "source": str(self.upload)})
        self.assertTrue(imported["imported"])
        rules = await self.projects.layr(["--project", self.project, "protect"])
        self.assertIn("direct true", rules)
        denied = await self.projects.layr(["--project", self.project, "access"])
        self.assertIn("@studio-agents\tdeny access sync remote records backup", denied)

        lead_id, worker_id = str(uuid.uuid4()), str(uuid.uuid4())
        lead = await self.call(self.agents, "line.bind", {"projectId": self.project, "agentId": lead_id, "line": "main"})
        worker = await self.call(self.agents, "line.branch", {"projectId": self.project, "agentId": worker_id,
                                                              "parentAgentId": lead_id})
        self.assertNotEqual(worker["owner"], lead["owner"])

        # The worker changes its own line and saves its turn.
        await self.agents.command(worker["cwd"], ["sh", "-c", "echo four >> app.txt"], user=worker)
        await self.agents.layr(worker, "commit", "-qam", "worker change")
        saved = await self.call(self.agents, "line.save", {"agentId": worker_id, "turnId": "turn-1"})
        self.assertTrue(saved["stateId"])
        head = (await self.call(self.agents, "line.status", {"agentId": worker_id}))["stateId"]

        # The worker cannot write main, nor change roles.
        with self.assertRaises(Exception):
            await self.agents.command(worker["cwd"], ["env", "LAYR_PROJECT=" + self.project, "LAYR_LINE=main",
                                                      "layr", "commit", "--allow-empty", "-qm", "intrude"], user=worker)
        with self.assertRaises(Exception):
            await self.agents.command(worker["cwd"], ["layr", "access", "grant", worker["owner"], "admin"], user=worker)

        # The lead merges exactly the reviewed state, and may also commit in main.
        merged = await self.call(self.agents, "line.merge", {"agentId": lead_id, "sourceAgentId": worker_id,
                                                             "expectedStateId": head})
        self.assertEqual(merged["reviewedStateId"], head)
        self.assertEqual((Path(lead["path"]) / "app.txt").read_text(), "one\ntwo\nthree\nfour\n")
        await self.agents.command(lead["path"], ["sh", "-c", "echo lead > lead.txt"], user=lead)
        await self.agents.layr(lead, "add", "lead.txt")
        await self.agents.layr(lead, "commit", "-qm", "lead change")

        # A host_exec slot: the line goes out, a Mac edit comes back with a three-way merge.
        slot = {"agentId": worker_id, "slotId": "1", "generation": "a" * 32, "operationId": "op-1"}
        prepared = await self.slots.dispatch("prep", "host.slot.prepare", slot)
        source = Path(prepared["path"])
        self.assertEqual((source / "app.txt").read_text(), "one\ntwo\nthree\nfour\n")
        self.assertEqual(prepared["nameConflicts"], [])
        entries = (await self.slots.dispatch("manifest", "host.slot.manifest", slot))["entries"]
        self.assertIn("app.txt", entries)
        self.assertNotIn(".layr-export.json", entries)
        await self.agents.command(str(source), ["sh", "-c", "echo mac > mac.txt; sed -i s/one/ONE/ app.txt"], user=worker)
        report = await self.slots.dispatch("collect", "host.slot.collect",
                                           {**slot, "paths": ["mac.txt", "app.txt"], "final": True})
        self.assertEqual(report["conflicts"], [])
        line = Path(worker["path"])
        self.assertEqual((line / "mac.txt").read_text(), "mac\n")
        self.assertEqual((line / "app.txt").read_text(), "ONE\ntwo\nthree\nfour\n")

        removed = await self.call(self.agents, "line.remove", {"agentId": worker_id})
        self.assertEqual(removed["state"], "removed")
        self.assertFalse(line.exists())



@unittest.skipUnless(ROOT and os.geteuid() == 0 and sys.platform == "linux", "needs LAYR_E2E_ROOT, root and Linux")
class MacSyncEndToEnd(LayrEndToEnd):
    """mac_sync.py against a real layr daemon (docs/vm-mac-sync.md)."""

    test_lead_worker_merge_and_host_slot = None

    async def make_upload(self, files):
        root = Path("/var/lib/codex-studio/projects") / ("sync-" + uuid.uuid4().hex[:12])
        root.mkdir(parents=True)
        self.addCleanup(shutil.rmtree, root, True)
        for rel, data in files.items():
            (root / rel).parent.mkdir(parents=True, exist_ok=True)
            (root / rel).write_text(data)
        return str(root)

    async def test_mac_changes_merge_into_main_and_main_changes_come_back(self):
        from mac_sync import MacSync
        imported = await self.projects.dispatch(uuid.uuid4().hex, "project.import",
                                                {"projectId": self.project, "source": str(self.upload)})
        start = imported["stateId"]
        sync = MacSync(self.state, self.root, self.projects)
        owner = self.projects.owner(self.project)["owner"]
        main = self.projects.folder(self.project) / "lines/main"

        # An agent changes the first line of app.txt in main.
        (main / "app.txt").write_text("ONE\ntwo\nthree\n")
        await sync.layr(["commit", "-qam", "agent change"], cwd=main, owner=owner)

        # The Mac changes the last line of app.txt and adds a file, both based on the import.
        upload = await self.make_upload({"app.txt": "one\ntwo\nTHREE\n", "notes.md": "mac notes\n"})
        first = await sync.apply("r1", {"projectId": self.project, "operationId": "round-1", "upload": upload,
                                        "since": [start], "entries": [
                                            {"path": "app.txt", "kind": "file", "base": start},
                                            {"path": "notes.md", "kind": "file", "base": None}]})
        self.assertEqual(first["merge"], "merged")
        self.assertEqual(first["conflicts"], [])
        self.assertEqual((main / "app.txt").read_text(), "ONE\ntwo\nTHREE\n")
        self.assertEqual((main / "notes.md").read_text(), "mac notes\n")
        outbound = {row["path"]: row for row in first["outbound"][start]}
        self.assertEqual(set(outbound), {"app.txt", "notes.md"})
        read = await sync.read("r2", {"projectId": self.project, "stateId": first["mainStateId"], "path": "app.txt"})
        self.assertEqual(base64.b64decode(read["data"]).decode(), "ONE\ntwo\nTHREE\n")
        self.assertTrue(read["eof"])
        # A repeated operation returns its receipt and applies nothing twice.
        again = await sync.apply("r3", {"projectId": self.project, "operationId": "round-1", "upload": upload,
                                        "since": [start], "entries": []})
        self.assertEqual(again, first)

        # The same line changed on both sides: a conflict, no markers anywhere, main unchanged.
        agreed = first["mainStateId"]
        (main / "app.txt").write_text("ONE\ntwo\nagent three\n")
        await sync.layr(["commit", "-qam", "agent again"], cwd=main, owner=owner)
        upload = await self.make_upload({"app.txt": "one\ntwo\nmac three\n"})
        upload_root = Path(upload)
        (upload_root / "app.txt").write_text("ONE\ntwo\nmac three\n")
        second = await sync.apply("r4", {"projectId": self.project, "operationId": "round-2", "upload": upload,
                                         "since": [agreed], "entries": [
                                             {"path": "app.txt", "kind": "file", "base": agreed}]})
        self.assertEqual(second["conflicts"], ["app.txt"])
        self.assertEqual((main / "app.txt").read_text(), "ONE\ntwo\nagent three\n")
        line = self.projects.folder(self.project) / "lines/mac"
        self.assertNotIn("<<<<<<<", (line / "app.txt").read_text())

        # Uncommitted lead work in main makes a round wait, never a conflict.
        upload = await self.make_upload({"other.txt": "x\n"})
        (main / "scratch.txt").write_text("lead work in progress\n")
        third = await sync.apply("r5", {"projectId": self.project, "operationId": "round-3", "upload": upload,
                                        "since": [second["mainStateId"]], "entries": [
                                            {"path": "other.txt", "kind": "file", "base": None}]})
        self.assertIn(third["merge"], {"merged", "refused"})
        self.assertEqual(third["mergeConflicts"], [])

        page = await sync.hashes("r6", {"projectId": self.project, "stateId": start})
        self.assertIn("app.txt", page["items"])
        self.assertNotIn(".git", {path.split("/")[0] for path in page["items"]})


if __name__ == "__main__":
    unittest.main()
