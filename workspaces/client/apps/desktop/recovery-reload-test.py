"""The recovery agent exits when an install replaced its files, so launchd runs the new code."""
import json
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import unittest

HERE = Path(__file__).resolve().parent


def free_port():
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()[1]


class RecoveryReload(unittest.TestCase):
    def test_the_agent_exits_after_its_installed_files_change(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            app = root / "Resources"
            app.mkdir()
            shutil.copy(HERE / "recover_backend.py", app / "recover_backend.py")
            shutil.copy(HERE / "install_update.py", app / "install_update.py")
            state = root / "state"
            state.mkdir()
            config = state / "background-recovery.json"
            # No backend is installed under these resources: the agent only waits.
            config.write_text(json.dumps({
                "version": 1, "enabled": True, "stateDir": str(state), "port": free_port(),
                "python": sys.executable, "codex": "/usr/bin/true", "resources": str(root / "missing"),
                "supervisorEnabled": False}))
            agent = subprocess.Popen([sys.executable, "-B", str(app / "recover_backend.py"), "--config", str(config)],
                                     stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
            try:
                time.sleep(1.5)
                self.assertIsNone(agent.poll(), "the agent must keep running while its files stay")
                (app / "install_update.py").write_text("# a newer installer\n")
                output, _ = agent.communicate(timeout=20)
            finally:
                if agent.poll() is None:
                    agent.kill()
            self.assertEqual(agent.returncode, 0)
            self.assertIn('"agent code changed"', output)


if __name__ == "__main__":
    unittest.main()
