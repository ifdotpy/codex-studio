"""Guest crash detection and the VM watchdog, without a VM, a helper or user state."""
from __future__ import annotations

import importlib.util
import json
from pathlib import Path
import tempfile
import threading
import time
from types import SimpleNamespace
from typing import Any
import unittest
from unittest.mock import Mock, patch

import codex_linux_vm as vm
import codex_linux_vm_watchdog as watchdog

PANIC = ('[ 8123.456789] Kernel panic - not syncing: corrupted stack end detected inside scheduler\n'
         '[ 8123.456790] CPU: 2 PID: 97 Comm: khvcd Not tainted 6.8.0-85-generic\n')


class FakeHelper:
    """The helper's control methods: one VM, a PID per boot, a console cleared at boot."""

    def __init__(self, directory: Path) -> None:
        self.directory = directory
        self.state = 'running'
        self.pid = 1000
        self.stops = 0
        self.launches = 0
        self.healthy = True
        # A dead kernel ignores the ACPI stop: the helper stops it this many polls later.
        self.stop_polls = 0
        self.while_stopping: Any = None
        self.health_record_at_launch: bool | None = None

    def host(self, method: str, timeout: float = 5) -> dict[str, Any]:
        if method == 'host.status':
            if self.state == 'stopping':
                hook, self.while_stopping = self.while_stopping, None
                if hook is not None:
                    hook()
                self.stop_polls -= 1
                if self.stop_polls <= 0:
                    self.state = 'stopped'
            if self.state == 'stopped':
                raise vm.LinuxVMError('The Linux VM helper is unavailable: no socket')
            return {'state': self.state, 'pid': self.pid, 'error': None}
        if method == 'host.stop':
            self.stops += 1
            self.state = 'stopping' if self.stop_polls > 0 else 'stopped'
            return {'state': 'stopping'}
        raise AssertionError(method)

    def launch(self, *args: Any, **kwargs: Any) -> Mock:
        self.health_record_at_launch = (self.directory / 'guest-health.json').exists()
        self.launches += 1
        self.pid += 1
        self.state = 'running'
        self.healthy = True
        (self.directory / 'console.log').write_text('STUDIO_PROVISION_READY\n')
        process = Mock()
        process.poll.return_value = None
        return process

    def call(self, method: str, params: dict[str, Any] | None = None, **options: Any) -> dict[str, Any]:
        if method not in {'health', 'ping'}:
            raise AssertionError(method)
        if not self.healthy:
            raise vm.LinuxVMError('The Linux VM connection closed before its response.', uncertain=True)
        if method == 'ping':
            # The guest service answers an unknown method with an error from its event loop.
            raise vm.LinuxVMError('Linux guest ping: unsupported', code='invalid_params')
        return {'protocol': 1}


class Clock:
    def __init__(self) -> None:
        self.now = time.time()

    def __call__(self) -> float:
        return self.now


class WatchdogTests(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory(prefix='vm-watchdog-')
        self.addCleanup(temporary.cleanup)
        self.directory = Path(temporary.name)
        (self.directory / 'config.json').write_text('{}')
        self.client = vm.Client(self.directory)
        self.client.helper = self.directory / 'helper'
        self.client.helper.touch(mode=0o700)
        self.helper = FakeHelper(self.directory)
        self.clock = Clock()
        self.addCleanup(patch.stopall)
        patch.object(self.client, '_host', side_effect=self.helper.host).start()
        patch.object(self.client, 'call', side_effect=self.helper.call).start()
        patch.object(self.client, '_wait_guest_clock').start()
        patch('codex_linux_vm.subprocess.Popen', side_effect=self.helper.launch).start()
        patch('codex_linux_vm.platform.system', return_value='Darwin').start()
        patch('codex_linux_vm.platform.machine', return_value='arm64').start()
        patch.object(vm, '_require_space').start()
        self.ensure_service = patch('codex_host_exec.ensure_service').start()
        self.configure_guest = patch('codex_host_exec.configure_guest').start()
        patch('codex_linux_vm_share.remount_registered').start()
        self.stopped: list[dict[str, Any]] = []
        self.finished: list[dict[str, Any]] = []
        self.watchdog = watchdog.Watchdog(self.client, on_stopped=self.on_stopped,
                                          on_finished=self.finished.append, clock=self.clock)

    def on_stopped(self, record: dict[str, Any]) -> list[str]:
        self.stopped.append(dict(record))
        return ['chat-1']

    def panic(self) -> None:
        (self.directory / 'console.log').write_text('STUDIO_PROVISION_READY\n' + PANIC)

    def records(self) -> list[dict[str, Any]]:
        return list(watchdog.load(self.directory)['restarts'])

    # Detection

    def test_a_kernel_panic_behind_a_running_helper_is_a_failed_vm(self) -> None:
        self.panic()
        status = self.client.status()
        self.assertEqual(status['state'], 'failed')
        self.assertEqual(status['helperState'], 'running')
        self.assertEqual(status['error'], 'Kernel panic - not syncing: corrupted stack end detected inside scheduler')
        self.assertEqual(status['guestFailure']['kind'], 'kernel_panic')

    def test_panic_words_in_other_output_and_an_old_console_are_not_a_failure(self) -> None:
        (self.directory / 'console.log').write_text(
            'STUDIO_PROVISION_READY\nsearch: grep "Kernel panic - not syncing: x" /var/log/syslog\n')
        self.assertEqual(self.client.status()['state'], 'running')
        # A stopped helper: the console is of the last boot, and the VM is just stopped.
        self.panic()
        self.helper.state = 'stopped'
        self.assertEqual(self.client.status()['state'], 'stopped')

    def test_no_health_answer_after_a_healthy_answer_fails_after_the_window(self) -> None:
        start = self.clock.now
        self.client.record_health(self.helper.pid, None, now=start)
        for step in range(1, 6):
            self.client.record_health(self.helper.pid, 'no answer', now=start + 30 * step)
        self.assertEqual(self.client.status()['state'], 'running')  # 120 seconds without an answer
        for step in range(6, 8):
            self.client.record_health(self.helper.pid, 'no answer', now=start + 30 * step)
        status = self.client.status()
        self.assertEqual(status['state'], 'failed')
        self.assertEqual(status['guestFailure']['kind'], 'unanswered')
        self.assertIn('has not answered health for 180 seconds', status['error'])

    def test_no_answer_before_the_first_answer_or_across_a_gap_is_not_a_failure(self) -> None:
        start = self.clock.now
        # A booting or provisioning guest has not answered in this boot yet.
        for step in range(20):
            self.client.record_health(self.helper.pid, 'booting', now=start + 30 * step)
        self.assertEqual(self.client.status()['state'], 'running')
        # A long gap (Mac sleep, backend stop) starts a new span.
        self.client.record_health(self.helper.pid, None, now=start + 1000)
        self.client.record_health(self.helper.pid, 'no answer', now=start + 1030)
        for at in (1500, 1530, 1560, 1590, 1620, 1650):
            self.client.record_health(self.helper.pid, 'no answer', now=start + at)
        self.assertEqual(self.client.status()['state'], 'running')
        # The record of another boot does not apply.
        for at in (1680, 1710):
            self.client.record_health(self.helper.pid, 'no answer', now=start + at)
        self.assertEqual(self.client.status()['state'], 'failed')
        self.helper.pid += 1
        self.assertEqual(self.client.status()['state'], 'running')

    def test_any_guest_answer_proves_life_and_clears_an_unanswered_verdict(self) -> None:
        self.assertIsNone(self.client.ping())
        self.helper.healthy = False
        self.assertIn('closed before its response', str(self.client.ping()))
        start = self.clock.now
        self.client.record_health(self.helper.pid, None, now=start)
        for step in range(1, 8):
            self.client.record_health(self.helper.pid, 'no answer', now=start + 30 * step)
        self.assertEqual(self.client.status()['guestFailure']['kind'], 'unanswered')
        # The guest answers again, for example after a slow minute: no restart.
        self.helper.healthy = True
        self.clock.now = start + 240
        self.assertEqual(self.watchdog.check(), 'running')
        self.assertEqual(self.helper.stops, 0)

    # Recovery

    def test_a_failed_guest_restarts_through_the_normal_start_and_keeps_its_console(self) -> None:
        # A failing probe record that the next helper PID would inherit.
        self.client.record_health(self.helper.pid + 1, 'no answer', now=self.clock.now)
        self.panic()
        self.assertEqual(self.watchdog.check(), 'restarted')
        self.assertEqual((self.helper.stops, self.helper.launches), (1, 1))
        self.ensure_service.assert_called_once_with(self.client.state_dir)
        self.configure_guest.assert_called_once()
        status = self.client.status()
        self.assertEqual(status['state'], 'running')
        (record,) = self.records()
        self.assertEqual((record['stage'], record['outcome'], record['kind']), ('done', 'restarted', 'kernel_panic'))
        self.assertEqual(record['reason'], 'Kernel panic - not syncing: corrupted stack end detected inside scheduler')
        self.assertIn('khvcd', record['consoleTail'])
        self.assertEqual(record['chats'], ['chat-1'])
        self.assertIn('Kernel panic', (self.directory / record['consoleLog']).read_text())
        self.assertNotIn('Kernel panic', (self.directory / 'console.log').read_text())
        self.assertEqual(self.stopped[0]['stage'], 'starting')
        self.assertEqual(self.finished[0]['outcome'], 'restarted')
        self.assertNotIn('restart', self.client.intent())
        self.assertEqual(status['watchdog']['lastRestart']['outcome'], 'restarted')
        # The new boot answered through ensure_running: from now on no answer is a failure.
        health = json.loads((self.directory / 'guest-health.json').read_text())
        self.assertEqual(health['helperPid'], self.helper.pid)
        self.assertIsNotNone(health['healthyAt'])
        # A new helper can get an old PID: its probe record starts empty.
        self.assertFalse(self.helper.health_record_at_launch)

    def test_an_unanswered_guest_is_restarted_by_the_checks(self) -> None:
        self.assertEqual(self.watchdog.check(), 'running')
        self.helper.healthy = False
        results = []
        for _ in range(8):
            self.clock.now += watchdog.PROBE_INTERVAL
            results.append(self.watchdog.check())
        self.assertEqual(results[:6], ['running'] * 6)
        self.assertEqual(results[6], 'restarted')
        self.assertEqual(self.records()[0]['kind'], 'unanswered')

    def test_restarts_wait_ten_minutes_and_stop_at_the_daily_limit(self) -> None:
        start = self.clock.now
        self.panic()
        self.assertEqual(self.watchdog.check(), 'restarted')
        self.panic()
        self.clock.now = start + 60
        self.assertEqual(self.watchdog.check(), 'limited')
        self.assertEqual(self.helper.stops, 1)
        held = self.client.status()['watchdog']['held']
        self.assertEqual(held['until'], start + watchdog.MIN_RESTART_INTERVAL)
        for step in range(1, watchdog.DAILY_RESTART_LIMIT):
            self.clock.now = start + step * (watchdog.MIN_RESTART_INTERVAL + 1)
            self.panic()
            self.assertEqual(self.watchdog.check(), 'restarted')
        self.clock.now += watchdog.MIN_RESTART_INTERVAL + 1
        self.panic()
        self.assertEqual(self.watchdog.check(), 'limited')
        self.assertEqual(self.helper.stops, watchdog.DAILY_RESTART_LIMIT)
        status = self.client.status()
        self.assertEqual(status['state'], 'failed')
        self.assertEqual(status['watchdog']['held']['until'], start + 24 * 3600)
        self.assertIn('4 automatic restarts in 24 hours', status['watchdog']['held']['rule'])
        self.clock.now = start + 24 * 3600 + 1
        self.assertEqual(self.watchdog.check(), 'restarted')

    def test_a_stopped_vm_is_never_started(self) -> None:
        self.helper.state = 'stopped'
        for intent in (None, ('stopped', 'stop'), ('running', 'start')):
            if intent is not None:
                self.client._write_intent(*intent)
            self.assertEqual(self.watchdog.check(), 'stopped')
        self.assertEqual(self.helper.launches, 0)

    def test_a_stop_on_purpose_during_the_restart_keeps_the_vm_stopped(self) -> None:
        self.panic()
        # A person stops the VM between the watchdog's stop and its start.
        self.watchdog.on_stopped = lambda record: [self.client.stop()['state']]
        self.assertEqual(self.watchdog.check(), 'cancelled')
        self.assertEqual(self.helper.launches, 0)
        self.assertEqual(self.client.status()['state'], 'stopped')
        self.assertEqual(self.client.intent()['by'], 'stop')
        self.assertEqual(self.records()[0]['outcome'], 'cancelled')
        # Later checks leave it stopped.
        self.clock.now += 3600
        self.assertEqual(self.watchdog.check(), 'stopped')
        self.assertEqual(self.helper.launches, 0)

    def test_a_stop_while_a_dead_guest_stops_keeps_the_vm_stopped(self) -> None:
        self.panic()
        self.helper.stop_polls = 3
        # A person stops the VM while the helper waits to force the stop of the dead kernel.
        self.helper.while_stopping = self.client.stop
        self.assertEqual(self.watchdog.check(), 'cancelled')
        self.assertEqual(self.helper.launches, 0)
        self.assertEqual(self.client.intent()['state'], 'stopped')
        self.assertEqual(self.finished[0]['outcome'], 'cancelled')

    def test_a_restart_that_a_backend_stop_interrupted_after_its_launch_is_closed(self) -> None:
        record = {'id': 'restart-1', 'at': self.clock.now, 'kind': 'kernel_panic', 'reason': 'x',
                  'stage': 'starting', 'chats': ['chat-1']}
        (self.directory / watchdog.RECORD_FILE).write_text(json.dumps({'restarts': [record]}))
        self.client._write_intent('running', 'watchdog', 'restart-1')
        self.assertEqual(self.watchdog.check(), 'running')
        self.assertEqual(self.records()[0]['outcome'], 'restarted')
        self.assertEqual(self.finished[0]['chats'], ['chat-1'])
        self.assertNotIn('restart', self.client.intent())
        # A later stop by the guest itself does not start the VM again.
        self.helper.state = 'stopped'
        self.assertEqual(self.watchdog.check(), 'stopped')
        self.assertEqual(self.helper.launches, 0)

    def test_no_restart_while_an_upgrade_or_a_create_holds_its_lock(self) -> None:
        self.panic()
        with self.client.maintenance():
            self.assertEqual(self.watchdog.check(), 'busy')
        with self.client._lock():
            self.assertEqual(self.watchdog.check(), 'busy')
        self.assertEqual((self.helper.stops, self.records()), (0, []))
        self.assertEqual(self.watchdog.check(), 'restarted')

    def test_an_upgrade_does_not_start_during_an_automatic_restart(self) -> None:
        path = Path(vm.__file__).with_name('upgrade-linux-vm-layr.py')
        spec = importlib.util.spec_from_file_location('upgrade_linux_vm_layr', path)
        assert spec is not None and spec.loader is not None
        upgrade = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(upgrade)
        guest = Mock(side_effect=AssertionError('no guest call'))
        with self.client.maintenance(), patch.object(vm.Client, 'call', guest):
            with self.assertRaises(vm.LinuxVMError) as error:
                upgrade.upgrade(self.directory, self.client.helper, self.directory, 'upgrade-1')
        self.assertEqual(error.exception.code, 'maintenance_busy')
        self.assertFalse((self.directory / 'updates').exists())

    def test_a_restart_that_a_backend_stop_interrupted_is_finished_once(self) -> None:
        self.helper.state = 'stopped'
        record = {'id': 'restart-1', 'at': self.clock.now, 'time': 'x', 'kind': 'kernel_panic',
                  'reason': 'Kernel panic - not syncing', 'stage': 'starting', 'chats': ['chat-1']}
        (self.directory / watchdog.RECORD_FILE).write_text(json.dumps({'restarts': [record]}))
        self.client._write_intent('running', 'watchdog', 'restart-1')
        self.clock.now += 60
        self.assertEqual(self.watchdog.check(), 'restarted')
        self.assertEqual(self.helper.launches, 1)
        self.assertEqual(self.finished[0]['chats'], ['chat-1'])
        self.assertEqual(self.watchdog.check(), 'running')
        self.assertEqual(self.helper.launches, 1)

    def test_an_old_interrupted_restart_is_abandoned(self) -> None:
        self.helper.state = 'stopped'
        record = {'id': 'restart-1', 'at': self.clock.now, 'kind': 'kernel_panic', 'reason': 'x', 'stage': 'stopping'}
        (self.directory / watchdog.RECORD_FILE).write_text(json.dumps({'restarts': [record]}))
        self.client._write_intent('running', 'watchdog', 'restart-1')
        self.clock.now += watchdog.RESUME_SECONDS + 1
        self.assertEqual(self.watchdog.check(), 'abandoned')
        self.assertEqual(self.watchdog.check(), 'stopped')
        self.assertEqual(self.helper.launches, 0)
        self.assertEqual(self.records()[0]['outcome'], 'abandoned')

    def test_crash_console_copies_are_bounded(self) -> None:
        for step in range(watchdog.CRASH_LOGS_KEPT + 2):
            (self.directory / 'console.log').write_text(f'boot {step}\n')
            watchdog._keep_console(self.directory, self.clock.now + step)
        kept = sorted(path.read_text() for path in self.directory.glob('console-crash-*.log'))
        self.assertEqual(kept, [f'boot {step}\n' for step in range(2, watchdog.CRASH_LOGS_KEPT + 2)])

    def test_providers_of_the_dead_boot_end_through_the_transport_loss_path(self) -> None:
        servers = {'chat-1': Mock(), 'chat-2': Mock()}
        servers['chat-2'].fail_transport.side_effect = RuntimeError('already closed')
        runtime = SimpleNamespace(lock=threading.Lock(), linux_servers=servers,
                                  offline_linux_agents={'other'})
        record = {'id': 'restart-1', 'reason': 'Kernel panic - not syncing'}
        self.assertEqual(watchdog.drop_providers(runtime, record), ['chat-1', 'chat-2'])
        servers['chat-1'].fail_transport.assert_called_once()
        self.assertIn('outcome unknown', servers['chat-1'].fail_transport.call_args.args[0])
        self.assertEqual(runtime.offline_linux_agents, {'other', 'chat-1', 'chat-2'})

    def test_only_the_backend_that_owns_the_vm_state_starts_the_watchdog(self) -> None:
        with tempfile.TemporaryDirectory(prefix='vm-watchdog-other-') as other:
            # A test runtime: the default VM state is the user's, outside its root.
            stranger = SimpleNamespace(root=Path(other), closed=False, _linux_vm_client=self.client)
            watchdog.start(stranger, interval=3600)
            self.assertNotIn('_vm_watchdog', stranger.__dict__)
        owner = SimpleNamespace(root=self.directory.parent, closed=False, _linux_vm_client=self.client)
        watchdog.start(owner, interval=3600)
        service = owner.__dict__['_vm_watchdog']
        self.assertTrue(service.thread.is_alive())
        watchdog.close(owner)
        service.thread.join(5)
        self.assertFalse(service.thread.is_alive())


if __name__ == '__main__':
    unittest.main()
