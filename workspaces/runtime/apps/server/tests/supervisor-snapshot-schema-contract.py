#!/usr/bin/env python3
"""A backend update must read the handle table of a supervisor that still runs the old schema."""

# Support direct execution without runner-provided PYTHONPATH.
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import sqlite3
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(SERVER_SOURCE_ROOT))
from codex_process_supervisor import supervisor_launch_snapshot  # noqa: E402


def make_db(root: Path, *, job_columns: bool) -> None:
    db = sqlite3.connect(root / "supervisor.sqlite3")
    db.execute("CREATE TABLE handles(id TEXT PRIMARY KEY, signature TEXT, pid INTEGER, generation INTEGER,"
               " closed_at REAL, init_result TEXT, sequence INTEGER, acknowledged INTEGER)")
    extra = ", job_name TEXT NOT NULL DEFAULT '', job_kill_on_close INTEGER NOT NULL DEFAULT 0" if job_columns else ""
    db.execute("CREATE TABLE child_identities(handle TEXT PRIMARY KEY, pid INTEGER NOT NULL,"
               " pgid INTEGER NOT NULL, start_time TEXT NOT NULL" + extra + ")")
    db.execute("INSERT INTO handles VALUES('h','sig',10,1,NULL,'{}',3,2)")
    if job_columns:
        db.execute("INSERT INTO child_identities VALUES('h',10,10,'t','job-1',1)")
    else:
        db.execute("INSERT INTO child_identities VALUES('h',10,10,'t')")
    db.commit()
    db.close()


class SnapshotSchema(unittest.TestCase):
    def test_old_supervisor_schema_reads_without_job_columns(self) -> None:
        with tempfile.TemporaryDirectory() as folder:
            make_db(Path(folder), job_columns=False)
            row = supervisor_launch_snapshot(folder, "h")
            self.assertIsNotNone(row)
            assert row is not None
            self.assertEqual((row["pid"], row["identity_pid"], row["start_time"]), (10, 10, "t"))
            self.assertEqual((row["job_name"], row["job_kill_on_close"]), ("", 0))

    def test_current_schema_reads_job_columns(self) -> None:
        with tempfile.TemporaryDirectory() as folder:
            make_db(Path(folder), job_columns=True)
            row = supervisor_launch_snapshot(folder, "h")
            assert row is not None
            self.assertEqual((row["job_name"], row["job_kill_on_close"]), ("job-1", 1))


if __name__ == "__main__":
    unittest.main()
