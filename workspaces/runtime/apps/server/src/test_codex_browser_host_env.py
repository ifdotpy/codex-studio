"""The environment that sends browser links of host chats to Studio (docs/vm-browser-bridge.md,
section 6.4): the launcher links for tools that search PATH, and the per-thread Codex overrides."""
from __future__ import annotations

import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

from codex_browser_host import PATH_NAMES, publish, session_environment, thread_environment


class HostEnvironment(unittest.TestCase):
    def setUp(self) -> None:
        # A short path: tools and Unix sockets reject long ones.
        self.root = Path(tempfile.mkdtemp(prefix="sb-", dir="/tmp"))

    def tearDown(self) -> None:
        shutil.rmtree(self.root)

    def test_session_environment_links_the_launcher_for_tools_that_search_path(self) -> None:
        environment = session_environment(self.root)
        folder = self.root / "browser-host"
        self.assertEqual(environment["STUDIO_BROWSER_PATH"], str(folder / "bin"))
        for name in PATH_NAMES:
            link = folder / "bin" / name
            self.assertTrue(link.is_symlink(), name)
            self.assertEqual(link.resolve(), (folder / "studio-browser").resolve())
        # A second call keeps the same links.
        self.assertEqual(session_environment(self.root), environment)
        # The link runs the launcher: with no socket the shim reports that Studio is absent.
        result = subprocess.run([str(folder / "bin" / "xdg-open"), "https://id.example.com/"],
                                capture_output=True, text=True, timeout=20,
                                env={**os.environ, "STUDIO_BROWSER_SOCKET": str(folder / "none.sock"),
                                     "STUDIO_BROWSER_SESSION": "s"})
        self.assertEqual(result.returncode, 1)
        self.assertIn("Studio is not available here", result.stderr)

    def test_publish_gives_the_claude_bridge_the_link_folder(self) -> None:
        bridge = self.root / "claude"
        bridge.mkdir()
        publish(bridge, self.root)
        value = json.loads((bridge / "studio-browser.json").read_text())
        self.assertEqual(value["STUDIO_BROWSER_PATH"], str(self.root / "browser-host" / "bin"))

    def test_thread_environment_sets_the_three_names_only_once_the_launcher_exists(self) -> None:
        self.assertEqual(thread_environment(self.root, "agent-1"), {})
        session_environment(self.root)
        folder = self.root / "browser-host"
        self.assertEqual(thread_environment(self.root, "agent-1"), {
            "shell_environment_policy.set.BROWSER": str(folder / "studio-browser"),
            "shell_environment_policy.set.STUDIO_BROWSER_SOCKET": str(folder / "open.sock"),
            "shell_environment_policy.set.STUDIO_BROWSER_SESSION": "agent-1",
        })


if __name__ == "__main__":
    unittest.main()
