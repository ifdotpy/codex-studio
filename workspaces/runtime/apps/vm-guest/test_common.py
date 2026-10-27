"""Durable command results at the supervisor exit boundary."""
import asyncio
import base64
from contextlib import closing
import json
from pathlib import Path
import sqlite3
import tempfile
import unittest
from unittest.mock import patch

import common


class SupervisedCompletionTests(unittest.IsolatedAsyncioTestCase):
    async def test_exit_publishes_its_durable_result_after_the_initial_status_read(self):
        for initial_state in (None, "running"):
            with self.subTest(initial_state=initial_state), tempfile.TemporaryDirectory() as root:
                directory = Path(root) / "command"
                common.private_dir(directory)
                if initial_state:
                    common.atomic_json(directory / "status.json", {"state": initial_state})
                calls = []

                class Process:
                    def poll(self):
                        calls.append("poll")
                        with closing(sqlite3.connect(directory / "journal.sqlite3")) as database, database:
                            database.execute("CREATE TABLE events (seq INTEGER PRIMARY KEY, event TEXT, data TEXT)")
                            for seq, stream, data in ((1, "stdout", b"first"), (2, "stderr", b"error"),
                                                      (3, "stdout", b"second")):
                                database.execute("INSERT INTO events VALUES (?, 'output', ?)",
                                                 (seq, json.dumps({"stream": stream,
                                                                   "data": base64.b64encode(data).decode()})))
                        common.atomic_json(directory / "status.json",
                                           {"state": "exited", "exitCode": 17, "reason": "exited"})
                        return 0

                    def wait(self):
                        return 0

                with patch("subprocess.Popen", return_value=Process()) as start:
                    result = await common.supervised(directory, {}, {"requestId": "original"}, 310)
                    await asyncio.sleep(0)
                    self.assertEqual(start.call_count, 1)
                self.assertEqual(calls, ["poll"])
                self.assertEqual(result, {"stdout": base64.b64encode(b"firstsecond").decode(),
                                          "stderr": base64.b64encode(b"error").decode(),
                                          "exitCode": 17, "reason": "exited"})
                self.assertEqual(json.loads((directory / "association.json").read_text()),
                                 {"requestId": "original"})

    async def test_exit_without_a_final_status_stays_unknown_and_cannot_start_again(self):
        for initial_state in (None, "running"):
            with self.subTest(initial_state=initial_state), tempfile.TemporaryDirectory() as root:
                directory = Path(root) / "command"
                common.private_dir(directory)
                if initial_state:
                    common.atomic_json(directory / "status.json", {"state": initial_state})

                class Process:
                    def poll(self):
                        return 0

                    def wait(self):
                        return 0

                with patch("subprocess.Popen", return_value=Process()) as start:
                    with self.assertRaises(common.GuestError) as raised:
                        await common.supervised(directory, {}, {"requestId": "original"}, 310)
                    self.assertEqual(raised.exception.code, "outcome_unknown")
                    with self.assertRaises(common.GuestError) as repeated:
                        await common.supervised(directory, {}, {"requestId": "original"}, 310)
                    self.assertEqual(repeated.exception.code, "invalid_params")
                    await asyncio.sleep(0)
                    self.assertEqual(start.call_count, 1)


if __name__ == "__main__":
    unittest.main()
