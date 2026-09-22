#!/usr/bin/env python3
"""Catalog writers and retired native processes retain the Runtime lease."""
import fcntl
import importlib.util
from pathlib import Path
import tempfile
import threading
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("runtime_fixture", Path(__file__).with_name("runtime-contract.py"))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
import codex_native_tools


class Server(fixture.FakeServer):
    def __init__(self, *args):
        super().__init__(*args)
        self.closes = 0
        self.drains = 0

    def close(self):
        self.closes += 1
        super().close()

    def join_callbacks(self, timeout=10):
        self.drains += 1
        return super().join_callbacks(timeout)


class Shutdown(unittest.TestCase):
    def test_writer_finishes_before_servers_close_and_lease_release(self):
        with tempfile.TemporaryDirectory() as directory:
            runtime = fixture.Runtime(Path(directory), Server)
            current = runtime.connect()
            retired = Server(Path(directory), lambda _: None, lambda _: None, lambda: None)
            runtime._native_tools_retiring = {"old": {"server": retired}, "same": {"server": current}}
            release, waiting = threading.Event(), threading.Event()
            failures = []
            worker = threading.Thread(target=lambda: release.wait(3))
            runtime._native_tools_workers = {worker}
            worker.start()
            original_wait = codex_native_tools.wait_updates

            def wait(rt):
                self.assertFalse(rt.lock._is_owned())
                waiting.set()
                original_wait(rt)

            def close():
                try:
                    runtime.close()
                except BaseException as error:
                    failures.append(error)

            try:
                with patch.object(codex_native_tools, "wait_updates", wait):
                    closer = threading.Thread(target=close)
                    closer.start()
                    self.assertTrue(waiting.wait(2))
                    with open(runtime.lease.name, "a+") as contender:
                        with self.assertRaises(BlockingIOError):
                            fcntl.flock(contender, fcntl.LOCK_EX | fcntl.LOCK_NB)
                        self.assertEqual((current.closes, retired.closes), (0, 0))
                        release.set()
                        closer.join(3)
                        self.assertFalse(closer.is_alive())
                        self.assertEqual(failures, [])
                        fcntl.flock(contender, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    self.assertEqual((current.closes, retired.closes), (1, 1))
                    self.assertEqual((current.drains, retired.drains), (1, 1))
            finally:
                release.set()
                worker.join(3)
                if not runtime.closed:
                    runtime.close()

    def test_unfinished_writer_keeps_the_lease_and_servers(self):
        with tempfile.TemporaryDirectory() as directory:
            runtime = fixture.Runtime(Path(directory), Server)
            server = runtime.connect()
            try:
                with patch.object(codex_native_tools, "wait_updates", side_effect=RuntimeError("Writer still active")):
                    with self.assertRaisesRegex(RuntimeError, "Writer still active"):
                        runtime.close()
                self.assertFalse(runtime.lease.closed)
                self.assertEqual(server.closes, 0)
                with open(runtime.lease.name, "a+") as contender:
                    with self.assertRaises(BlockingIOError):
                        fcntl.flock(contender, fcntl.LOCK_EX | fcntl.LOCK_NB)
            finally:
                runtime.closed = False
                runtime.close()


if __name__ == "__main__":
    unittest.main()
