#!/usr/bin/env python3
"""Supervisor connection reuse retains exact durable output and command receipts."""
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

from contextlib import contextmanager
import io
import json
from pathlib import Path
import sqlite3
import statistics
import sys
import tempfile
import threading
import time
from types import SimpleNamespace
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
sys.path.insert(0, str(SERVER_SOURCE_ROOT))
import codex_process_supervisor as supervisor


class FreshJournal(supervisor.Journal):
    """The previous per-call connection boundary, without a Git history dependency."""
    @contextmanager
    def db(self):
        db = sqlite3.connect(self.path, timeout=5)
        db.row_factory = sqlite3.Row
        try:
            db.execute("PRAGMA synchronous=FULL")
            db.execute("PRAGMA wal_autocheckpoint=100")
            with db:
                yield db
        finally:
            db.close()


class Connections:
    def __init__(self):
        self.opened = []
        self.closed = []
        self.sql = []
        self.fail_commit = False
        self.fail_rollback = False
        self.fail_configuration = False
        original = sqlite3.connect
        owner = self

        class Connection(sqlite3.Connection):
            def execute(self, sql, *args, **kwargs):
                if owner.fail_configuration and sql == "PRAGMA synchronous=FULL":
                    raise sqlite3.OperationalError("injected configuration failure")
                return super().execute(sql, *args, **kwargs)

            def __exit__(self, *args):
                result = super().__exit__(*args)
                if owner.fail_commit and args[0] is None:
                    owner.fail_commit = False
                    error = sqlite3.OperationalError("injected lost commit response")
                    error.sqlite_errorcode = sqlite3.SQLITE_IOERR
                    raise error
                if owner.fail_rollback and args[0] is not None:
                    owner.fail_rollback = False
                    raise sqlite3.OperationalError("injected rollback failure")
                return result

            def close(self):
                if self not in owner.closed:
                    owner.closed.append(self)
                return super().close()

        def connect(*args, **kwargs):
            kwargs["factory"] = Connection
            db = original(*args, **kwargs)
            self.opened.append(db)
            db.set_trace_callback(self.sql.append)
            return db
        self.patch = patch.object(supervisor.sqlite3, "connect", connect)

    def __enter__(self):
        self.patch.start()
        return self

    def __exit__(self, *args):
        self.patch.stop()


def close(journal):
    closer = getattr(journal, "close", None)
    if closer:
        closer()


def workload(journal_type, directory, count):
    with Connections() as connections:
        started = time.process_time()
        server = supervisor.Supervisor(directory)
        journal = server.journal = journal_type(directory)
        child = supervisor.Child.__new__(supervisor.Child)
        child.handle = "account:private-journal"
        child.generation = 1
        child.lock = threading.RLock()
        child.append_lock = threading.Lock()
        child.output = threading.Condition(child.lock)
        child.stopping = threading.Event()
        child.paused = threading.Event()
        child.persistence_errors = {}
        child.reader = SimpleNamespace(is_alive=lambda: False)
        child.stderr = SimpleNamespace(is_alive=lambda: False)
        child.process = SimpleNamespace(supervisor=server, stdin=io.StringIO(), poll=lambda: None)
        server.children[child.handle] = child
        receipts = []
        try:
            with journal.db() as db:
                db.execute("INSERT INTO handles(id,signature,pid,created,generation) VALUES(?,?,?,?,1)",
                           (child.handle, "exact-signature", 42, 1))
            message = {"id": 7, "method": "command/exec", "params": {"fixture": True}}
            with patch.object(supervisor.time, "time", return_value=1000):
                accepted = child.write("monitor:exact-input", 7, message)
                duplicate = child.write("monitor:exact-input", 7, message)
                for index in range(count):
                    frame = ({"id": 1, "result": {"userAgent": "private-model"}} if index == 0 else
                             {"method": "item/agentMessage/delta", "params": {"delta": str(index)}})
                    child.append("stdout", frame)
                    event = server.handle({"action": "next", "handle": child.handle, "cursor": index})
                    receipts.append(event)
                    receipts.append(server.handle({"action": "ack", "handle": child.handle,
                                                   "sequence": index + 1}))
                receipts.append(server.handle({"action": "operationStatus", "handle": child.handle,
                                               "operationId": "monitor:exact-input"}))
            accepted.pop("durableMs")
            duplicate.pop("durableMs")
            with journal.db() as db:
                state = dict(db.execute("SELECT * FROM handles").fetchone())
                remaining = db.execute("SELECT COUNT(*) FROM events").fetchone()[0]
                operations = [dict(row) for row in db.execute("SELECT * FROM operations")]
            result = {"accepted": accepted, "duplicate": duplicate, "receipts": receipts,
                      "state": state, "remainingEvents": remaining, "operations": operations,
                      "nativeWrites": child.process.stdin.getvalue()}
        finally:
            child.stopping.set()
            close(journal)
        metrics = {"connects": len(connections.opened), "closes": len(connections.closed),
                   "cpuMs": round((time.process_time() - started) * 1000, 3),
                   "fullPragmas": sum(sql == "PRAGMA synchronous=FULL" for sql in connections.sql),
                   "checkpointPragmas": sum(sql == "PRAGMA wal_autocheckpoint=100"
                                            for sql in connections.sql)}
        return result, metrics


class JournalConnectionReuse(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="studio-journal-reuse-")
        self.directory = Path(self.tmp.name)
        self.journals = []
        self.releases = []
        self.threads = []

    def tearDown(self):
        for release in self.releases:
            release.set()
        for thread in self.threads:
            thread.join(2)
        for journal in self.journals:
            close(journal)
        self.tmp.cleanup()

    def journal(self):
        journal = supervisor.Journal(self.directory)
        self.journals.append(journal)
        return journal

    def test_real_append_poll_ack_and_duplicate_input_keep_identical_receipts(self):
        old, before = workload(FreshJournal, self.directory / "before", 12)
        new, after = workload(supervisor.Journal, self.directory / "after", 12)
        self.assertEqual(new, old)
        self.assertEqual(after["connects"], 1)
        self.assertEqual(after["closes"], after["connects"])
        self.assertGreater(before["connects"], 36)
        self.assertEqual(before["closes"], before["connects"])
        self.assertEqual(after["checkpointPragmas"], 1)
        self.assertEqual(new["remainingEvents"], 0)
        self.assertEqual(new["state"]["sequence"], 12)
        self.assertEqual(new["state"]["acknowledged"], 12)
        self.assertEqual(len(new["nativeWrites"].splitlines()), 1)
        self.assertTrue(new["accepted"]["accepted"])
        self.assertTrue(new["duplicate"]["duplicate"])

    def test_connections_keep_full_sync_checkpoint_and_no_open_transaction(self):
        journal = self.journal()
        for _ in range(5):
            with journal.db() as db:
                self.assertEqual(db.execute("PRAGMA synchronous").fetchone()[0], 2)
                self.assertEqual(db.execute("PRAGMA wal_autocheckpoint").fetchone()[0], 100)
                self.assertFalse(db.in_transaction)

    def test_same_connection_moves_between_threads_with_exclusive_leases(self):
        with Connections() as connections:
            journal = self.journal()
            seen, errors = [], []
            def read():
                try:
                    with journal.db() as db:
                        seen.append(db)
                        self.assertEqual(db.execute("SELECT COUNT(*) FROM handles").fetchone()[0], 0)
                except BaseException as error:
                    errors.append(error)
            for _ in range(3):
                thread = threading.Thread(target=read)
                thread.start()
                thread.join(2)
                self.assertFalse(thread.is_alive())
            self.assertEqual(errors, [])
            self.assertEqual(len({id(db) for db in seen}), 1)
            self.assertEqual(len(connections.opened), 1)

    def test_extra_lease_does_not_wait_and_idle_pool_is_bounded(self):
        with Connections() as connections:
            journal = self.journal()
            entered, release, acquired = threading.Event(), threading.Event(), threading.Event()
            self.releases.append(release)
            active, identities, errors = set(), [], []
            guard = threading.Lock()
            def hold(wait):
                try:
                    with journal.db() as db:
                        with guard:
                            self.assertNotIn(id(db), active)
                            active.add(id(db))
                            identities.append(id(db))
                            if len(active) == 4:
                                entered.set()
                        if wait:
                            if not release.wait(2):
                                raise RuntimeError("The fixture lease did not release")
                        else:
                            acquired.set()
                        with guard:
                            active.remove(id(db))
                except BaseException as error:
                    errors.append(error)
            for _ in range(4):
                thread = threading.Thread(target=hold, args=(True,))
                self.threads.append(thread)
                thread.start()
            self.assertTrue(entered.wait(1))
            waiter = threading.Thread(target=hold, args=(False,))
            self.threads.append(waiter)
            waiter.start()
            self.assertTrue(acquired.wait(1), "Busy leases must not delay another caller")
            self.assertEqual(len(connections.opened), 5)
            release.set()
            self.assertTrue(acquired.wait(1))
            for thread in self.threads:
                thread.join(2)
            self.assertEqual(errors, [])
            self.assertEqual(len(connections.opened), 5)
            self.assertEqual(len(connections.closed), 1)
            close(journal)
            self.assertEqual(len(connections.closed), 5)

    def test_callback_failure_rolls_back_and_discards_the_connection(self):
        with Connections() as connections:
            journal = self.journal()
            with self.assertRaisesRegex(ValueError, "fixture failure"):
                with journal.db() as db:
                    db.execute("INSERT INTO supervisor_state VALUES('failed','not-durable')")
                    raise ValueError("fixture failure")
            self.assertEqual(len(connections.closed), 1)
            with journal.db() as db:
                self.assertEqual(db.execute("SELECT COUNT(*) FROM supervisor_state").fetchone()[0], 0)
                self.assertFalse(db.in_transaction)
            self.assertEqual(len(connections.opened), 2)

    def test_lost_commit_confirmation_is_durable_but_connection_is_discarded(self):
        with Connections() as connections:
            journal = self.journal()
            connections.fail_commit = True
            with self.assertRaisesRegex(sqlite3.OperationalError, "lost commit response"):
                with journal.db() as db:
                    db.execute("INSERT INTO supervisor_state VALUES('exact','durable')")
            self.assertEqual(len(connections.closed), 1)
            with journal.db() as db:
                self.assertEqual(db.execute("SELECT value FROM supervisor_state WHERE key='exact'").fetchone()[0],
                                 "durable")
            self.assertEqual(len(connections.opened), 2)

    def test_rollback_error_does_not_return_the_connection(self):
        with Connections() as connections:
            journal = self.journal()
            connections.fail_rollback = True
            with self.assertRaisesRegex(sqlite3.OperationalError, "rollback failure"):
                with journal.db() as db:
                    db.execute("INSERT INTO supervisor_state VALUES('failed','not-durable')")
                    raise ValueError("fixture failure")
            with journal.db() as db:
                self.assertEqual(db.execute("SELECT COUNT(*) FROM supervisor_state").fetchone()[0], 0)
            self.assertEqual(len(connections.opened), 2)
            self.assertEqual(len(connections.closed), 1)

    def test_close_preserves_active_commit_and_closes_on_return(self):
        with Connections() as connections:
            journal = self.journal()
            with journal.db() as db:
                db.execute("INSERT INTO supervisor_state VALUES('before-close','durable')")
                journal.close()
                self.assertEqual(connections.closed, [])
                with self.assertRaisesRegex(RuntimeError, "journal is closed"):
                    with journal.db():
                        self.fail("A closed journal must reject leases")
            self.assertEqual(len(connections.closed), 1)
            journal.close()
        db = sqlite3.connect(journal.path)
        try:
            self.assertEqual(db.execute("SELECT value FROM supervisor_state").fetchone()[0], "durable")
        finally:
            db.close()

    def test_failed_connection_configuration_releases_reserved_pool_capacity(self):
        with Connections() as connections:
            journal = self.journal()
            with self.assertRaises(ValueError):
                with journal.db():
                    raise ValueError("discard cached connection")
            connections.fail_configuration = True
            with self.assertRaisesRegex(sqlite3.OperationalError, "configuration failure"):
                with journal.db():
                    self.fail("An unconfigured connection must not become available")
            connections.fail_configuration = False
            with journal.db() as db:
                self.assertEqual(db.execute("PRAGMA synchronous").fetchone()[0], 2)
            self.assertEqual(len(connections.opened), 3)
            self.assertEqual(len(connections.closed), 2)

    def test_baseexceptions_close_connections_and_rollback(self):
        with Connections() as connections:
            journal = self.journal()
            for error in (KeyboardInterrupt, GeneratorExit):
                with self.assertRaises(error):
                    with journal.db() as db:
                        db.execute("INSERT INTO supervisor_state VALUES('interrupted','not-durable')")
                        raise error()
            self.assertEqual(len(connections.closed), 2)
            with journal.db() as db:
                self.assertEqual(db.execute("SELECT COUNT(*) FROM supervisor_state").fetchone()[0], 0)
                self.assertFalse(db.in_transaction)

    def test_schema_change_is_visible_on_a_reused_connection(self):
        journal = self.journal()
        with journal.db() as first:
            self.assertEqual(first.execute("SELECT * FROM supervisor_state").description[0][0], "key")
        external = sqlite3.connect(journal.path)
        try:
            external.execute("ALTER TABLE supervisor_state ADD COLUMN fixture INTEGER")
            external.execute("INSERT INTO supervisor_state VALUES('new-schema','durable',7)")
            external.commit()
        finally:
            external.close()
        with journal.db() as second:
            self.assertIs(second, first)
            self.assertEqual(tuple(second.execute("SELECT * FROM supervisor_state").fetchone()),
                             ("new-schema", "durable", 7))

    def test_ack_with_lost_response_can_retry_the_exact_cursor(self):
        journal = self.journal()
        server = supervisor.Supervisor(self.directory)
        server.journal = journal
        handle = "account:private-ack"
        server.children[handle] = SimpleNamespace(paused=threading.Event())
        with journal.db() as db:
            db.execute("INSERT INTO handles(id,signature,pid,created,generation,sequence) VALUES(?,?,?,?,1,1)",
                       (handle, "exact", 42, 1))
            db.execute("INSERT INTO events VALUES(?,1,'stdout','{}',2,1)", (handle,))
            db.execute("INSERT INTO operations VALUES(?,?,?,?,?,1,'{}',1)",
                       (handle, "monitor:exact", "exact", 1, 1))
        request = {"action": "ack", "handle": handle, "sequence": 1}
        def response_lost():
            server.handle(request)
            raise ConnectionError("The saved ACK response was lost")
        with self.assertRaises(ConnectionError):
            response_lost()
        self.assertEqual(server.handle(request), {"acknowledged": 1})
        self.assertEqual(server.handle({"action": "operationStatus", "handle": handle,
                                       "operationId": "monitor:exact"})["response"], {})
        with journal.db() as db:
            self.assertEqual(db.execute("SELECT COUNT(*) FROM events").fetchone()[0], 0)
            self.assertEqual(tuple(db.execute("SELECT sequence,acknowledged FROM handles").fetchone()), (1, 1))


def benchmark(output):
    with tempfile.TemporaryDirectory(prefix="studio-journal-benchmark-") as directory:
        results, receipts = {}, {}
        for label, journal_type in (("before", FreshJournal), ("after", supervisor.Journal)):
            runs = []
            for index in range(5):
                receipt, metrics = workload(journal_type, Path(directory) / (label + str(index)), 128)
                runs.append(metrics)
                receipts[label] = receipt
            results[label] = {"runs": runs, "medianCpuMs": statistics.median(run["cpuMs"] for run in runs)}
        if receipts["before"] != receipts["after"]:
            raise AssertionError("The benchmark changed exact durable receipts")
        results.update(events=128, runsPerVariant=5, identicalReceipts=True)
        output = Path(output)
        output.parent.mkdir(parents=True, exist_ok=True)
        output.write_text(json.dumps(results, indent=2) + "\n")
        print(json.dumps(results))


if __name__ == "__main__":
    if len(sys.argv) == 3 and sys.argv[1] == "--benchmark":
        benchmark(sys.argv[2])
    else:
        unittest.main()
