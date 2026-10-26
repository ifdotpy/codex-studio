"""SCRATCH EXPERIMENT (not committed): a slow write between file creation and content."""
import asyncio
import json

from test_service import GuestTests


class Experiment(GuestTests):
    async def lost_exec(self, source):
        socket_path = self.root / "guest.sock"
        server = await asyncio.start_unix_server(self.service.client, path=str(socket_path))
        self.servers.append(server)
        reader, writer = await asyncio.open_unix_connection(str(socket_path))
        params = self.command(source)
        writer.write(json.dumps({"id": "lost-exec", "method": "exec", "params": params}).encode() + b"\n")
        await writer.drain()
        writer.close()
        await writer.wait_closed()
        return params

    async def test_original_with_slow_write(self):
        marker = self.home / "proof"
        # Same command as the original test, with a pause after open() created the file.
        params = await self.lost_exec(
            f"import time; time.sleep(.2); f=open({str(marker)!r},'w'); time.sleep(.5); f.write('one'); f.close()")
        deadline = asyncio.get_running_loop().time() + 5
        while not marker.exists() and asyncio.get_running_loop().time() < deadline:
            await asyncio.sleep(0.025)
        self.assertEqual(marker.read_text(), "one")
        await asyncio.sleep(0.2)
        result = await self.call("exec", params, "lost-exec")
        self.assertEqual(result["exitCode"], 0)
        self.assertEqual(len(self.service.list_providers()), 1)

    async def test_fixed_with_slow_write(self):
        marker = self.home / "proof"
        release = self.home / "release"
        params = await self.lost_exec(
            f"import os,time\nwhile not os.path.exists({str(release)!r}): time.sleep(.01)\n"
            f"f=open({str(marker)!r},'w'); time.sleep(.5); f.write('one'); f.close()")
        try:
            await self.until(lambda: "lost-exec" in self.service.active and self.service.clients == 0,
                             "the guest did not start the exec and see the disconnect")
        finally:
            release.touch()
        await self.until(lambda: "lost-exec" not in self.service.active, "the exec did not end after the disconnect")
        self.assertEqual(marker.read_text(), "one")
        result = await self.call("exec", params, "lost-exec")
        self.assertEqual(result["exitCode"], 0)
        self.assertEqual(len(self.service.list_providers()), 1)
