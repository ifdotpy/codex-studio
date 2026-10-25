"""Bounded guest reads through the Mac sync dispatch path."""
from __future__ import annotations

import base64
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import AsyncMock, MagicMock, patch

sys.path.insert(0, str(Path(__file__).parent))
from common import GuestError
from host_exec_protocol import HostExecError
from mac_sync import MacSync


class MacSyncRead(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self) -> None:
        scratch = tempfile.TemporaryDirectory()
        self.addCleanup(scratch.cleanup)
        self.root = Path(scratch.name)
        self.sync = MacSync(self.root, self.root, None)
        self.sync.lines = AsyncMock(return_value=("owner", self.root, self.root, self.root))
        self.sync.checkout = AsyncMock()

    async def test_clean_uses_status_json_and_ignores_ignored_files(self) -> None:
        status = {"staged": [], "unstaged": [], "untracked": [], "conflicts": [], "ignored": ["target/cache"]}
        self.sync.layr = AsyncMock(return_value=json.dumps(status))
        await self.sync.clean(self.root, "owner", "main")
        self.sync.layr.assert_awaited_once_with(["status", "--json"], cwd=self.root, owner="owner")
        for field in ("staged", "unstaged", "untracked", "conflicts"):
            with self.subTest(field=field):
                self.sync.layr.return_value = json.dumps({**status, field: ["changed"]})
                with self.assertRaises(GuestError):
                    await self.sync.clean(self.root, "owner", "main")

    async def read(self, path: str, offset: int = 0) -> dict:
        return await self.sync.dispatch("read-request", "sync.mac.read", {
            "projectId": "project", "stateId": "abcdef0", "path": path, "offset": offset,
        }, None)

    async def test_regular_file_reads_only_the_requested_chunk_under_lock(self) -> None:
        content = b"aaaabbbbtail"
        path = self.root / "file"
        path.write_bytes(content)
        path.chmod(0o751)
        original_open = Path.open
        reads = []

        def counted_open(target: Path, *args, **kwargs):
            stream = original_open(target, *args, **kwargs)
            wrapped = MagicMock(wraps=stream)
            wrapped.__enter__.return_value = wrapped
            wrapped.__exit__.side_effect = lambda *exc: stream.close()

            def counted_read(size: int = -1) -> bytes:
                self.assertTrue(self.sync.locks["project"].locked())
                data = stream.read(size)
                reads.append((size, len(data)))
                return data

            wrapped.read.side_effect = counted_read
            return wrapped

        for offset, encoded, size, eof in [
            (0, "YWFhYQ==", 4, False),
            (4, "YmJiYg==", 4, False),
            (8, "dGFpbA==", 4, True),
            (12, "", 0, True),
            (16, "", 0, True),
            (1 << 100, "", 0, True),
        ]:
            with self.subTest(offset=offset), patch("mac_sync.CHUNK", 4), patch.object(Path, "open", counted_open):
                reads.clear()
                result = await self.read("file", offset)
                self.assertEqual(reads, [(4, size)])
                self.assertEqual(result, {
                    "kind": "file", "mode": 0o751, "size": len(content), "offset": offset,
                    "data": encoded,
                    "eof": eof,
                })

    async def test_link_returns_target_bytes_without_following_it(self) -> None:
        (self.root / "link").symlink_to("missing-target")
        result = await self.read("link", 3)
        self.assertEqual(result, {
            "kind": "link", "mode": 0, "size": len(b"missing-target"), "offset": 3,
            "data": base64.b64encode(b"sing-target").decode(), "eof": True,
        })

    async def test_absent_directory_and_parent_file_are_absent(self) -> None:
        (self.root / "file").touch()
        (self.root / "folder").mkdir()
        for path in ("missing", "folder", "file/child"):
            with self.subTest(path=path):
                self.assertEqual(await self.read(path), {"kind": "absent"})

    async def test_empty_file_preserves_size_and_eof(self) -> None:
        (self.root / "empty").touch()
        result = await self.read("empty")
        self.assertEqual((result["size"], result["data"], result["eof"]), (0, "", True))

    async def test_special_file_is_rejected_without_opening_it(self) -> None:
        os.mkfifo(self.root / "pipe")
        with patch.object(Path, "open", side_effect=AssertionError("special file opened")):
            with self.assertRaises(GuestError) as error:
                await self.read("pipe")
        self.assertEqual(error.exception.code, "unsupported_file")

    async def test_invalid_offset_and_link_parent_are_rejected(self) -> None:
        (self.root / "parent").symlink_to(self.root, target_is_directory=True)
        with self.assertRaises(HostExecError):
            await self.read("parent/file")
        for offset in (-1, True, "0"):
            with self.subTest(offset=offset), self.assertRaises(GuestError):
                await self.read("file", offset)


if __name__ == "__main__":
    unittest.main()
