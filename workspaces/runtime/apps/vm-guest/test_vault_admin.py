"""The vault broker methods with a fake `layr-desk` executable: the configuration goes on its
standard input, status.json records each outcome, and a failure names its cause."""
from __future__ import annotations

import base64
import asyncio
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).parent))
from common import GuestError
import vault_admin
from vault_admin import VaultHandlers

SECRET = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"
SIGNING = base64.b64encode(bytes(range(32))).decode()
# The fake writes each call to calls.jsonl; modes.json chooses what it does for each project.
FAKE = r'''#!@PYTHON@
import json, os, sys, time
from pathlib import Path
here = Path(@HERE@)
args = sys.argv[1:]
config = sys.stdin.read()
with (here / "calls.jsonl").open("a") as log:
    log.write(json.dumps({"args": args, "stdin": config, "env": dict(os.environ), "cwd": os.getcwd()}) + "\n")
project = args[args.index("--project") + 1]
state = Path(args[args.index("--state-dir") + 1])
mode = json.loads((here / "modes.json").read_text()).get(project, "push")
record = json.loads((state / "writer.json").read_text()) if (state / "writer.json").exists() else {"seq": 0}
if mode == "push":
    state.mkdir(mode=0o700, parents=True, exist_ok=True)
    record = {"seq": record["seq"] + 1, "attempt": "ab12cd34", "state": "s1", "side": "a", "since_full": 0}
    (state / "writer.json").write_text(json.dumps(record))
    print(json.dumps({"pushed": True, "seq": record["seq"], "state": "s1", "files": 2, "parts": 1, "bytes": 99}))
elif mode == "same":
    print(json.dumps({"pushed": False, "seq": record["seq"], "state": "s1"}))
elif mode == "fail":
    sys.stderr.write("Error: S3 put app/states/0000000002.ab.0000.data.age\n\nCaused by:\n"
                     "    S3 put app/states/0000000002.ab.0000.data.age: HTTP 403 AccessDenied Access Denied\n")
    sys.exit(1)
elif mode == "quiet":
    sys.exit(3)
elif mode == "garbage":
    print("not a summary")
elif mode == "slow":
    time.sleep(30)
'''


class VaultAdminTests(unittest.IsolatedAsyncioTestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory(prefix="vault-admin-")
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name)
        self.fake = self.root / "bin/layr-desk"
        self.fake.parent.mkdir()
        self.fake.write_text(FAKE.replace("@PYTHON@", sys.executable).replace("@HERE@", repr(str(self.root))))
        self.fake.chmod(0o755)
        self.modes({})
        patcher = mock.patch.object(vault_admin, "LAYR_DESK", str(self.fake))
        patcher.start()
        self.addCleanup(patcher.stop)
        self.handlers = VaultHandlers(self.root / "state", self.root / "store")

    def modes(self, modes):
        (self.root / "modes.json").write_text(json.dumps(modes))

    def calls(self):
        path = self.root / "calls.jsonl"
        return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []

    async def configure(self, project="app", **changes):
        (self.root / "store/projects" / project / "lines/main").mkdir(parents=True, exist_ok=True)
        params = {"projectId": project, "recipient": "age1" + "q" * 58, "bucket": "bucket-one",
                  "region": "eu-central-1", "prefix": project, "accessKeyId": "AKIAIOSFODNN7EXAMPLE",
                  "secretAccessKey": SECRET, **changes}
        return await self.handlers.dispatch("c", "vault.configure", params, None)

    async def status(self, project="app"):
        return await self.handlers.dispatch("s", "vault.status", {"projectId": project}, None)

    async def push(self, project="app"):
        return await self.handlers.dispatch("p", "vault.push", {"projectId": project}, None)

    async def test_the_configuration_goes_on_standard_input_and_never_in_the_arguments(self):
        await self.configure(signingKey=SIGNING)
        config = self.root / "state/vault/app/config.json"
        self.assertEqual(config.stat().st_mode & 0o777, 0o600)
        self.assertEqual((await self.push())["seq"], 1)
        [call] = self.calls()
        writer = str(self.root / "state/vault/app/writer")
        self.assertEqual(call["args"], ["push", "--project", "app", "--state-dir", writer,
                                        "--layr", "/usr/local/bin/layr", "--config", "-"])
        self.assertEqual(json.loads(call["stdin"]), {
            "recipient": "age1" + "q" * 58, "bucket": "bucket-one", "region": "eu-central-1", "prefix": "app",
            "accessKeyId": "AKIAIOSFODNN7EXAMPLE", "secretAccessKey": SECRET, "signingKey": SIGNING})
        self.assertNotIn(SECRET, json.dumps(call["args"]) + json.dumps(call["env"]))
        self.assertEqual(call["cwd"], "/")
        self.assertEqual(call["env"]["LAYR_ROOT"], str(self.root / "store"))
        self.assertEqual(call["env"]["LAYR_SOCKET"], "/run/layr/layr.sock")
        status = await self.status()
        self.assertEqual(status["last"]["seq"], 1)
        self.assertNotIn(SECRET, json.dumps(status))
        self.assertNotIn(SIGNING, json.dumps(status))

    async def test_a_new_configuration_waits_for_a_running_push(self):
        await self.configure()
        config = self.root / "state/vault/app/config.json"
        lock = self.handlers.locks.setdefault("app", asyncio.Lock())
        await lock.acquire()  # A push with the old configuration runs.
        change = asyncio.ensure_future(self.configure(signingKey=SIGNING))
        await asyncio.sleep(0.05)
        self.assertFalse(change.done())
        self.assertNotIn("signingKey", json.loads(config.read_text()))
        lock.release()
        await change
        self.assertEqual(json.loads(config.read_text())["signingKey"], SIGNING)

    async def test_the_signing_key_is_optional_and_a_test_endpoint_is_passed(self):
        await self.configure(endpoint="http://127.0.0.1:9000")
        await self.push()
        sent = json.loads(self.calls()[0]["stdin"])
        self.assertNotIn("signingKey", sent)
        self.assertEqual(sent["endpoint"], "http://127.0.0.1:9000")
        for bad in ("", "not base64!", base64.b64encode(bytes(31)).decode(), 42):
            with self.assertRaises(GuestError, msg=repr(bad)):
                await self.configure(signingKey=bad)
        with self.assertRaises(GuestError):
            await self.configure(endpoint="https://evil.example/")

    async def test_status_records_the_attempt_the_push_and_the_cause_of_a_failure(self):
        await self.configure()
        with mock.patch.object(vault_admin.time, "time", return_value=1000):
            await self.push()
        status = await self.status()
        self.assertEqual((status["lastAttempt"], status["lastPush"], status["lastError"]), (1000, 1000, None))
        # Nothing new: an attempt, no push.
        self.modes({"app": "same"})
        with mock.patch.object(vault_admin.time, "time", return_value=2000):
            self.assertFalse((await self.push())["pushed"])
        status = await self.status()
        self.assertEqual((status["lastAttempt"], status["lastPush"], status["lastError"]), (2000, 1000, None))
        # A failure keeps the last push and names its cause on one line.
        self.modes({"app": "fail"})
        with mock.patch.object(vault_admin.time, "time", return_value=3000):
            with self.assertRaises(GuestError) as raised:
                await self.push()
        self.assertEqual(raised.exception.code, "operation_failed")
        status = await self.status()
        self.assertEqual((status["lastAttempt"], status["lastPush"]), (3000, 1000))
        self.assertEqual(status["lastError"],
                         "layr-desk push: S3 put app/states/0000000002.ab.0000.data.age Caused by: "
                         "S3 put app/states/0000000002.ab.0000.data.age: HTTP 403 AccessDenied Access Denied")
        self.assertEqual(str(raised.exception), status["lastError"])
        # The next success clears the error.
        self.modes({})
        await self.push()
        self.assertIsNone((await self.status())["lastError"])

    async def test_a_writer_that_says_nothing_useful_is_a_failure(self):
        await self.configure()
        for mode, cause in (("quiet", "layr-desk push: exit status 3"),
                            ("garbage", "layr-desk push printed no summary")):
            self.modes({"app": mode})
            with self.assertRaises(GuestError):
                await self.push()
            self.assertEqual((await self.status())["lastError"], cause)
            self.assertNotIn("lastPush", await self.status())
        self.modes({"app": "slow"})
        with mock.patch.object(vault_admin, "PUSH_TIMEOUT", 1), self.assertRaises(GuestError):
            await self.push()
        self.assertEqual((await self.status())["lastError"], "layr-desk push ran longer than 1 seconds")
        os.unlink(self.fake)
        with self.assertRaises(GuestError):
            await self.push()
        self.assertEqual((await self.status())["lastError"], "layr-desk is not installed")

    async def test_another_vault_or_key_starts_the_writer_again(self):
        await self.configure()
        record = self.root / "state/vault/app/writer/writer.json"
        record.parent.mkdir(parents=True)
        record.write_text("{}")
        await self.configure(accessKeyId="AKIAIOSFODNN7OTHERX")
        self.assertTrue(record.exists(), "a new writer key keeps the chain")
        await self.configure(accessKeyId="AKIAIOSFODNN7OTHERX", signingKey=SIGNING)
        self.assertFalse(record.exists(), "a new signing key starts again")
        record.parent.mkdir(parents=True)
        record.write_text("{}")
        await self.configure(accessKeyId="AKIAIOSFODNN7OTHERX", signingKey=SIGNING)
        self.assertTrue(record.exists(), "the same configuration keeps the chain")
        await self.configure(recipient="age1" + "p" * 58, signingKey=SIGNING)
        self.assertFalse(record.exists(), "a new vault key starts again")

    async def test_the_periodic_round_pushes_every_project_and_records_the_outcome(self):
        for project in ("one", "two"):
            await self.configure(project)
        self.modes({"one": "fail"})
        await self.handlers.tick()
        one, two = await self.status("one"), await self.status("two")
        self.assertTrue(one["lastError"].startswith("layr-desk push: S3 put "))
        self.assertNotIn("lastPush", one)
        self.assertIsNone(two["lastError"])
        self.assertEqual(two["lastPush"], two["lastAttempt"])
        self.assertEqual([call["args"][2] for call in self.calls()], ["one", "two"])


if __name__ == "__main__":
    unittest.main()
