"""Project secrets: the root broker methods and the start of an agent process with its secrets."""
from __future__ import annotations

import base64
import json
import os
from pathlib import Path
import pwd
import sqlite3
import stat
import sys
import tempfile
import unittest
import uuid
from unittest.mock import AsyncMock, patch

sys.path.insert(0, str(Path(__file__).parent))
import project_env
from common import GuestError
from layr_admin import Broker
from layr_agents import AgentHandlers
from layr_projects import Projects
from project_secrets import MAX_VALUE_BYTES, RESERVED, RESERVED_PREFIXES

VALUE = "zulip-value-7f3a9c"
PROJECT = "a" * 64


def fake_runuser(folder: Path) -> Path:
    """runuser -u OWNER -- COMMAND... without the user change: it keeps the environment and the
    inherited descriptors, as runuser does."""
    path = folder / "runuser"
    path.write_text('#!/bin/sh\n[ "$1" = -u ] && [ "$3" = -- ] || exit 2\nshift 3\nexec "$@"\n')
    path.chmod(0o755)
    return path


class SecretBrokerTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="secrets-", dir="/tmp")
        self.root = Path(self.temp.name)
        (self.root / "store/projects" / PROJECT / "lines/main").mkdir(parents=True)
        self.broker = Broker(self.root / "state", self.root / "store", modules=())
        self.file = self.root / "state/secrets" / (PROJECT + ".json")

    async def asyncTearDown(self):
        self.broker.close()
        self.temp.cleanup()

    async def call(self, method, params, identity=None):
        request = {"id": identity or str(uuid.uuid4()), "method": method, "params": {"projectId": PROJECT, **params}}
        return await self.broker.request(request, None)

    @staticmethod
    def files_with_value(folder):
        return [path for path in sorted(folder.rglob("*")) if path.is_file() and VALUE.encode() in path.read_bytes()]

    def assert_no_value(self, *items):
        for item in items:
            self.assertNotIn(VALUE, json.dumps(item))

    async def test_set_keeps_the_value_in_a_root_only_file_and_replies_with_names(self):
        result = await self.call("secrets.set", {"name": "ZULIP_APEIRO_SHOWROOM_API_KEY", "value": VALUE})
        self.assertEqual(result["result"], {"projectId": PROJECT, "names": ["ZULIP_APEIRO_SHOWROOM_API_KEY"]})
        self.assertEqual(stat.S_IMODE(self.file.stat().st_mode), 0o600)
        self.assertEqual(stat.S_IMODE(self.file.parent.stat().st_mode), 0o700)
        self.assertFalse(self.file.is_relative_to(self.root / "store"))
        self.assertEqual(json.loads(self.file.read_text())["secrets"], {"ZULIP_APEIRO_SHOWROOM_API_KEY": VALUE})
        listed = await self.call("secrets.list", {})
        self.assertEqual(listed["result"]["names"], ["ZULIP_APEIRO_SHOWROOM_API_KEY"])
        self.assert_no_value(result, listed)
        # The receipts keep the reply and a digest: the secret file is the only place of the value.
        self.assertEqual(self.files_with_value(self.root / "state"), [self.file])

    async def test_names_and_values_are_checked_and_errors_never_contain_the_value(self):
        too_long = "A" * 65
        for name in ("lower", "1ABC", "A-B", "", too_long, "PATH", "HOME", "USER", "LOGNAME", "SHELL", "LANG",
                     "LAYR_ROOT", "LAYR_SESSION", "BROWSER", "CODEX_HOME", "CLAUDE_CONFIG_DIR", "STUDIO_CLAUDE_OPTIONS",
                     "STUDIO_PROJECT_SECRETS_FD", "STUDIO_ANY", "CODEX_SANDBOX", "CLAUDECODE", 7, None):
            with self.subTest(name=name):
                response = await self.call("secrets.set", {"name": name, "value": VALUE})
                self.assertEqual(response["error"]["code"], "invalid_params")
                self.assert_no_value(response)
        for value in ("", VALUE + "\0", "x" * (MAX_VALUE_BYTES + 1), "é" * (MAX_VALUE_BYTES // 2 + 1), VALUE + "\ud800", 5, None):
            with self.subTest(size=len(value) if isinstance(value, str) else value):
                response = await self.call("secrets.set", {"name": "TOKEN", "value": value})
                self.assertEqual(response["error"]["code"], "invalid_params")
                self.assert_no_value(response)
        accepted = await self.call("secrets.set", {"name": "_A9", "value": "x" * MAX_VALUE_BYTES})
        self.assertEqual(accepted["result"]["names"], ["_A9"])
        missing = await self.broker.request({"id": "other", "method": "secrets.set",
                                             "params": {"projectId": "b" * 64, "name": "TOKEN", "value": VALUE}}, None)
        self.assertEqual(missing["error"]["code"], "not_found")
        escape = await self.broker.request({"id": "escape", "method": "secrets.list",
                                            "params": {"projectId": "../" + PROJECT}}, None)
        self.assertEqual(escape["error"]["code"], "invalid_params")

    async def test_a_project_has_at_most_64_secrets(self):
        for index in range(64):
            self.assertIn("result", await self.call("secrets.set", {"name": f"S{index}", "value": "v"}))
        response = await self.call("secrets.set", {"name": "S64", "value": "v"})
        self.assertEqual(response["error"]["code"], "invalid_params")
        self.assertIn("result", await self.call("secrets.set", {"name": "S0", "value": "new"}))

    async def test_a_repeated_request_id_applies_once(self):
        params = {"name": "TOKEN", "value": VALUE}
        first = await self.call("secrets.set", params, "set-1")
        removed = await self.call("secrets.remove", {"name": "TOKEN"}, "remove-1")
        self.assertEqual(removed["result"]["names"], [])
        # A reply lost after the set: the repeat returns the saved reply and does not set again.
        self.assertEqual(await self.call("secrets.set", params, "set-1"), first)
        self.assertEqual((await self.call("secrets.list", {}))["result"]["names"], [])
        changed = await self.call("secrets.set", {"name": "TOKEN", "value": "other"}, "set-1")
        self.assertEqual(changed["error"]["code"], "id_conflict")
        self.assert_no_value(changed)
        self.assertEqual(await self.call("secrets.remove", {"name": "TOKEN"}, "remove-1"), removed)
        missing = await self.call("secrets.remove", {"name": "NEVER_SET"})
        self.assertEqual(missing["result"]["names"], [])

    async def test_the_write_is_atomic(self):
        await self.call("secrets.set", {"name": "TOKEN", "value": VALUE})
        before = self.file.read_bytes()
        with patch("common.os.replace", side_effect=OSError("disk")):
            response = await self.call("secrets.set", {"name": "OTHER", "value": "second"})
        self.assertEqual(response["error"]["code"], "internal")
        self.assertEqual(self.file.read_bytes(), before)
        self.assertEqual(sorted(path.name for path in self.file.parent.iterdir()), [self.file.name])
        await self.call("secrets.set", {"name": "OTHER", "value": "second"})
        self.assertEqual(sorted(path.name for path in self.file.parent.iterdir()), [self.file.name])
        self.assertEqual(stat.S_IMODE(self.file.stat().st_mode), 0o600)

    async def test_the_guest_service_forwards_the_methods_to_the_broker(self):
        import service
        self.assertTrue({"secrets.set", "secrets.remove", "secrets.list"} <= service.METHODS)
        self.assertIn("secrets.list", service.READ_METHODS)
        self.assertFalse({"secrets.set", "secrets.remove"} & service.READ_METHODS)
        self.assertIn("secrets.list", self.broker.read_methods)
        self.assertFalse({"secrets.set", "secrets.remove"} & self.broker.read_methods)
        forwarded = []

        async def admin_request(request_id, method, params, emit=None):
            forwarded.append((request_id, method, params))
            return {"projectId": PROJECT, "names": ["TOKEN"]}

        guest = service.Service(self.root / "guest", self.root / "guest", self.root / "projects", self.root)
        try:
            with patch("layr_admin_client.admin_request", admin_request):
                for method, params in (("secrets.set", {"name": "TOKEN", "value": VALUE}),
                                       ("secrets.remove", {"name": "TOKEN"}), ("secrets.list", {})):
                    response = await guest.request({"id": method, "method": method,
                                                    "params": {"projectId": PROJECT, **params}}, None)
                    self.assertEqual(response["result"]["names"], ["TOKEN"])
        finally:
            guest.close()
        self.assertEqual([row[1] for row in forwarded], ["secrets.set", "secrets.remove", "secrets.list"])
        self.assertEqual(self.files_with_value(self.root / "guest"), [])


class InjectionTests(unittest.IsolatedAsyncioTestCase):
    """Every agent process of the project starts with the secrets; no launch record has a value."""

    async def asyncSetUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="inject-", dir="/tmp")
        self.root = Path(self.temp.name).resolve()
        self.state, self.store = self.root / "state", self.root / "store"
        self.line = self.store / "projects" / PROJECT / "lines/main"
        self.line.mkdir(parents=True)
        self.home = self.root / "home"
        self.home.mkdir()
        self.broker = Broker(self.state, self.store, modules=())
        response = await self.broker.request({"id": "set", "method": "secrets.set", "params": {
            "projectId": PROJECT, "name": "ZULIP_APEIRO_SHOWROOM_API_KEY", "value": VALUE}}, None)
        self.assertIn("result", response)
        self.owner = pwd.getpwuid(os.getuid()).pw_name
        self.agents = AgentHandlers(self.state, self.store)
        self.agent = str(uuid.uuid4())
        self.context = {"agentId": self.agent, "projectId": PROJECT, "line": "main", "subpath": ".",
                        "readOnly": False, "owner": self.owner, "uid": os.getuid(), "gid": os.getgid(),
                        "home": str(self.home)}
        (self.state / "agents" / (self.agent + ".json")).write_text(json.dumps(self.context))
        runuser = patch.object(project_env, "RUNUSER", str(fake_runuser(self.root)))
        runuser.start()
        self.addCleanup(runuser.stop)

    async def asyncTearDown(self):
        self.broker.close()
        self.temp.cleanup()

    def assert_wrapped(self, argv, command):
        script = str(Path(project_env.__file__).resolve())
        secrets = str(self.state / "secrets" / (PROJECT + ".json"))
        self.assertEqual(argv, [sys.executable, "-I", script, "root", secrets, "--", project_env.RUNUSER, "-u",
                                self.owner, "--", sys.executable, "-I", script, "user", "--", *command])

    async def test_a_provider_starts_through_the_wrapper_with_the_launch_names_only(self):
        for provider, profile, argv, env in (
                ("codex", ".codex/studio-accounts/" + "0" * 32,
                 ["codex", "app-server", "--listen", "stdio://", "-c", 'cli_auth_credentials_store="file"'], "CODEX_HOME"),
                ("claude", ".claude/studio-accounts/" + "1" * 32,
                 ["node", "/opt/codex-studio/claude_bridge/bridge.mjs",
                  str(self.home / ".codex/studio-bridges" / self.agent)], "CLAUDE_CONFIG_DIR")):
            with self.subTest(provider=provider):
                source = self.root / "studio" / profile
                source.mkdir(parents=True)
                (source / (".credentials.json" if provider == "claude" else "auth.json")).write_text("{}")
                self.agents.studio_home = self.root / "studio"
                self.agents.command = AsyncMock(return_value="")
                self.agents.native.start = AsyncMock(return_value={"handle": "linux-worker:" + self.agent})
                await self.agents.dispatch("start-" + provider, "layr.provider.start", {
                    "agentId": self.agent, "provider": provider, "profile": profile,
                    "handle": "linux-worker:" + self.agent, "argv": argv, "cwd": str(self.line),
                    "env": {env: str(self.home / profile)}}, None)
                handle, launch, _directory = self.agents.native.start.await_args.args
                self.assertNotIn(VALUE, json.dumps(launch))
                self.assert_wrapped(launch["argv"], argv)
                self.assertTrue(set(launch["env"]) <= RESERVED)
                saved = (self.state / "agents" / (self.agent + ".json")).read_text()
                self.assertNotIn(VALUE, saved)

    async def exec(self, identity, argv):
        """layr.exec through the real command supervisor; returns the launch and the result."""
        import common
        launches = []

        async def supervised(directory, config, association, wait):
            launches.append(json.loads(json.dumps(config)))
            return await real(directory, config, association, wait)

        real = common.supervised
        with patch("common.supervised", supervised):
            result = await self.agents.dispatch(identity, "layr.exec", {"agentId": self.agent, "argv": argv}, None)
        self.assertEqual(result["exitCode"], 0, base64.b64decode(result["stderr"]))
        return launches[0], base64.b64decode(result["stdout"]).decode()

    async def test_a_layr_exec_process_has_the_secret_in_its_environment(self):
        dump = [sys.executable, "-c", "import json, os; print(json.dumps(dict(os.environ)))"]
        launch, output = await self.exec("exec-1", dump)
        self.assertNotIn(VALUE, json.dumps(launch))
        self.assert_wrapped(launch["argv"], dump)
        self.assertTrue(set(launch["env"]) <= RESERVED)
        environment = json.loads(output)
        self.assertEqual(environment["ZULIP_APEIRO_SHOWROOM_API_KEY"], VALUE)
        self.assertNotIn(project_env.FD_VARIABLE, environment)
        self.assertEqual((environment["USER"], environment["LAYR_SESSION"]), (self.owner, self.agent))
        # The command journal keeps the output of the command, not its launch environment.
        for path in (self.state / "commands").rglob("*"):
            if path.is_file() and path.suffix == ".json":
                self.assertNotIn(VALUE, path.read_text())

    async def test_the_launch_names_win_over_a_secret_file(self):
        # The broker refuses these names; a file with one cannot replace the launch value.
        path = self.state / "secrets" / (PROJECT + ".json")
        path.write_text(json.dumps({"version": 1, "secrets": {"HOME": "/elsewhere", "EXTRA": VALUE}}))
        dump = [sys.executable, "-c", "import json, os; print(json.dumps(dict(os.environ)))"]
        environment = json.loads((await self.exec("exec-2", dump))[1])
        self.assertEqual((environment["HOME"], environment["EXTRA"]), (str(self.home), VALUE))

    async def test_a_later_secret_reaches_a_later_process(self):
        await self.broker.request({"id": "later", "method": "secrets.set", "params": {
            "projectId": PROJECT, "name": "LATER", "value": "second"}}, None)
        dump = [sys.executable, "-c", "import os; print(os.environ.get('LATER'))"]
        self.assertEqual((await self.exec("exec-3", dump))[1].strip(), "second")

    async def test_an_operator_command_gets_the_secrets(self):
        projects = Projects(self.state, self.store, AsyncMock(), None)
        dump = [sys.executable, "-c", "import os; print(os.environ['ZULIP_APEIRO_SHOWROOM_API_KEY'])"]
        with patch("layr_projects.supervised", AsyncMock(return_value={"exitCode": 0})) as run:
            await projects.exec("op-0", {"projectId": PROJECT, "argv": dump})
        config = run.await_args.args[1]
        self.assertNotIn(VALUE, json.dumps(config))
        self.assert_wrapped(config["argv"], dump)
        self.assertTrue(set(config["env"]) <= RESERVED)
        result = await projects.exec("op-1", {"projectId": PROJECT, "argv": dump})
        self.assertEqual(base64.b64decode(result["stdout"]).decode().strip(), VALUE)

    def test_the_reserved_names_cover_the_prefixes_of_studio_and_the_providers(self):
        self.assertTrue({"PATH", "HOME", "CODEX_HOME", "CLAUDE_CONFIG_DIR", project_env.FD_VARIABLE} <= RESERVED)
        self.assertEqual(RESERVED_PREFIXES, ("STUDIO_", "LAYR_", "CODEX_", "CLAUDE_"))

    def test_the_user_step_without_its_descriptor_fails_without_running_the_command(self):
        import subprocess
        marker = self.root / "ran"
        done = subprocess.run([sys.executable, "-I", project_env.__file__, "user", "--", "touch", str(marker)],
                              env={"PATH": os.environ["PATH"]}, capture_output=True, text=True, timeout=30)
        self.assertEqual(done.returncode, 126)
        self.assertIn(project_env.FD_VARIABLE, done.stderr)
        self.assertFalse(marker.exists())

    def test_an_invalid_secret_file_stops_the_start(self):
        import subprocess
        path = self.state / "secrets" / (PROJECT + ".json")
        path.write_text(json.dumps({"version": 1, "secrets": {"bad name": VALUE}}))
        marker = self.root / "ran"
        argv = project_env.wrap(self.state, PROJECT, self.owner, ["touch", str(marker)])
        done = subprocess.run(argv, env={"PATH": os.environ["PATH"]}, capture_output=True, text=True, timeout=30)
        self.assertEqual(done.returncode, 126)
        self.assertNotIn(VALUE, done.stderr)
        self.assertFalse(marker.exists())


@unittest.skipUnless(sys.platform == "linux" and os.geteuid() == 0 and Path(project_env.RUNUSER).exists(),
                     "needs root on Linux with runuser")
class RunuserTests(unittest.TestCase):
    """The real runuser keeps the descriptor; the root steps never hold a value in their environment."""

    def test_the_secret_crosses_runuser_to_the_unprivileged_command(self):
        import subprocess
        with tempfile.TemporaryDirectory() as name:
            state = Path(name)
            (state / "secrets").mkdir(mode=0o700)
            (state / "secrets" / (PROJECT + ".json")).write_text(json.dumps({"version": 1, "secrets": {"TOKEN": VALUE}}))
            check = ("import json, os\nlinks = []\nfor fd in os.listdir('/proc/self/fd'):\n"
                     "    try: links.append(os.readlink('/proc/self/fd/' + fd))\n"
                     "    except OSError: pass\nprint(json.dumps([os.getuid(), dict(os.environ), links]))")
            argv = project_env.wrap(state, PROJECT, "nobody", [sys.executable, "-c", check])
            done = subprocess.run(argv, env={"PATH": "/usr/bin:/bin", "HOME": "/nonexistent", "LANG": "C.UTF-8"},
                                  cwd="/", capture_output=True, text=True, timeout=60)
        self.assertEqual(done.returncode, 0, done.stderr)
        uid, environment, descriptors = json.loads(done.stdout)
        self.assertEqual(uid, pwd.getpwnam("nobody").pw_uid)
        self.assertEqual(environment["TOKEN"], VALUE)
        self.assertNotIn(project_env.FD_VARIABLE, environment)
        self.assertFalse([path for path in descriptors if "memfd" in path])
        self.assertNotIn(VALUE, " ".join(argv))


if __name__ == "__main__":
    unittest.main()
