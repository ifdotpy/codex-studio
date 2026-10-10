"""Credential integration with the real runtime and its isolated fake provider."""
from __future__ import annotations

from pathlib import Path
import runpy
import sys
import tempfile
import unittest

from codex_multi_server import MultiServerService


class RuntimeAccessTests(unittest.TestCase):
    def test_actual_runtime_accessor_preserves_state_and_federation(self) -> None:
        root = Path(__file__).resolve().parents[3]
        sys.path.insert(0, str(root / "tests"))
        try:
            fixture = runpy.run_path(str(root / "tests" / "runtime-contract.py"), run_name="multi_server_runtime_fixture")
        finally:
            sys.path.pop(0)
        from codex_runtime import Runtime

        with tempfile.TemporaryDirectory(prefix="studio-access-runtime-") as directory:
            state = Path(directory)
            first = Runtime(state, fixture["FakeServer"])
            try:
                service = first.paired_access()
                self.assertIsInstance(service, MultiServerService)
                self.assertIs(first.paired_access(), service)
                server_id = service.local_server_id
                self.assertEqual(first.db_path, state / "canvas.sqlite3")
                self.assertFalse(first.federation().enabled())
                with first.read_db() as db:
                    count = db.execute("SELECT count(*) FROM runtime_access_clients").fetchone()[0]
                self.assertEqual(count, 0)
            finally:
                first.close()
            second = Runtime(state, fixture["FakeServer"])
            try:
                self.assertEqual(second.paired_access().local_server_id, server_id)
                self.assertFalse(second.federation().enabled())
            finally:
                second.close()


if __name__ == "__main__":
    unittest.main()
