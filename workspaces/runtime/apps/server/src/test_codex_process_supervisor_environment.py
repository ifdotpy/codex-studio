"""A reattach waits for a just-started child's launch record to settle."""
import importlib
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from unittest.mock import patch

importlib.import_module('test_isolation').isolate_supervisor_environment()

import codex_process_supervisor as process_supervisor
from codex_process_supervisor import Journal, Supervisor

RUST_OWNER = (Path(__file__).resolve().parents[3]
              / 'packages/runtime-process/src/provider_task.rs')
HANDLE = 'account:default'

# The child re-executes itself without HOME, waits for a release file, then
# re-executes with its accepted launch again. It drops LC_CTYPE, which Python
# startup adds to os.environ, so the final record is the accepted launch.
STAGED_CHILD = r'''
import os, sys, time
source = sys.orig_argv[sys.orig_argv.index('-c') + 1]
stage = sys.argv[1] if len(sys.argv) > 1 else 'launch'
env = {key: value for key, value in os.environ.items()
       if key not in ('LC_CTYPE', '__PYVENV_LAUNCHER__')}
if stage == 'launch':
    home = env.pop('HOME')
    os.execve(sys.executable, [sys.executable, '-u', '-c', source, 'transition', home], env)
if stage == 'transition':
    open('transition', 'w').close()
    while not os.path.exists('release'):
        time.sleep(0.01)
    env['HOME'] = sys.argv[2]
    os.execve(sys.executable, [sys.executable, '-u', '-c', source, 'ready'], env)
sys.stdin.read()
'''


def launch_environment(root: Path) -> dict[str, str]:
    environment = {'PATH': '/usr/bin:/bin', 'PYTHONUNBUFFERED': '1', 'HOME': str(root / 'home'),
                   'CODEX_AGENTS_BACKEND_ID': 'earlier-backend'}
    # A Homebrew Python launcher re-executes itself when this value is absent.
    if '__CF_USER_TEXT_ENCODING' in os.environ:
        environment['__CF_USER_TEXT_ENCODING'] = os.environ['__CF_USER_TEXT_ENCODING']
    return environment


class SettledLaunchEnvironment(unittest.TestCase):
    def setUp(self) -> None:
        self.requested = {'HOME': '/home/fixture', 'PYTHONUNBUFFERED': '1'}

    def test_a_transient_record_settles_to_the_accepted_launch(self) -> None:
        accepted = {**self.requested, 'CODEX_AGENTS_BACKEND_ID': 'earlier', 'PATH': '/bin'}
        with_apple_strings = {**accepted, 'pfz': '0x1', 'stack_guard': '0x2'}
        reads = [ValueError('zeroed record during an exec'), with_apple_strings, accepted]
        with patch.object(process_supervisor, 'process_launch_environment',
                          side_effect=reads) as read:
            original = process_supervisor.settled_launch_environment(4242, self.requested)
        self.assertEqual(original, accepted)
        self.assertEqual(read.call_count, 3)

    def test_a_record_that_keeps_differing_is_returned_for_refusal(self) -> None:
        other = {**self.requested, 'HOME': '/home/other'}
        with patch.object(process_supervisor, 'RETAINED_ENVIRONMENT_SETTLE_SECONDS', 0.05), \
                patch.object(process_supervisor, 'process_launch_environment',
                             return_value=other) as read:
            original = process_supervisor.settled_launch_environment(4242, self.requested)
        self.assertEqual(original, other)
        self.assertGreaterEqual(read.call_count, 2)

    def test_a_read_that_keeps_failing_raises_its_error(self) -> None:
        with patch.object(process_supervisor, 'RETAINED_ENVIRONMENT_SETTLE_SECONDS', 0.05), \
                patch.object(process_supervisor, 'process_launch_environment',
                             side_effect=ProcessLookupError('gone')):
            with self.assertRaises(ProcessLookupError):
                process_supervisor.settled_launch_environment(4242, self.requested)

    def test_a_set_stop_event_ends_the_wait(self) -> None:
        stop = threading.Event()
        stop.set()
        with patch.object(process_supervisor, 'RETAINED_ENVIRONMENT_SETTLE_SECONDS', 60.0), \
                patch.object(process_supervisor, 'process_launch_environment',
                             side_effect=ValueError('zeroed record')):
            started = time.monotonic()
            with self.assertRaisesRegex(RuntimeError, 'stopped before the native launch settled'):
                process_supervisor.settled_launch_environment(4242, self.requested, stop)
        self.assertLess(time.monotonic() - started, 5.0)

    def test_settle_values_match_their_rust_owner(self) -> None:
        source = RUST_OWNER.read_text(encoding='utf-8')
        settle = re.search(
            r'const RETAINED_ENVIRONMENT_SETTLE: Duration = Duration::from_secs\((\d+)\);', source)
        retry = re.search(
            r'const RETAINED_ENVIRONMENT_RETRY: Duration = Duration::from_millis\((\d+)\);', source)
        assert settle is not None and retry is not None, RUST_OWNER
        self.assertEqual(process_supervisor.RETAINED_ENVIRONMENT_SETTLE_SECONDS, float(settle[1]))
        self.assertEqual(process_supervisor.RETAINED_ENVIRONMENT_RETRY_SECONDS, int(retry[1]) / 1000)


@unittest.skipUnless(sys.platform == 'darwin' or sys.platform.startswith('linux'),
                     'reads the launch record of a live child')
class ReattachDuringLaunchTransition(unittest.TestCase):
    def test_reattach_keeps_a_child_whose_launch_record_is_in_transition(self) -> None:
        directory = tempfile.TemporaryDirectory(prefix='env-settle-')
        self.addCleanup(directory.cleanup)
        root = Path(directory.name).resolve()
        (root / 'home').mkdir()
        command = [sys.executable, '-u', '-c', STAGED_CHILD]
        launch = launch_environment(root)
        child = subprocess.Popen(command, env=launch, cwd=root, stdin=subprocess.PIPE,
                                 stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        self.addCleanup(child.wait)
        self.addCleanup(child.kill)
        deadline = time.monotonic() + 30
        while not (root / 'transition').exists():
            self.assertIsNone(child.poll(), 'the staged child exited early')
            self.assertLess(time.monotonic(), deadline, 'the child did not reach its transition')
            time.sleep(0.01)
        started = process_supervisor.process_start_time(child.pid)
        journal = Journal(root)
        with journal.db() as db:
            db.execute('INSERT INTO handles(id,signature,pid,created) VALUES (?,?,?,1)',
                       (HANDLE, Supervisor.signature(command, launch, str(root)), child.pid))
            db.execute('INSERT INTO child_identities(handle,pid,pgid,start_time) VALUES (?,?,?,?)',
                       (HANDLE, child.pid, child.pid, started))
        journal.close()
        # Release the child only after the reattach has started to read.
        release = threading.Timer(0.3, lambda: (root / 'release').touch())
        release.start()
        self.addCleanup(release.cancel)
        requested = {**launch, 'CODEX_AGENTS_BACKEND_ID': 'replacement-backend'}
        original = process_supervisor.native_launch_environment(
            root, HANDLE, command, requested, str(root))
        self.assertEqual(original, launch)


if __name__ == '__main__':
    unittest.main()
