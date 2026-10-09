#!/usr/bin/env python3
"""Runtime's idle WAL connection preserves per-event FULL commits."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import fcntl
from pathlib import Path
import sqlite3
import sys
import tempfile
import threading
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
import codex_runtime
from codex_runtime import Runtime, _RuntimeWalKeeper


class RuntimeWalKeeperContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)

    @staticmethod
    def wal_path(runtime):
        return Path(str(runtime.db_path) + '-wal')

    def runtime(self):
        runtime = Runtime(self.root, server_factory=object)
        self.addCleanup(runtime.close)
        return runtime

    def test_each_runtime_transaction_stays_in_wal_until_keeper_closes(self):
        runtime = self.runtime()
        wal_path = self.wal_path(runtime)
        self.assertTrue(runtime._wal_keeper.idle)

        with runtime.db() as db:
            self.assertEqual(db.execute('PRAGMA journal_mode').fetchone()[0], 'wal')
            self.assertEqual(db.execute('PRAGMA synchronous').fetchone()[0], 2)
            db.execute("INSERT INTO runtime_tasks(id,record) VALUES ('wal-test','{}')")

        self.assertTrue(wal_path.is_file())
        self.assertGreater(wal_path.stat().st_size, 0)
        reader = sqlite3.connect(runtime.db_path)
        try:
            self.assertEqual(reader.execute(
                "SELECT record FROM runtime_tasks WHERE id='wal-test'").fetchone(), ('{}',))
        finally:
            reader.close()
        self.assertTrue(wal_path.is_file())

        runtime.close()
        self.assertIsNone(runtime._wal_keeper)
        self.assertFalse(wal_path.exists())
        runtime.close()

    def test_keeper_closes_on_its_owner_thread_after_runtime_shutdown(self):
        runtime = self.runtime()
        keeper = runtime._wal_keeper
        failures = []

        def close_runtime():
            try:
                runtime.close()
            except BaseException as error:
                failures.append(error)

        closer = threading.Thread(target=close_runtime)
        closer.start()
        closer.join(5)
        self.assertFalse(closer.is_alive())
        self.assertEqual(failures, [])
        self.assertFalse(keeper.idle)
        self.assertFalse(keeper._thread.is_alive())
        self.assertTrue(runtime.lease.closed)
        runtime.close()

    def test_failed_keeper_startup_releases_runtime_lease(self):
        with patch.object(codex_runtime, '_RuntimeWalKeeper', side_effect=OSError('keeper setup failed')):
            with self.assertRaisesRegex(OSError, 'keeper setup failed'):
                Runtime(self.root, server_factory=object)

        with (self.root / 'runtime.lock').open('a+') as contender:
            fcntl.flock(contender, fcntl.LOCK_EX | fcntl.LOCK_NB)
            fcntl.flock(contender, fcntl.LOCK_UN)

    def test_schema_read_failure_closes_partial_connection(self):
        database = self.root / 'partial.sqlite'
        connection = sqlite3.connect(database)
        connection.execute('PRAGMA journal_mode=WAL').fetchone()
        connection.execute('CREATE TABLE sample(value TEXT)')
        connection.close()

        real_connect = sqlite3.connect
        state = {'closed': False, 'close_thread': None, 'connection_thread': None}

        class BrokenConnection:
            def execute(self, *_args, **_kwargs):
                raise OSError('schema read failed')

            def close(self):
                state['closed'] = True
                state['close_thread'] = threading.current_thread()
                state['connection'].close()

        def connect_on_keeper_thread(*args, **kwargs):
            state['connection_thread'] = threading.current_thread()
            state['connection'] = real_connect(*args, check_same_thread=False, **kwargs)
            return BrokenConnection()

        with patch.object(codex_runtime.sqlite3, 'connect', side_effect=connect_on_keeper_thread):
            with self.assertRaisesRegex(RuntimeError, 'Could not initialize'):
                _RuntimeWalKeeper(database)

        self.assertTrue(state['closed'])
        self.assertIs(state['close_thread'], state['connection_thread'])
        with self.assertRaisesRegex(sqlite3.ProgrammingError, 'closed database'):
            state['connection'].execute('SELECT 1')

    def test_history_start_failure_cleans_keeper_scheduler_and_lease(self):
        keepers = []
        cleaned_runtimes = []
        keeper_type = _RuntimeWalKeeper
        cleanup = Runtime._cleanup_failed_initialization
        step_started = threading.Event()
        release_step = threading.Event()
        cleanup_started = threading.Event()
        constructor_errors = []

        def create_keeper(*args, **kwargs):
            keeper = keeper_type(*args, **kwargs)
            keepers.append(keeper)
            return keeper

        def cleanup_and_record(runtime, error):
            cleaned_runtimes.append(runtime)
            cleanup_started.set()
            cleanup(runtime, error)

        def blocked_step():
            step_started.set()
            release_step.wait()

        def start_history_then_raise(runtime):
            worker = threading.Thread(target=blocked_step, daemon=True, name='analytics-history-fixture')
            runtime.analytics_history_thread = worker
            worker.start()
            if not step_started.wait(5):
                raise RuntimeError('history worker did not enter its first step')
            raise RuntimeError('history start failed after thread launch')

        def construct_runtime():
            try:
                Runtime(self.root)
            except BaseException as error:
                constructor_errors.append(error)

        # This fixture covers the history-start failure. A search migration can
        # sleep for 30 seconds when the host cannot meet its disk-space reserve.
        with patch.object(Runtime, 'search_migration_start'), \
                patch.object(codex_runtime, '_RuntimeWalKeeper', side_effect=create_keeper):
            with patch.object(Runtime, '_cleanup_failed_initialization', autospec=True,
                              side_effect=cleanup_and_record):
                with patch.object(Runtime, 'analytics_history_start', autospec=True,
                                  side_effect=start_history_then_raise):
                    constructor = threading.Thread(target=construct_runtime)
                    constructor.start()
                    try:
                        self.assertTrue(step_started.wait(5), repr(constructor_errors))
                        self.assertTrue(cleanup_started.wait(5))
                        self.assertTrue(constructor.is_alive())
                        runtime = cleaned_runtimes[0]
                        self.assertFalse(runtime.lease.closed)
                        self.assertTrue(keepers[0].idle)
                    finally:
                        release_step.set()
                        constructor.join(5)

        self.assertEqual(len(keepers), 1)
        self.assertEqual(len(cleaned_runtimes), 1)
        self.assertFalse(constructor.is_alive())
        self.assertEqual(len(constructor_errors), 1)
        self.assertRegex(str(constructor_errors[0]), 'history start failed after thread launch')
        runtime = cleaned_runtimes[0]
        self.assertFalse(runtime.scheduler.is_alive())
        self.assertFalse(runtime.analytics_history_thread.is_alive())
        self.assertFalse(keepers[0].idle)
        self.assertFalse(keepers[0]._thread.is_alive())
        with (self.root / 'runtime.lock').open('a+') as contender:
            fcntl.flock(contender, fcntl.LOCK_EX | fcntl.LOCK_NB)
            fcntl.flock(contender, fcntl.LOCK_UN)

    def test_shutdown_error_keeps_keeper_until_writers_can_drain(self):
        runtime = self.runtime()
        keeper = runtime._wal_keeper
        try:
            with patch('codex_native_tools.wait_updates', side_effect=RuntimeError('writer still active')):
                with self.assertRaisesRegex(RuntimeError, 'writer still active'):
                    runtime.close()
            self.assertTrue(keeper.idle)
            self.assertFalse(runtime.lease.closed)
            self.assertFalse(runtime._shutdown_writers_drained)
        finally:
            # The existing shutdown contract retains ownership after a failed
            # drain; release only these fixture resources to avoid a test leak.
            keeper.close()
            runtime._wal_keeper = None
            runtime._release_lease()


if __name__ == '__main__':
    unittest.main()
