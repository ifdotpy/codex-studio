#!/usr/bin/env python3
"""An immediately exited native child still has a provable birth identity."""

import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import json
import os
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

import codex_process_supervisor as process_supervisor
from server_test_waits import wait_for_condition


@unittest.skipUnless(sys.platform.startswith("linux"), "Linux zombie identity contract")
class ShortLivedChildContract(unittest.TestCase):
    def test_open_records_output_and_exit_after_child_exits_before_identity_check(self):
        with tempfile.TemporaryDirectory(prefix="supervisor-short-lived-", dir="/var/tmp") as name:
            root = Path(name)
            instance = process_supervisor.Supervisor(root)
            original_start_time = process_supervisor.process_start_time
            original_popen = process_supervisor.subprocess.Popen
            started_processes = []
            observed_start_times = []
            expected_start_times = []

            def check_after_exit(pid, *args, **kwargs):
                os.waitid(os.P_PID, pid, os.WEXITED | os.WNOWAIT)
                value = original_start_time(pid, *args, **kwargs)
                observed_start_times.append(value)
                expected_start_times.append(subprocess.check_output(
                    ["/bin/ps", "-p", str(pid), "-o", "lstart="],
                    text=True, stderr=subprocess.DEVNULL,
                ).strip())
                return value

            command = [sys.executable, "-c",
                       "import sys; print('short-child-stdout'); print('short-child-stderr', file=sys.stderr)"]

            def track_child(*args, **kwargs):
                process = original_popen(*args, **kwargs)
                if args[0] == command:
                    started_processes.append(process)
                return process

            try:
                with patch.object(process_supervisor.subprocess, "Popen", side_effect=track_child), \
                        patch.object(process_supervisor, "process_start_time", side_effect=check_after_exit):
                    opened = instance.handle({
                        "action": "open", "handle": "short-lived-contract", "command": command,
                        "env": {}, "cwd": str(root),
                    })

                self.assertEqual(opened["returnCode"], 0)
                with instance.journal.db() as db:
                    saved_start = db.execute(
                        "SELECT start_time FROM child_identities WHERE handle=?",
                        ("short-lived-contract",),
                    ).fetchone()[0]
                self.assertEqual(saved_start, observed_start_times[0])
                self.assertEqual(saved_start, expected_start_times[0])
                self.assertTrue(process_supervisor._valid_process_start(saved_start))
                cursor = 0
                events = []

                def all_events_delivered():
                    nonlocal cursor
                    while True:
                        result = instance.handle({
                            "action": "next", "handle": "short-lived-contract", "cursor": cursor,
                        })
                        event = result["event"]
                        if event is None:
                            break
                        cursor = event["sequence"]
                        event["payload"] = json.loads(event["payload"])
                        events.append(event)
                    kinds = {event["kind"] for event in events}
                    return events if {"stdout", "stderr", "exit"} <= kinds else None

                delivered = wait_for_condition(all_events_delivered, "short-lived child output and exit events")
                stdout = [event["payload"] for event in delivered if event["kind"] == "stdout"]
                stderr = [event["payload"] for event in delivered if event["kind"] == "stderr"]
                exit_events = [event["payload"] for event in delivered if event["kind"] == "exit"]
                self.assertTrue(any(item.get("supervisorRaw") == "short-child-stdout\n" for item in stdout))
                self.assertTrue(any(item.get("data") == "short-child-stderr\n" for item in stderr))
                self.assertEqual(exit_events, [{"returnCode": 0}])

                child = instance.children["short-lived-contract"]
                child.reader.join(timeout=2)
                child.stderr.join(timeout=2)
                self.assertFalse(child.reader.is_alive())
                self.assertFalse(child.stderr.is_alive())
            finally:
                child = instance.children.get("short-lived-contract")
                if child is not None:
                    child.stopping.set()
                for process in started_processes:
                    if process.poll() is None:
                        process.kill()
                    process.wait()
                    child = instance.children.get("short-lived-contract")
                    if child is not None:
                        child.reader.join(timeout=2)
                        child.stderr.join(timeout=2)
                    for stream in (process.stdin, process.stdout, process.stderr):
                        if stream is not None:
                            stream.close()
                if instance.journal is not None:
                    instance.journal.close()


if __name__ == "__main__":
    unittest.main(verbosity=2)
