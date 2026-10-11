"""The project vault in the sidebar without a VM (docs/vault.md): the host records of the VM
writer's report, and pulls through a stand-in layr-desk command."""
from __future__ import annotations

from concurrent.futures import ThreadPoolExecutor
from contextlib import ExitStack
import json
import os
from pathlib import Path
import shlex
import stat
import tempfile
import time
import unittest
from typing import Any
from unittest.mock import patch

import codex_vm_vault as vault
from codex_linux_vm import LinuxVMError


class FakeClient:
    """Answers vault.status from a table: a dict, or an error to raise."""

    def __init__(self, state_dir: Path, replies: dict[str, Any]):
        self.state_dir, self.replies = state_dir, replies
        self.calls: list[str] = []

    def call(self, method: str, params: dict[str, Any], **options: Any) -> dict[str, Any]:
        assert method == 'vault.status'
        self.calls.append(params['projectId'])
        reply = self.replies[params['projectId']]
        if isinstance(reply, Exception):
            raise reply
        return dict(reply)


def project(state: Path, name: str) -> Path:
    folder = state / 'layr-projects' / name
    folder.mkdir(parents=True)
    (folder / 'project.json').write_text(json.dumps({'projectId': name}))
    return folder


class Reports(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.state = Path(self.temp.name)

    def tearDown(self) -> None:
        self.temp.cleanup()

    def test_a_check_keeps_each_writer_report_and_says_when_a_view_changed(self) -> None:
        for name in ('ok', 'failed', 'plain', 'down'):
            project(self.state, name)
        client = FakeClient(self.state, {
            'ok': {'prefix': 'ok', 'lastPush': 1791720000, 'lastAttempt': 1791720600, 'lastError': None},
            'failed': {'prefix': 'failed', 'lastPush': None, 'lastError': 'S3 said 403'},
            'plain': LinuxVMError('The project has no vault', code='not_found'),
            'down': LinuxVMError('the VM does not answer', code='unavailable'),
        })
        self.assertIsNone(vault.vault_view(self.state, 'ok'))
        self.assertTrue(vault.check(client))
        self.assertEqual(vault.vault_view(self.state, 'ok'),
                         {'projectId': 'ok', 'state': 'ok', 'prefix': 'ok', 'lastPush': 1791720000})
        self.assertEqual(vault.vault_view(self.state, 'failed'),
                         {'projectId': 'failed', 'state': 'error', 'prefix': 'failed', 'reason': 'S3 said 403'})
        self.assertEqual(vault.vault_view(self.state, 'plain'), {'projectId': 'plain', 'state': 'none'})
        # A VM that cannot answer now leaves no report, not a false "none".
        self.assertIsNone(vault.vault_view(self.state, 'down'))
        # Only the time of an attempt changed: the view is the same, so no refresh.
        client.replies['ok'] = {**client.replies['ok'], 'lastAttempt': 1791721200}
        self.assertFalse(vault.check(client))
        client.replies['ok'] = {**client.replies['ok'], 'lastPush': 1791721200}
        self.assertTrue(vault.check(client))

    def test_the_desk_link_needs_an_https_site(self) -> None:
        folder = project(self.state, 'p')
        (folder / 'vault.json').write_text(json.dumps({'configured': True, 'prefix': 'p'}))
        vault.store_site(self.state, 'p', 'https://desk.example.com')
        view = vault.vault_view(self.state, 'p')
        assert view is not None
        self.assertEqual(view['site'], 'https://desk.example.com/')
        for bad in ('http://desk.example.com/', 'https://desk example/', 'file:///etc'):
            with self.assertRaises(ValueError):
                vault.store_site(self.state, 'p', bad)


class Ticks(unittest.TestCase):
    def test_a_new_report_or_a_new_site_refreshes_the_sidebar_once(self) -> None:
        with tempfile.TemporaryDirectory() as name:
            state = Path(name)
            project(state, 'p')
            client = FakeClient(state, {'p': {'prefix': 'p', 'lastPush': 100, 'lastError': None}})
            client.status = lambda: {'state': 'running'}  # type: ignore[attr-defined]
            runtime = type('Runtime', (), {})()
            runtime.pool = type('Pool', (), {'submit': staticmethod(lambda run: run())})()
            refreshes: list[int] = []
            with patch('codex_linux_workspaces.client', lambda r: client), \
                    patch('codex_vm_agents.refresh_readiness', lambda r: refreshes.append(1)), \
                    patch('platform.system', lambda: 'Darwin'), \
                    patch.object(vault, 'mac_check', lambda state_dir: None):
                def tick() -> None:
                    runtime._vault_checked = -vault.INTERVAL * 2
                    vault.tick(runtime)
                tick()
                self.assertEqual(len(refreshes), 1)
                tick()
                self.assertEqual(len(refreshes), 1)
                # The site comes from the Mac, not from the report of the VM.
                vault.store_site(state, 'p', 'https://desk.example.com/')
                tick()
                self.assertEqual(len(refreshes), 2)
                view = vault.vault_view(state, 'p')
                assert view is not None
                self.assertEqual(view['site'], 'https://desk.example.com/')


class Runtime:
    def __init__(self) -> None:
        self.pool = ThreadPoolExecutor(max_workers=2)


class Pulls(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.state = Path(self.temp.name) / 'state'
        self.downloads = Path(self.temp.name) / 'Downloads'
        self.downloads.mkdir()
        folder = project(self.state, 'p')
        (folder / 'vault.json').write_text(json.dumps({'configured': True, 'prefix': 'proj'}))
        vault.store_site(self.state, 'p', 'https://desk.example.com/')
        # A stand-in layr-desk: it counts its runs and makes the folder that --to names.
        self.runs = Path(self.temp.name) / 'runs'
        self.tool = Path(self.temp.name) / 'layr-desk'
        self.tool.write_text(
            '#!/bin/sh\n'
            f'echo "$@" >> {self.runs}\n'
            'while [ "$1" != "--to" ]; do shift; done\n'
            'mkdir "$2" && echo \'{"seq": 7, "state": "s7", "files": 3, "bytes": 42}\'\n')
        self.tool.chmod(self.tool.stat().st_mode | stat.S_IXUSR)
        self.runtime = Runtime()
        client = type('Client', (), {'state_dir': self.state})()
        self.patches = ExitStack()
        self.patches.enter_context(patch.dict(os.environ, {'STUDIO_LAYR_DESK_BIN': str(self.tool)}))
        self.patches.enter_context(patch('codex_linux_workspaces.client', lambda runtime: client))

    def tearDown(self) -> None:
        self.patches.close()
        self.runtime.pool.shutdown(wait=True)
        self.temp.cleanup()

    def wait(self, request_id: str) -> dict[str, Any]:
        for _ in range(200):
            job = vault.start_pull(self.runtime, 'p', request_id, str(self.downloads))
            if job['state'] != 'running':
                return job
            time.sleep(0.02)
        raise AssertionError('the pull did not end')

    def test_a_repeated_request_reports_the_same_pull(self) -> None:
        first = vault.start_pull(self.runtime, 'p', 'pull-1', str(self.downloads))
        self.assertTrue(first['folder'].startswith(str(self.downloads / 'proj ')))
        done = self.wait('pull-1')
        self.assertEqual((done['state'], done['seq'], done['files'], done['folder']), ('done', 7, 3, first['folder']))
        self.assertEqual(len(self.runs.read_text().splitlines()), 1)
        args = self.runs.read_text().split()
        self.assertEqual(args[:5], ['pull', '--site', 'https://desk.example.com/', '--project', 'proj'])
        # Another request pulls again, into another new folder.
        other = self.wait('pull-2')
        self.assertNotEqual(other['folder'], first['folder'])
        self.assertEqual(len(self.runs.read_text().splitlines()), 2)

    def test_a_failed_pull_says_why(self) -> None:
        self.tool.write_text('#!/bin/sh\necho "Error: the vault proj is locked on this Mac" >&2\nexit 1\n')
        failed = self.wait('pull-3')
        self.assertEqual((failed['state'], failed['error']), ('failed', 'the vault proj is locked on this Mac'))

    def test_a_pull_needs_a_vault_a_site_and_a_folder(self) -> None:
        with self.assertRaises(ValueError):
            vault.start_pull(self.runtime, 'p', 'pull-4', str(self.downloads / 'missing'))
        with self.assertRaises(ValueError):
            vault.start_pull(self.runtime, 'p', 'pull-5', 'relative/folder')
        (self.state / 'layr-projects/p/vault-site.json').unlink()
        with self.assertRaises(ValueError):
            vault.start_pull(self.runtime, 'p', 'pull-6', str(self.downloads))
        (self.state / 'layr-projects/p/vault.json').write_text(json.dumps({'configured': False}))
        with self.assertRaises(ValueError):
            vault.start_pull(self.runtime, 'p', 'pull-7', str(self.downloads))
        self.assertFalse(self.runs.exists())



FAKE_DESK = r'''#!/usr/bin/env python3
"""A stand-in layr-desk: it keeps its state in the folder of FAKE_DESK_STATE."""
import json, os, sys
from pathlib import Path
state = Path(os.environ["FAKE_DESK_STATE"])
with open(state / "calls", "a") as log:
    log.write(json.dumps(sys.argv[1:]) + "\n")
def load(name, default):
    path = state / name
    return json.loads(path.read_text()) if path.exists() else default
args = sys.argv[1:]
if args[:2] == ["service", "status"]:
    print(json.dumps(load("service.json", {"installed": False})))
elif args[:2] == ["service", "install"]:
    (state / "service.json").write_text(json.dumps({"installed": True, "program": str(Path(sys.argv[0]).resolve()),
        "site": args[args.index("--site") + 1], "port": 8095, "at": str(state / "Desk")}))
elif args[0] == "status":
    print(json.dumps({"site": args[2], "projects": [{"name": n, "unlocked": n in load("keys.json", [])}
        for n in ["proj", "other"]]}))
elif args[0] == "unlock":
    words = sys.stdin.read()
    (state / "stdin").write_text(words)
    if words.strip() != "right words":
        print('Error: "bogus" is not a word of the paper key list', file=sys.stderr)
        sys.exit(1)
    project = args[args.index("--project") + 1]
    (state / "keys.json").write_text(json.dumps(load("keys.json", []) + [project]))
'''


class ThisMac(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        root = Path(self.temp.name)
        self.state, self.desk_state = root / "state", root / "desk"
        self.desk_state.mkdir()
        folder = project(self.state, "p")
        (folder / "vault.json").write_text(json.dumps({"configured": True, "prefix": "proj"}))
        vault.store_site(self.state, "p", "https://desk.example.com/")
        self.tool = root / "bin" / "layr-desk"
        self.tool.parent.mkdir()
        self.tool.write_text(FAKE_DESK)
        self.tool.chmod(0o755)
        client = type("Client", (), {"state_dir": self.state})()
        self.patches = ExitStack()
        self.patches.enter_context(patch.dict(os.environ, {"FAKE_DESK_STATE": str(self.desk_state),
                                                           "STUDIO_LAYR_DESK_BIN": str(self.tool)}))
        self.patches.enter_context(patch("codex_linux_workspaces.client", lambda runtime: client))
        self.patches.enter_context(patch("codex_vm_agents.refresh_readiness", lambda runtime: None))

    def tearDown(self) -> None:
        self.patches.close()
        self.temp.cleanup()

    def calls(self) -> list[list[str]]:
        path = self.desk_state / "calls"
        return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []

    def test_the_app_installs_its_disk_agent_once_and_reports_the_lock_state(self) -> None:
        with patch.object(vault, "bundled_layr_desk", lambda: self.tool):
            vault.mac_check(self.state)
            view = vault.vault_view(self.state, "p")
            assert view is not None
            self.assertEqual((view["unlocked"], view["disk"]), (False, str(self.desk_state / "Desk" / "proj")))
            self.assertIn(["service", "install", "--site", "https://desk.example.com"], self.calls())
            # The agent already runs this program for this site: no second install.
            before = len([c for c in self.calls() if c[:2] == ["service", "install"]])
            vault.mac_check(self.state)
            self.assertEqual(len([c for c in self.calls() if c[:2] == ["service", "install"]]), before)

    def test_a_layr_desk_outside_the_app_never_installs_the_agent(self) -> None:
        vault.mac_check(self.state)
        self.assertFalse([c for c in self.calls() if c[:2] == ["service", "install"]])
        view = vault.vault_view(self.state, "p")
        assert view is not None
        self.assertEqual(view["unlocked"], False)
        self.assertNotIn("disk", view)

    def test_the_words_go_to_layr_desk_on_stdin_only(self) -> None:
        runtime = Runtime()
        try:
            with self.assertRaises(ValueError) as refused:
                vault.unlock(runtime, "p", "bogus words")
            self.assertNotIn("bogus", str(refused.exception))
            self.assertIn("one word is not a word of the paper key list", str(refused.exception))
            self.assertEqual(vault.unlock(runtime, "p", "right words\n"), {"unlocked": True})
            self.assertEqual((self.desk_state / "stdin").read_text(), "right words\n")
            unlock_calls = [c for c in self.calls() if c[0] == "unlock"]
            self.assertTrue(unlock_calls)
            self.assertFalse(any("words" in " ".join(c) for c in unlock_calls))
            self.assertEqual(unlock_calls[0][-2:], ["--paper-file", "-"])
            view = vault.vault_view(self.state, "p")
            assert view is not None
            self.assertTrue(view["unlocked"])
        finally:
            runtime.pool.shutdown()


FAKE_SETUP_DESK = r"""#!/usr/bin/env python3
import json, os, sys, time
from pathlib import Path
state = Path(os.environ["FAKE_DESK_STATE"])
with open(state / "calls", "a") as log:
    log.write(json.dumps(sys.argv[1:]) + "\n")
args = sys.argv[1:]
if args[0] == "aws-profiles":
    print(json.dumps({"profiles": [{"name": "work", "region": "eu-central-1", "sso": True, "signedIn": False}]}))
elif args[0] == "deploy":
    mode = (state / "mode").read_text() if (state / "mode").exists() else "ok"
    print("layr-desk: creating studio-vault-" + args[2], file=sys.stderr, flush=True)
    if mode == "signin":
        print("Error: the AWS profile work needs a sign-in: layr-desk aws-login --profile work", file=sys.stderr)
        sys.exit(3)
    if mode == "fail":
        print("Error: CreateStack studio-vault-x: HTTP 400\n\nCaused by:\n    0: AlreadyExists", file=sys.stderr)
        sys.exit(1)
    print(json.dumps({"stack": "s", "site": "https://desk.example.com/" + args[2] + "/", "bucket": "b"}))
elif args[0] == "aws-login":
    print(json.dumps({"page": "https://device.example/?code=ABCD", "code": "ABCD"}), flush=True)
    print(json.dumps({"signedIn": True}))
"""


class Setup(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        root = Path(self.temp.name)
        self.state, self.desk_state, self.home = root / "state", root / "desk", root / "home"
        self.desk_state.mkdir()
        self.settings = self.home / ".config/studio-vault"
        self.settings.mkdir(parents=True)
        (self.settings / "gateway.json").write_text(json.dumps(
            {"host": "desk.example.com", "profile": "work", "region": "eu-west-1", "role": "r", "artifacts": "a"}))
        folder = project(self.state, "p")
        (folder / "project.json").write_text(json.dumps({"projectId": "p", "source": "/Users/me/My Project.v2"}))
        (folder / "vault.json").write_text(json.dumps({"configured": False}))
        self.tool = root / "layr-desk"
        self.tool.write_text(FAKE_SETUP_DESK)
        self.tool.chmod(0o755)
        self.runtime = Runtime()
        self.refreshed: list[Any] = []
        client = type("Client", (), {"state_dir": self.state})()
        self.patches = ExitStack()
        self.patches.enter_context(patch.dict(os.environ, {"FAKE_DESK_STATE": str(self.desk_state),
                                                           "STUDIO_LAYR_DESK_BIN": str(self.tool),
                                                           "HOME": str(self.home)}))
        self.patches.enter_context(patch("codex_linux_workspaces.client", lambda runtime: client))
        self.patches.enter_context(patch.object(vault, "_refresh", self.refreshed.append))

    def tearDown(self) -> None:
        self.patches.close()
        self.runtime.pool.shutdown(wait=True)
        self.temp.cleanup()

    def calls(self) -> list[list[str]]:
        path = self.desk_state / "calls"
        return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []

    def keys(self, prefix: str) -> None:
        (self.settings / prefix).mkdir()
        (self.settings / prefix / "recipient.txt").write_text("age1x\n")

    def wait(self, start: Any, *args: Any) -> dict[str, Any]:
        for _ in range(300):
            job: dict[str, Any] = start(self.runtime, *args)
            if job["state"] != "running":
                return job
            time.sleep(0.02)
        raise AssertionError("the job did not end")

    def test_the_facts_name_the_vault_after_the_folder_and_show_the_keys_and_profiles(self) -> None:
        facts = vault.setup_facts(self.runtime, "p")
        self.assertEqual(facts["prefix"], "my-project-v2")
        self.assertFalse(facts["keys"])
        self.assertEqual((facts["site"], facts["region"], facts["gatewayProfile"]),
                         ("https://desk.example.com/", "eu-west-1", "work"))
        self.assertEqual(facts["profiles"][0]["name"], "work")
        self.keys("other")
        self.assertTrue(vault.setup_facts(self.runtime, "p", "other")["keys"])
        with self.assertRaises(ValueError):
            vault.setup_facts(self.runtime, "p", "api")

    def test_a_project_with_a_vault_has_no_setup(self) -> None:
        (self.state / "layr-projects/p/vault.json").write_text(json.dumps({"configured": True, "prefix": "x"}))
        with self.assertRaises(ValueError):
            vault.setup_facts(self.runtime, "p")

    def test_the_setup_needs_the_keys_then_deploys_with_the_vm_writer(self) -> None:
        with self.assertRaises(ValueError):
            vault.start_setup(self.runtime, "p", "s-1", "demo", "work", "eu-central-1")
        self.assertFalse(self.calls())
        self.keys("demo")
        done = self.wait(vault.start_setup, "p", "s-2", "demo", "work", "eu-central-1")
        self.assertEqual((done["state"], done["site"], done["prefix"]), ("done", "https://desk.example.com/demo/", "demo"))
        self.assertNotIn("progress", done)
        deploy = [c for c in self.calls() if c[0] == "deploy"]
        self.assertEqual(len(deploy), 1)
        self.assertEqual(deploy[0][:7], ["deploy", "--project", "demo", "--profile", "work", "--region", "eu-central-1"])
        writer = shlex.split(deploy[0][deploy[0].index("--writer") + 1])
        self.assertTrue(writer[1].endswith("codex_vault.py"))
        self.assertEqual(writer[2:], ["configure", "/Users/me/My Project.v2"])
        self.assertEqual(len(self.refreshed), 1)
        # The same request id reports the same setup and deploys no second time.
        self.assertEqual(vault.start_setup(self.runtime, "p", "s-2", "demo", "work", "eu-central-1"), done)
        self.assertEqual(len([c for c in self.calls() if c[0] == "deploy"]), 1)

    def test_a_profile_without_a_sign_in_and_a_failure_say_so(self) -> None:
        self.keys("demo")
        (self.desk_state / "mode").write_text("signin")
        signin = self.wait(vault.start_setup, "p", "s-3", "demo", "work", "eu-central-1")
        self.assertEqual(signin["state"], "signin")
        self.assertIn("aws-login --profile work", signin["error"])
        (self.desk_state / "mode").write_text("fail")
        failed = self.wait(vault.start_setup, "p", "s-4", "demo", "work", "eu-central-1")
        self.assertEqual((failed["state"], failed["error"]),
                         ("failed", "CreateStack studio-vault-x: HTTP 400 Caused by: 0: AlreadyExists"))
        self.assertFalse(self.refreshed)

    def test_a_sign_in_shows_the_page_and_its_code(self) -> None:
        done = self.wait(vault.start_login, "work", "l-1")
        self.assertEqual((done["state"], done["page"], done["code"]), ("done", "https://device.example/?code=ABCD", "ABCD"))
        self.assertIn(["aws-login", "--profile", "work", "--json"], self.calls())
        with self.assertRaises(ValueError):
            vault.start_login(self.runtime, "-bad", "l-2")


if __name__ == '__main__':
    unittest.main()
