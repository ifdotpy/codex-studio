"""Every agent user of a project joins that project's group, the layr principal for overlays that
all agents of the project, and no other project, read."""
from __future__ import annotations

from contextlib import redirect_stderr
import hashlib
import io
import json
import pathlib
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

import layr_agents


class FakeSystem:
    """Users and groups with the commands that change them, as layr_agents runs them."""

    def __init__(self):
        self.users: dict[str, int] = {}
        self.groups: dict[str, list[str]] = {layr_agents.AGENTS_GROUP: []}
        self.commands: list[list[str]] = []

    def run(self, command, **_options):
        self.commands.append(command)
        if command[0] == "/usr/sbin/groupadd":
            self.groups.setdefault(command[-1], [])
        elif command[0] == "/usr/sbin/useradd":
            name = command[-1]
            self.users[name] = 2000 + len(self.users)
            self.groups[layr_agents.AGENTS_GROUP].append(name)
        elif command[0] == "/usr/sbin/usermod":
            self.groups[command[2]].append(command[3])
        return SimpleNamespace(returncode=0)

    def getpwnam(self, name):
        if name not in self.users:
            raise KeyError(name)
        return SimpleNamespace(pw_uid=self.users[name], pw_gid=self.users[name], pw_dir="/home/" + name)

    def getgrnam(self, name):
        if name not in self.groups:
            raise KeyError(name)
        return SimpleNamespace(gr_mem=list(self.groups[name]))


class ProjectGroup(unittest.TestCase):
    def setUp(self):
        self.system = FakeSystem()
        home = SimpleNamespace(is_symlink=lambda: False, is_dir=lambda: True, chmod=lambda mode: None)
        for target, value in (("layr_agents.subprocess.run", self.system.run),
                              ("layr_agents.pwd.getpwnam", self.system.getpwnam),
                              ("layr_agents.grp.getgrnam", self.system.getgrnam),
                              ("layr_agents.Path", lambda path: home)):
            patcher = patch(target, value)
            patcher.start()
            self.addCleanup(patcher.stop)

    def test_the_lead_and_each_worker_join_only_their_project_group(self):
        sap, other = "a" * 64, "b" * 64
        group = layr_agents.project_group(sap)
        self.assertEqual(group, "studio-g-" + hashlib.sha256(sap.encode()).hexdigest()[:16])
        lead = layr_agents.ensure_project_owner(sap)
        worker = layr_agents.join_project_group(layr_agents._user("studio-a-" + "1" * 22), sap)
        stranger = layr_agents.ensure_project_owner(other)
        self.assertEqual(self.system.groups[group], [lead["owner"], worker["owner"]])
        self.assertNotIn(stranger["owner"], self.system.groups[group])
        # A second bind adds nobody twice.
        layr_agents.ensure_project_owner(sap)
        self.assertEqual(self.system.groups[group], [lead["owner"], worker["owner"]])
        self.assertEqual(sum(1 for c in self.system.commands if c[:2] == ["/usr/sbin/groupadd", "--system"]
                             and c[-1] == group), 1)

    def test_users_made_before_the_groups_join_them_at_service_start(self):
        sap, other = "a" * 64, "b" * 64
        lead = layr_agents._user("studio-p-" + hashlib.sha256(sap.encode()).hexdigest()[:16])
        worker = layr_agents._user("studio-a-" + "1" * 22)
        with tempfile.TemporaryDirectory() as folder:
            root, associations = pathlib.Path(folder, "layr"), pathlib.Path(folder, "agents")
            (root / "projects" / sap / "lines" / "main").mkdir(parents=True)
            (root / "projects" / other).mkdir(parents=True)
            associations.mkdir()
            for name, value in (("w.json", {"projectId": sap, "owner": worker["owner"]}),
                                ("lead.json", {"projectId": sap, "owner": lead["owner"]}),
                                ("root.json", {"projectId": sap, "owner": "root"}),
                                ("broken.json", None)):
                (associations / name).write_text("{" if value is None else json.dumps(value))
            handlers = SimpleNamespace(layr_root=root, associations=associations)
            with redirect_stderr(io.StringIO()) as errors:
                layr_agents.AgentHandlers.join_project_groups(handlers)
                layr_agents.AgentHandlers.join_project_groups(handlers)
        self.assertEqual(self.system.groups[layr_agents.project_group(sap)], [lead["owner"], worker["owner"]])
        # A project without main makes no owner, and a bad record changes nothing.
        self.assertNotIn(layr_agents.project_group(other), self.system.groups)
        self.assertNotIn("root", sum(self.system.groups.values(), []))
        self.assertIn("root.json", errors.getvalue())
        self.assertIn("broken.json", errors.getvalue())

    def test_a_project_identity_must_be_valid(self):
        for invalid in ("", None, "x" * 129):
            with self.assertRaises(Exception):
                layr_agents.project_group(invalid)
        # The group name is a hash, so no identity reaches it as text.
        self.assertRegex(layr_agents.project_group("../etc"), r"^studio-g-[0-9a-f]{16}$")


if __name__ == "__main__":
    unittest.main()
