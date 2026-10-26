"""The recovery agent exits when an install replaced its files, so launchd runs the new code."""
import ast
import json
from pathlib import Path
import selectors
import shutil
import socket
import subprocess
import sys
import tempfile
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
            # Start from the maintained startup resource contract, then copy its local import closure.
            contract = json.loads((HERE / 'delivery-contract.json').read_text())
            pending, copied = list(contract['startupResources']), set()
            while pending:
                name = pending.pop()
                if name in copied:
                    continue
                copied.add(name)
                shutil.copy(HERE / name, app / name)
                if not name.endswith('.py'):
                    continue
                for node in ast.walk(ast.parse((HERE / name).read_text())):
                    modules = ([entry.name for entry in node.names] if isinstance(node, ast.Import)
                               else [node.module] if isinstance(node, ast.ImportFrom) and node.module else [])
                    for module in modules:
                        local = module.split('.')[0] + '.py'
                        if (HERE / local).is_file():
                            pending.append(local)
            state = root / "state"
            state.mkdir()
            config = state / "background-recovery.json"
            # No backend is installed under these resources: the agent only waits.
            config.write_text(json.dumps({
                "version": 1, "enabled": True, "stateDir": str(state), "port": free_port(),
                "python": sys.executable, "codex": "/usr/bin/true", "resources": str(root / "missing"),
                "runtimeServer": str(root / "missing/studio-runtime-server"),
                "supervisorEnabled": False}))
            agent = subprocess.Popen([
                sys.executable, "-B", "-c",
                "import sys; from pathlib import Path; sys.path.insert(0, sys.argv[1]); "
                "import recover_backend; recover_backend.run(Path(sys.argv[2]), interval=0.01)",
                str(app), str(config)], stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
            try:
                with selectors.DefaultSelector() as ready:
                    ready.register(agent.stdout, selectors.EVENT_READ)
                    self.assertTrue(ready.select(timeout=5), "the recovery agent did not report readiness")
                    self.assertIn('"backend-recovery"', agent.stdout.readline())
                self.assertIsNone(agent.poll(), "the agent must keep running while its files stay")
                (app / "install_update.py").write_text("# a newer installer\n")
                output, _ = agent.communicate(timeout=20)
            finally:
                if agent.poll() is None:
                    agent.kill()
                try:
                    agent.communicate(timeout=5)
                finally:
                    agent.stdout.close()
            self.assertEqual(agent.returncode, 0)
            self.assertIn('"agent code changed"', output)


if __name__ == "__main__":
    unittest.main()
