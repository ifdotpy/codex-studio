#!/usr/bin/env python3
"""Real PTY sessions with isolated files and no model service."""

import importlib.util
import json
import os
import shlex
import signal
import subprocess
import sys
from pathlib import Path
from concurrent.futures import ThreadPoolExecutor
import threading
import time
import unittest
from unittest.mock import patch
import urllib.request
import urllib.error
import uuid

spec = importlib.util.spec_from_file_location(
    "workspace_fixture", Path(__file__).with_name("workspace-contract.py")
)
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)
from codex_terminals import TerminalManager, HISTORY_LIMIT
from codex_canvas import Canvas, make_server


class TerminalsContract(unittest.TestCase):
    def setUp(self):
        self.fixture = f.WorkspaceContract()
        self.fixture.setUp()
        self.root = self.fixture.root
        self.runtime = self.fixture.runtime
        self.agent = self.fixture.lead()
        self.manager = TerminalManager(self.fixture.state)
        self.env = patch.dict("os.environ", {"SHELL": "/bin/sh", "HTTP_PROXY": "http://127.0.0.1:9", "HTTPS_PROXY": "http://127.0.0.1:9", "ALL_PROXY": "http://127.0.0.1:9", "NO_PROXY": "127.0.0.1,localhost"})
        self.env.start()

    def tearDown(self):
        self.manager.close()
        f.eventually(lambda: not self.manager.processes)
        self.env.stop()
        self.fixture.tearDown()

    def create(self, **body):
        return self.manager.create(
            self.runtime,
            {
                "id": str(uuid.uuid4()),
                "agent": self.agent["id"],
                "cols": 80,
                "rows": 24,
                **body,
            },
        )

    def send(self, task, text, **body):
        return self.manager.action(
            "input",
            {"id": task["id"], "text": text, "request_id": str(uuid.uuid4()), **body},
        )

    def test_real_pty_cwd_output_resize_exit_and_no_model(self):
        task = self.create()
        marker = self.fixture.project / "terminal-proof"
        self.send(task, "pwd > terminal-proof; stty size >> terminal-proof\n")
        f.eventually(
            lambda: marker.exists() and len(marker.read_text().splitlines()) == 2
        )
        self.assertEqual(
            marker.read_text().splitlines(),
            [str(self.fixture.project.resolve()), "24 80"],
        )
        self.manager.action("resize", {"id": task["id"], "cols": 120, "rows": 36})
        self.send(
            task, 'stty size > resized-proof; printf "\\nPTY-OUTPUT\\n"; exit 7\n'
        )
        f.eventually(lambda: self.manager.output(task["id"])["status"] == "exited")
        self.assertEqual(
            (self.fixture.project / "resized-proof").read_text().strip(), "36 120"
        )
        output = self.manager.output(task["id"])
        self.assertIn("PTY-OUTPUT", output["text"])
        self.assertEqual(output["exitCode"], 7)
        self.assertIsNone(self.runtime.server)

    def test_create_and_input_retries_never_repeat(self):
        request = {"id": str(uuid.uuid4()), "agent": self.agent["id"]}
        with ThreadPoolExecutor(max_workers=6) as pool:
            tasks = list(
                pool.map(lambda _: self.manager.create(self.runtime, request), range(8))
            )
        self.assertEqual(len({t["id"] for t in tasks}), 1)
        text = "printf x >> once-proof\n"
        request_id = str(uuid.uuid4())
        self.send(tasks[0], text, request_id=request_id)
        self.send(tasks[0], text, request_id=request_id)
        marker = self.fixture.project / "once-proof"
        f.eventually(marker.exists)
        self.assertEqual(marker.read_text(), "x")
        with self.assertRaises(ValueError):
            self.send(tasks[0], "echo other\n", request_id=request_id)

    def test_sessions_independent_and_history_after_restart(self):
        first = self.create()
        second = self.create()
        self.manager.action("rename", {"id": first["id"], "title": "Build logs"})
        self.manager.action("close", {"id": first["id"]})
        f.eventually(lambda: first["id"] not in self.manager.processes)
        self.assertEqual(self.manager.output(second["id"])["status"], "running")
        self.send(second, 'printf "\\nRETAINED\\n"; exit 0\n')
        f.eventually(lambda: self.manager.output(second["id"])["status"] == "exited")
        self.manager.close()
        self.manager = TerminalManager(self.fixture.state)
        self.assertIn("RETAINED", self.manager.output(second["id"])["text"])
        self.assertEqual(len(self.manager.listing()["items"]), 1)

    def test_utf8_absolute_cursor_and_bounded_history(self):
        task = self.create()
        self.send(task, "exit\n")
        f.eventually(lambda: self.manager.output(task["id"])["status"] == "exited")
        initial = self.manager.output(task["id"])["offset"]
        self.manager.append(task["id"], "Ж🙂")
        value = self.manager.output(task["id"], initial)
        self.assertEqual(value["text"], "Ж🙂")
        self.assertEqual(value["offset"], initial + 2)
        self.manager.append(task["id"], "x" * (HISTORY_LIMIT + 10))
        value = self.manager.output(task["id"], 0)
        self.assertTrue(value["truncated"])
        self.assertEqual(len(value["text"]), HISTORY_LIMIT)

    def test_unknown_process_after_restart_is_not_recreated(self):
        with self.manager.db() as db:
            db.execute(
                "INSERT INTO user_terminals VALUES (?,?,?,?)",
                (
                    "old",
                    json.dumps(
                        {
                            "id": "old",
                            "agent": self.agent["id"],
                            "title": "Old",
                            "status": "running",
                            "created": 0,
                        }
                    ),
                    "saved",
                    0,
                ),
            )
        self.manager.close()
        self.manager = TerminalManager(self.fixture.state)
        value = self.manager.output("old")
        self.assertEqual(value["status"], "exited")
        self.assertIn("restarted", value["error"])
        self.assertEqual(self.manager.processes, {})

    def test_uncertain_input_receipt_does_not_resend(self):
        task = self.create()
        request = {"id": task["id"], "text": "ignored", "request_id": "uncertain"}
        with self.manager.db() as db:
            signature, _ = self.manager.receipt(
                db, "uncertain", {"action": "input", **request}
            )
            self.manager.save_receipt(
                db, "uncertain", signature, {"ok": False, "delivery": "uncertain"}
            )
        self.assertFalse(self.manager.action("input", request)["ok"])

    def test_close_and_shutdown_stop_all_owned_job_groups(self):
        script = self.fixture.project / "ignore-hup.py"
        script.write_text(
            "import os,pathlib,signal,sys,time\n"
            "signal.signal(signal.SIGHUP, signal.SIG_IGN)\n"
            "pathlib.Path(sys.argv[1]).write_text(str(os.getpid()))\n"
            "time.sleep(60)\n"
        )
        children = []

        def alive(pid):
            try:
                state = subprocess.check_output(
                    ["/bin/ps", "-p", str(pid), "-o", "stat="], text=True
                ).strip()
                return bool(state) and "Z" not in state
            except subprocess.CalledProcessError:
                return False

        def start_job(task, name, background=False):
            marker = self.fixture.project / name
            command = " ".join(
                map(shlex.quote, [sys.executable, str(script), str(marker)])
            )
            self.send(task, command + (" &\n" if background else "\n"))
            f.eventually(marker.exists)
            pid = int(marker.read_text())
            children.append(pid)
            shell = int(self.manager.processes[task["id"]]["pid_path"].read_text())
            self.assertEqual(os.getsid(pid), shell)
            self.assertNotEqual(os.getpgid(pid), shell)
            return pid

        try:
            first = self.create()
            second = self.create()
            background = start_job(first, "background-pid", background=True)
            foreground = start_job(first, "foreground-pid")
            independent = start_job(second, "independent-pid")
            self.manager.action("close", {"id": first["id"]})
            f.eventually(lambda: not alive(foreground) and not alive(background))
            self.assertTrue(alive(independent), "Another terminal must remain active")
            self.assertEqual(self.manager.output(second["id"])["status"], "running")
            self.manager.close()
            f.eventually(lambda: not alive(independent))
        finally:
            for pid in children:
                if alive(pid):
                    os.kill(pid, signal.SIGKILL)

    def test_shell_exit_cleans_background_job(self):
        task = self.create()
        marker = self.fixture.project / "exit-job-pid"
        script = self.fixture.project / "exit-job.py"
        script.write_text(
            "import os,pathlib,signal,time\n"
            "signal.signal(signal.SIGHUP, signal.SIG_IGN)\n"
            f"pathlib.Path({str(marker)!r}).write_text(str(os.getpid()))\n"
            "time.sleep(60)\n"
        )
        pid = None
        try:
            self.send(
                task, f"{shlex.quote(sys.executable)} {shlex.quote(str(script))} &\n"
            )
            f.eventually(marker.exists)
            pid = int(marker.read_text())
            self.send(task, "exit 9\n")
            f.eventually(lambda: task["id"] not in self.manager.processes)
            self.assertEqual(self.manager.output(task["id"])["exitCode"], 9)
            try:
                state = subprocess.check_output(
                    ["/bin/ps", "-p", str(pid), "-o", "stat="], text=True
                ).strip()
                self.assertIn("Z", state)
            except subprocess.CalledProcessError:
                pass
        finally:
            if pid:
                try:
                    os.kill(pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass

    def test_native_connection_loss_cleans_jobs_and_allows_a_new_terminal(self):
        first = self.create()
        self.send(first, "printf retained-before-loss; sleep 60 &\n")
        f.eventually(lambda: "retained-before-loss" in self.manager.output(first["id"])["text"])
        old_connection = self.manager.connection
        server = self.manager.server
        server.proc.kill()
        server.proc.wait(5)
        f.eventually(lambda: not self.manager.processes)
        output = self.manager.output(first["id"])
        self.assertEqual(output["status"], "exited")
        self.assertIsNone(output["exitCode"])
        self.assertIn("unknown", output["error"])
        second = self.create()
        self.assertIsNot(self.manager.server, server)
        self.manager.disconnected(old_connection)
        self.assertEqual(self.manager.output(second["id"])["status"], "running")
        self.send(second, "exit 4\n")
        f.eventually(lambda: self.manager.output(second["id"])["status"] == "exited")
        self.assertEqual(self.manager.output(second["id"])["exitCode"], 4)

    def test_lost_spawn_ack_does_not_create_another_shell(self):
        from codex_runtime import ResponseTimeout
        body = {"id": "lost-spawn", "agent": self.agent["id"]}
        server = self.manager.connect()
        wait = server.wait
        def lose_ack(submitted, **kwargs):
            result = wait(submitted, **kwargs)
            if submitted[1] == "process/spawn":
                raise ResponseTimeout("process/spawn response timed out; outcome unknown")
            return result
        with patch.object(server, "wait", side_effect=lose_ack):
            first = self.manager.create(self.runtime, body)
            second = self.manager.create(self.runtime, body)
        self.assertEqual(first["id"], second["id"])
        self.assertEqual(len(self.manager.processes), 1)
        self.assertIn("unknown", first["error"])
        f.eventually(lambda: self.manager.output(first["id"])["error"] is None)
        self.send(first, "exit 6\n")
        f.eventually(lambda: self.manager.output(first["id"])["status"] == "exited")
        self.assertEqual(self.manager.output(first["id"])["exitCode"], 6)

    def test_late_native_input_ack_updates_receipt_without_replay(self):
        from codex_runtime import ResponseTimeout
        task = self.create()
        data = {"id": task["id"], "text": "printf x >> late-input\n", "request_id": "late-input"}
        with patch.object(self.manager.server, "wait", side_effect=ResponseTimeout("outcome unknown")):
            with self.assertRaises(ResponseTimeout):
                self.manager.action("input", data)
        marker = self.fixture.project / "late-input"
        f.eventually(marker.exists)
        f.eventually(lambda: self.manager.action("input", data).get("ok"))
        self.assertEqual(marker.read_text(), "x")

    def test_native_spawn_rejection_is_durable_and_does_not_retry(self):
        from codex_native_errors import NativeRpcError
        body = {"id": "rejected-spawn", "agent": self.agent["id"]}
        server = self.manager.connect()
        with patch.object(server, "submit", side_effect=NativeRpcError({"code": -32601, "message": "Unknown method process/spawn"})) as call:
            first = self.manager.create(self.runtime, body)
            second = self.manager.create(self.runtime, body)
        self.assertEqual(first, second)
        self.assertEqual(first["status"], "exited")
        self.assertEqual(call.call_count, 1)
        self.assertEqual(self.manager.processes, {})

    def test_terminal_restores_user_environment_and_unicode(self):
        task = self.create()
        marker = self.fixture.project / "environment-proof"
        script = "import os,json; from pathlib import Path; Path('environment-proof').write_text(json.dumps([os.environ.get('CODEX_HOME'),os.environ.get('TERM'),os.environ.get('COLORTERM')])); print('Терминал🙂')"
        self.send(task, shlex.quote(sys.executable) + " -c " + shlex.quote(script) + "\n")
        f.eventually(marker.exists)
        self.assertEqual(json.loads(marker.read_text()), [os.environ.get("CODEX_HOME"), "xterm-256color", "truecolor"])
        f.eventually(lambda: "Терминал🙂" in self.manager.output(task["id"])["text"])
        self.assertIsNone(self.runtime.server)

    def test_http_origin_token_identity_and_manual_monitor_absent(self):
        canvas = Canvas(self.fixture.state)
        canvas.runtime = self.runtime
        server = make_server(canvas)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        url = f"http://127.0.0.1:{server.server_port}"

        def request(path, body=None, headers=None):
            req = urllib.request.Request(
                url + path,
                data=None if body is None else json.dumps(body).encode(),
                headers=headers or {},
            )
            with urllib.request.urlopen(req, timeout=5) as result:
                return json.load(result)

        try:
            self.assertEqual(request("/api/desktop")["application"], "codex-agents")
            token = request("/api/state")["token"]
            headers = {"Content-Type": "application/json", "X-Canvas-Token": token}
            body = {"id": str(uuid.uuid4()), "agent": self.agent["id"]}
            with self.assertRaises(urllib.error.HTTPError) as denied:
                request("/api/terminals/create", body)
            self.assertEqual(denied.exception.code, 403)
            with self.assertRaises(urllib.error.HTTPError) as denied:
                request(
                    "/api/terminals/create",
                    body,
                    {**headers, "Origin": "https://outside.invalid"},
                )
            self.assertEqual(denied.exception.code, 403)
            task = request("/api/terminals/create", body, headers)
            self.assertEqual(request("/api/terminals")["items"][0]["id"], task["id"])
            request("/api/terminals/close", {"id": task["id"]}, headers)
            with self.assertRaises(urllib.error.HTTPError) as removed:
                request(
                    "/api/monitor",
                    {"agent": self.agent["id"], "command": "must-not-run"},
                    headers,
                )
            self.assertEqual(removed.exception.code, 404)
        finally:
            server.shutdown()
            server.server_close()
            thread.join()


if __name__ == "__main__":
    unittest.main(verbosity=2)
