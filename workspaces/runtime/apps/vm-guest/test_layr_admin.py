"""Guest contracts for import isolation and durable broker receipts."""
from __future__ import annotations

import asyncio
import json
import os
import socket
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

    async def test_share_config_is_authenticated_loopback_only_and_read_only(self):
        with tempfile.TemporaryDirectory() as name:
            root = Path(name)
            share = Share(root, root / "store")
            (root / "export-app.json").write_text(json.dumps({"projectId": "app"}))
            with patch("layr_share.command", new=AsyncMock()) as command:
                await share.refresh()
            configuration = share.configuration.read_text()
            self.assertIn("map to guest = Never", configuration)
            # The VM network never reaches Samba; the vsock bridge connects on the guest loopback.
            self.assertIn("interfaces = lo\nbind interfaces only = yes", configuration)
            self.assertIn("hosts allow = 127.0.0.1\nhosts deny = ALL", configuration)
            self.assertIn("read only = yes", configuration)
            self.assertIn("follow symlinks = no", configuration)
            self.assertNotIn("path = " + str(share.root), configuration)
            self.assertEqual(command.await_args_list[-1].args[0][0], "systemctl")



class ShareNameTests(unittest.IsolatedAsyncioTestCase):
    async def share(self, root, *projects):
        share = Share(root, root / "store")
        for project in projects:
            (root / ("export-" + project + ".json")).write_text(json.dumps({"projectId": project}))
        (root / "smb-password").write_text("x")
        return share

    async def test_the_share_takes_the_preferred_name_and_stays_unique(self):
        from common import GuestError
        with tempfile.TemporaryDirectory() as name:
            root = Path(name)
            share = await self.share(root, "a" * 64, "b" * 64)
            with patch("layr_share.command", new=AsyncMock()):
                self.assertEqual((await share.name({"projectId": "a" * 64, "name": "my-app"}))["name"], "my-app")
                self.assertEqual((await share.name({"projectId": "b" * 64, "name": "MY-APP"}))["name"], "MY-APP-2")
                # A repeated request keeps the chosen name.
                self.assertEqual((await share.name({"projectId": "b" * 64, "name": "MY-APP"}))["name"], "MY-APP-2")
                self.assertEqual((await share.name({"projectId": "a" * 64, "name": "renamed"}))["name"], "renamed")
            configuration = share.configuration.read_text()
            self.assertIn("[renamed]\npath = " + str(share.export_root / ("a" * 64)), configuration)
            self.assertIn("[MY-APP-2]\npath = " + str(share.export_root / ("b" * 64)), configuration)
            for bad in ("global", "a b", "../x", "x" * 49, "", "[x]", "-x"):
                with self.subTest(bad=bad), self.assertRaises(GuestError):
                    await share.name({"projectId": "a" * 64, "name": bad})
            with self.assertRaises(GuestError):
                await share.name({"projectId": "c" * 64, "name": "other"})


class ShareBridgeTests(unittest.IsolatedAsyncioTestCase):
    """share_bridge.py with TCP standing in for vsock; the relay code is the same."""

    async def echo_target(self):
        async def echo(reader, writer):
            while data := await reader.read(1024):
                writer.write(data)
                await writer.drain()
            writer.close()
        server = await asyncio.start_server(echo, "127.0.0.1", 0)
        self.addAsyncCleanup(server.wait_closed)
        self.addCleanup(server.close)
        return server.sockets[0].getsockname()[:2]

    async def bridge_port(self, bridge):
        server = await asyncio.start_server(bridge.client, "127.0.0.1", 0)
        self.addAsyncCleanup(server.wait_closed)
        self.addCleanup(server.close)
        return server.sockets[0].getsockname()[1]

    async def test_bytes_cross_in_both_directions(self):
        from share_bridge import Bridge
        port = await self.bridge_port(Bridge(await self.echo_target()))
        reader, writer = await asyncio.open_connection("127.0.0.1", port)
        payload = bytes(range(256)) * 1024
        writer.write(payload)
        await writer.drain()
        self.assertEqual(await reader.readexactly(len(payload)), payload)
        writer.close()

    async def test_connections_over_the_limit_are_closed(self):
        from share_bridge import Bridge
        port = await self.bridge_port(Bridge(await self.echo_target(), limit=1))
        first = await asyncio.open_connection("127.0.0.1", port)
        first[1].write(b"held")
        self.assertEqual(await first[0].readexactly(4), b"held")
        second = await asyncio.open_connection("127.0.0.1", port)
        self.assertEqual(await asyncio.wait_for(second[0].read(), 5), b"")
        first[1].close()

    async def test_an_unavailable_samba_closes_the_client(self):
        from share_bridge import Bridge
        with socket.socket() as probe:
            probe.bind(("127.0.0.1", 0))
            closed = probe.getsockname()[:2]
        port = await self.bridge_port(Bridge(closed))
        reader, _writer = await asyncio.open_connection("127.0.0.1", port)
        self.assertEqual(await asyncio.wait_for(reader.read(), 15), b"")


class PolicyTests(unittest.IsolatedAsyncioTestCase):
    async def test_project_rules_apply_once(self):
        with tempfile.TemporaryDirectory() as name:
            root = Path(name)
            projects = Projects(root, root / "store", AsyncMock(), None)
            projects.layr = AsyncMock(return_value="")
            with patch("layr_agents.ensure_agents_group"):
                await projects.apply_policy("app")
                await projects.apply_policy("app")
            self.assertEqual([call.args[0] for call in projects.layr.await_args_list], [
                ["--project", "app", "protect", "main", "--direct"],
                ["--project", "app", "access", "deny", "@studio-agents", "access", "sync", "remote", "records", "backup"]])


class ServiceUmaskTests(unittest.TestCase):
    """The guest units run with UMask=0077; shared folders still get their modes."""

    def test_socket_and_slot_folders_keep_their_modes_under_umask_077(self):
        import stat
        from types import SimpleNamespace
        from unittest.mock import patch
        import host_exec_slot
        import layr_admin
        previous = os.umask(0o077)
        try:
            with tempfile.TemporaryDirectory() as name:
                run = Path(name) / "run"
                with patch.object(layr_admin.os, "chown"), \
                        patch.object(layr_admin.pwd, "getpwnam", return_value=SimpleNamespace(pw_gid=os.getgid())):
                    layr_admin.socket_directory(run)
                self.assertEqual(stat.S_IMODE(run.stat().st_mode), 0o750)
                slots = Path(name) / "host-slots"
                slots.mkdir(mode=0o700)
                host_exec_slot.traversable(slots)
                self.assertEqual(stat.S_IMODE(slots.stat().st_mode), 0o711)
        finally:
            os.umask(previous)

if __name__ == "__main__":
    unittest.main()
