"""Guest contracts for import isolation and durable broker receipts."""
from __future__ import annotations

import asyncio
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import AsyncMock, patch

sys.path.insert(0, str(Path(__file__).parent))
from layr_admin import Broker
from layr_projects import Projects, copy_source
from layr_share import Share, project_id


class BrokerTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.broker = Broker(self.root / "state", self.root / "store", modules=())

    async def asyncTearDown(self):
        self.broker.close()
        self.temp.cleanup()

    async def test_same_identity_applies_once_and_changed_payload_is_rejected(self):
        self.broker.dispatch = AsyncMock(return_value={"stateId": "exact-state"})
        request = {"id": "import-once", "method": "project.import", "params": {"projectId": "one"}}
        first = await self.broker.request(request, None)
        replay = await self.broker.request(request, None)
        self.assertEqual(first, replay)
        self.broker.dispatch.assert_awaited_once()
        changed = await self.broker.request({**request, "params": {"projectId": "two"}}, None)
        self.assertEqual(changed["error"]["code"], "id_conflict")

    async def test_unfinished_receipt_survives_broker_restart_without_retry(self):
        from common import digest, receipt
        params = {"projectId": "one"}
        receipt(self.broker.db, "crashed", digest("project.import", params))
        self.broker.close()
        self.broker = Broker(self.root / "state", self.root / "store", modules=())
        self.broker.dispatch = AsyncMock()
        result = await self.broker.request({"id": "crashed", "method": "project.import", "params": params}, None)
        self.assertEqual(result["error"]["code"], "outcome_unknown")
        self.broker.dispatch.assert_not_awaited()

    async def test_concurrent_same_identity_joins_one_operation(self):
        applied = []
        gate = asyncio.Event()

        async def dispatch(*args):
            applied.append(args)
            await gate.wait()
            return {"ok": True}

        self.broker.dispatch = dispatch
        request = {"id": "concurrent", "method": "project.import", "params": {}}
        first = asyncio.create_task(self.broker.request(request, None))
        await asyncio.sleep(0)
        second = asyncio.create_task(self.broker.request(request, None))
        await asyncio.sleep(0)
        gate.set()
        self.assertEqual(await first, await second)
        self.assertEqual(len(applied), 1)

    async def test_unknown_admin_operation_cannot_execute(self):
        self.broker.dispatch = AsyncMock()
        response = await self.broker.request({"id": "root-command", "method": "exec", "params": {"argv": ["id"]}}, None)
        self.assertIn("error", response)
        self.broker.dispatch.assert_not_awaited()

    async def test_host_slot_read_keeps_the_handler_error_code(self):
        class SlotError(Exception):
            def object(self):
                return {"code": "lease_lost", "message": "The slot lease changed"}

        self.broker.host_errors = (SlotError,)
        self.broker.methods.add("host.slot.read")
        self.broker.read_methods.add("host.slot.read")
        self.broker.dispatch = AsyncMock(side_effect=SlotError())
        response = await self.broker.request({"id":"read-slot", "method":"host.slot.read", "params":{}}, None)
        self.assertEqual(response["error"]["code"], "lease_lost")


class ImportTests(unittest.IsolatedAsyncioTestCase):
    async def test_existing_project_ignores_mac_source_and_does_not_import(self):
        with tempfile.TemporaryDirectory() as name:
            root = Path(name)
            cwd = root / "store/projects/app/lines/main"
            cwd.mkdir(parents=True)
            share = AsyncMock()
            projects = Projects(root, root / "store", share, None)
            projects.ensure = AsyncMock(return_value={"projectId": "app", "stateId": "vm-main"})
            projects.freeze_source = AsyncMock()
            projects.layr = AsyncMock()
            result = await projects.dispatch("second-import", "project.import", {"projectId": "app", "source": "/missing-mac"})
            self.assertEqual(result["stateId"], "vm-main")
            self.assertFalse(result["imported"])
            projects.freeze_source.assert_not_awaited()
            projects.layr.assert_not_awaited()

    async def test_explicit_import_rejects_stale_main_before_file_changes(self):
        with tempfile.TemporaryDirectory() as name:
            root = Path(name)
            (root / "store/projects/app/lines/main").mkdir(parents=True)
            projects = Projects(root, root / "store", AsyncMock(), None)
            projects.owner = lambda project: {"owner": "studio", "uid": os.getuid()}
            projects.ensure = AsyncMock(return_value={"stateId": "new-state"})
            projects.freeze_source = AsyncMock()
            from common import GuestError
            with patch("layr_projects.Path.is_dir", return_value=True), patch("layr_projects.Path.resolve", lambda path: path):
                with self.assertRaises(GuestError) as error:
                    await projects.dispatch("retry", "project.import", {"projectId": "app",
                        "incremental": True, "expectedStateId": "old-state",
                        "source": "/var/lib/codex-studio/projects/upload"})
            self.assertEqual(error.exception.code, "stale_state")
            projects.freeze_source.assert_not_awaited()

    async def test_import_copy_preserves_links_without_reading_their_targets(self):
        with tempfile.TemporaryDirectory() as name:
            root = Path(name)
            source, target = root / "source", root / "target"
            source.mkdir()
            target.mkdir()
            (source / "file").write_text("source")
            (source / "outside").symlink_to("/etc")
            copy_source(source, target)
            self.assertEqual((target / "file").read_text(), "source")
            self.assertEqual(os.readlink(target / "outside"), "/etc")
            self.assertTrue((target / "outside").is_symlink())

    async def test_project_ids_cannot_escape_export_paths(self):
        from common import GuestError
        for value in ("../escape", "/etc", "a\n[global]", "a/b", "", "a" * 101):
            with self.assertRaises(GuestError):
                project_id(value)

    async def test_share_config_is_authenticated_internal_and_read_only(self):
        with tempfile.TemporaryDirectory() as name:
            root = Path(name)
            share = Share(root, root / "store")
            (root / "export-app.json").write_text(json.dumps({"projectId": "app"}))
            share.network = AsyncMock(return_value={"interface": "enp0s1", "gateway": "192.168.64.1"})
            with patch("layr_share.command", new=AsyncMock()) as command:
                await share.refresh()
            configuration = share.configuration.read_text()
            self.assertIn("map to guest = Never", configuration)
            self.assertIn("hosts allow = 192.168.64.1", configuration)
            self.assertIn("read only = yes", configuration)
            self.assertIn("follow symlinks = no", configuration)
            self.assertNotIn("path = " + str(share.root), configuration)
            self.assertEqual(command.await_args_list[-1].args[0][0], "systemctl")


if __name__ == "__main__":
    unittest.main()
