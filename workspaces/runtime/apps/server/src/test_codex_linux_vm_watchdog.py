"""Guest crash detection and the VM watchdog, without a VM, a helper or user state."""
from __future__ import annotations

import importlib.util
import json
import socket
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
        self.full = False
        # A dead kernel ignores the ACPI stop: the helper stops it this many polls later.
        self.stop_polls = 0
        self.while_stopping: Any = None
        self.health_record_at_launch: bool | None = None
        # layr.health through the guest service and the root broker: ready, unavailable or down.
        self.layr = 'ready'
        self.layr_calls = 0

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
        if method == 'layr.health':
            self.layr_calls += 1
            if self.layr == 'down':
                raise vm.LinuxVMError('The layr admin channel is unavailable')
            return {'state': self.layr}
        if method != 'health':
            raise AssertionError(method)
        if not self.healthy:
            raise vm.LinuxVMError('The Linux VM connection closed before its response.', uncertain=True)
        return {'protocol': 1}

    def ping(self, timeout: float = 15) -> str | None:
        if self.full:
            return vm.BUSY  # The guest service has 32 clients and closes a new one at once.
        return None if self.healthy else 'The guest gave no answer in 15 seconds.'


class GuestSocket:
    """One connection to guest.sock, handled the way the guest service or the helper does."""

    def __init__(self, directory: Path, mode: str) -> None:
        self.listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.listener.bind(str(directory / 'guest.sock'))
        self.listener.listen(1)
        self.listener.settimeout(5)
        self.mode = mode
        self.thread = threading.Thread(target=self.run)
        self.thread.start()

    def run(self) -> None:
        try:
            channel, _ = self.listener.accept()
        except OSError:
            return
        with channel:
            if self.mode == 'full':
                channel.makefile('rb').readline()
                return  # Drain ping before the full service closes without a response byte.
            line = channel.makefile('rb').readline()
            if self.mode == 'answer':
                request = json.loads(line)
                channel.sendall(json.dumps({'id': request['id'], 'error': {
                    'code': 'invalid_request', 'message': 'The method is not supported'}}).encode() + b'\n')
            elif self.mode == 'late-close':
                time.sleep(0.3)  # the helper's connect timer for a kernel that never accepts
            else:
                time.sleep(0.5)

    def finish(self) -> None:
        self.thread.join(5)
        self.listener.close()


class PingTests(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory(prefix='vm-ping-')
        self.addCleanup(temporary.cleanup)
        self.directory = Path(temporary.name)
        self.client = vm.Client(self.directory)

    def ping(self, mode: str, timeout: float = 2) -> str | None:
        guest = GuestSocket(self.directory, mode)
        try:
            return self.client.ping(timeout)
        finally:
            guest.finish()
            (self.directory / 'guest.sock').unlink()

    def test_any_byte_proves_the_guest_and_a_fast_empty_close_is_busy(self) -> None:
        self.assertIsNone(self.ping('answer'))
        self.assertEqual(self.ping('full'), vm.BUSY)

    def test_silence_or_a_late_empty_close_is_no_answer(self) -> None:
        self.assertIn('no answer in 0.2 seconds', str(self.ping('silent', timeout=0.2)))
        with patch('codex_linux_vm.BUSY_CLOSE_SECONDS', 0.1):
            self.assertIn('closed without an answer', str(self.ping('late-close')))
        self.assertIn('connection failed', str(self.client.ping(0.2)))


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
        patch.object(self.client, 'ping', side_effect=self.helper.ping).start()
        patch.object(self.client, '_wait_guest_clock').start()
        patch('codex_linux_vm.subprocess.Popen', side_effect=self.helper.launch).start()
        patch('codex_linux_vm.platform.system', return_value='Darwin').start()
        patch('codex_linux_vm.platform.machine', return_value='arm64').start()
        patch.object(vm, '_require_space').start()
        # Every intent of a test is from the current Mac boot unless a test says otherwise.
        patch.object(watchdog, 'host_boot_time', return_value=0.0).start()
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

    def assert_dead(self, line: str, kind: str, reason: str) -> None:
        (self.directory / 'console.log').write_text(
            'STUDIO_PROVISION_READY\n[  412.100000] BTRFS info (device vdb): first mount of filesystem\n' + line)
        status = self.client.status()
        self.assertEqual((status['state'], status['guestFailure']['kind']), ('failed', kind))
        self.assertEqual(status['error'], reason)
        self.assertEqual(self.watchdog.check(), 'restarted')
        self.assertEqual(self.records()[0]['reason'], reason)

    def test_a_kernel_oops_is_a_proven_death(self) -> None:
        # 2026-10-13, igor-mbp: guest memory zeroed under host memory pressure.
        self.assert_dead(
            '[ 9001.120000] Internal error: Oops - Undefined instruction: 0000000002000000 [#1] SMP\n'
            '[ 9001.120100] pc : autofs_show_options+0x0/0x240 [autofs4]\n'
            '[ 9001.120200] Code: ???????? ???????? 00000000 00000000 (00000000)\n',
            'kernel_error', 'Internal error: Oops - Undefined instruction: 0000000002000000 [#1] SMP')

    def test_unable_to_handle_kernel_is_a_proven_death(self) -> None:
        self.assert_dead(
            '[  77.000001] Unable to handle kernel paging request at virtual address dead000000000108\n',
            'kernel_error', 'Unable to handle kernel paging request at virtual address dead000000000108')

    def test_a_kernel_bug_is_a_proven_death(self) -> None:
        self.assert_dead('[  88.500000] BUG: Bad page state in process btrfs  pfn:12345\n',
                         'kernel_error', 'BUG: Bad page state in process btrfs  pfn:12345')

    def test_a_kernel_general_protection_fault_is_a_proven_death(self) -> None:
        self.assert_dead('[  99.000000] general protection fault: 0000 [#1] SMP\n',
                         'kernel_error', 'general protection fault: 0000 [#1] SMP')

    def test_ordinary_kernel_and_service_lines_are_not_a_death(self) -> None:
        (self.directory / 'console.log').write_text('\n'.join([
            'STUDIO_PROVISION_READY',
            '[  412.100000] BTRFS info (device vdb): first mount of filesystem 1234',
            '[  412.200000] BTRFS info (device vdb): using crc32c (crc32c-generic) checksum algorithm',
            '[  500.000000] audit: type=1400 audit(1760000000.000:42): apparmor="STATUS"',
            # A stall that can pass under host load, not a kernel error.
            '[  600.000000] watchdog: BUG: soft lockup - CPU#2 stuck for 23s! [kworker/2:1H:97]',
            '[  601.000000] rcu: INFO: rcu_sched detected stalls on CPUs/tasks:',
            # A user process fault, not the kernel.
            '[  700.000000] traps: python3[4242] general protection fault ip:7f00 sp:7ffd error:0 in libc.so.6',
            'systemd[1]: Starting codex-studio-guest.service...',
            'grep "Internal error: Oops" /var/log/kern.log',
            '']))
        self.assertEqual(self.client.status()['state'], 'running')
        self.assertEqual(self.watchdog.check(), 'running')
        self.assertEqual(self.helper.stops, 0)

    def test_panic_words_in_other_output_and_an_old_console_are_not_a_failure(self) -> None:
        (self.directory / 'console.log').write_text(
            'STUDIO_PROVISION_READY\nsearch: grep "Kernel panic - not syncing: x" /var/log/syslog\n')
        self.assertEqual(self.client.status()['state'], 'running')
        # A stopped helper: the console is of the last boot, and the VM is just stopped.
        self.panic()
        self.helper.state = 'stopped'
        self.assertEqual(self.client.status()['state'], 'stopped')

    def test_no_answer_after_an_answer_is_unresponsive_after_the_window(self) -> None:
        start = self.clock.now
        self.client.record_health(self.helper.pid, None, now=start)
        for step in range(1, 6):
            self.client.record_health(self.helper.pid, 'no answer', now=start + 30 * step)
        self.assertEqual(self.client.status()['state'], 'running')  # 120 seconds without an answer
        for step in range(6, 8):
            self.client.record_health(self.helper.pid, 'no answer', now=start + 30 * step)
        status = self.client.status()
        self.assertEqual(status['state'], 'unresponsive')
        self.assertEqual(status['guestFailure']['kind'], 'unanswered')
        self.assertIn('has not answered for 180 seconds', status['error'])

    def check_every(self, seconds: float, count: int) -> list[str]:
        results = []
        for _ in range(count):
            results.append(self.watchdog.check())
            self.clock.now += seconds
        return results

    def test_a_layr_broker_outage_behind_an_answering_guest_is_shown_not_restarted(self) -> None:
        # 2026-10-14: the guest answered every ping, the layr admin channel was unavailable,
        # and all VM chats paused while the status said running and healthy.
        changes: list[bool] = []
        self.watchdog.on_layr_changed = lambda: changes.append(
            self.client.layr_fault(self.helper.pid, now=self.clock.now) is not None)
        self.helper.layr = 'down'
        self.assertEqual(self.check_every(30, 9), ['running'] * 9)
        status = self.client.status()
        self.assertEqual(status['state'], 'running')  # its providers keep running
        self.assertEqual(status['layrFault']['kind'], 'layr_unavailable')
        self.assertIn('LAYR in the VM has not answered for 240 seconds', status['error'])
        self.assertIn('layr admin channel is unavailable', status['error'])
        self.assertEqual((self.helper.stops, self.helper.launches, self.records()), (0, 0, []))
        self.assertEqual(self.helper.layr_calls, 5)  # once per minute, not at every 30-second ping
        self.assertEqual(changes, [True])
        self.helper.layr = 'ready'
        self.check_every(30, 2)
        self.assertNotIn('layrFault', self.client.status())
        self.assertEqual(changes, [True, False])

    def test_a_short_layr_outage_or_an_unavailable_answer_counts_only_with_time(self) -> None:
        self.helper.layr = 'down'
        self.check_every(60, 2)  # two failures over 60 seconds: one unit restart is not a fault
        self.assertNotIn('layrFault', self.client.status())
        self.helper.layr = 'ready'
        self.check_every(60, 1)
        self.helper.layr = 'unavailable'  # layr.health answers, but the daemon does not
        self.check_every(60, 2)
        self.assertNotIn('layrFault', self.client.status())
        self.check_every(60, 1)
        self.assertIn('layr state unavailable', self.client.status()['error'])

    def test_no_layr_probe_without_a_guest_answer_and_an_old_fault_is_not_shown(self) -> None:
        self.helper.healthy = False
        self.check_every(60, 4)
        self.assertEqual(self.helper.layr_calls, 0)
        self.helper.healthy, self.helper.layr = True, 'down'
        self.check_every(60, 4)
        self.assertIsNotNone(vm.layr_fault(self.directory, now=self.clock.now))
        # A stopped VM or backend probes no more: a record of that age shows no fault.
        self.assertIsNone(vm.layr_fault(self.directory, now=self.clock.now + vm.LAYR_FAULT_FRESH_SECONDS + 61))
        # A new helper (a new boot) never shows the fault of the old one.
        self.assertIsNone(vm.layr_fault(self.directory, self.helper.pid + 1, now=self.clock.now))

    def test_no_answer_before_the_first_answer_or_across_a_gap_is_not_unresponsive(self) -> None:
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
        self.assertEqual(self.client.status()['state'], 'unresponsive')
        self.helper.pid += 1
        self.assertEqual(self.client.status()['state'], 'running')

    def test_a_busy_close_is_neither_an_answer_nor_a_failure(self) -> None:
        start = self.clock.now
        self.client.record_health(self.helper.pid, None, now=start)
        self.client.record_health(self.helper.pid, 'no answer', now=start + 30)
        for step in range(2, 20):
            self.client.record_health(self.helper.pid, vm.BUSY, now=start + 30 * step)
        self.assertEqual(self.client.status()['state'], 'running')

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

    def test_an_unanswered_guest_is_shown_and_recorded_but_never_restarted(self) -> None:
        self.assertEqual(self.watchdog.check(), 'running')
        self.helper.healthy = False
        results = []
        for _ in range(30):
            self.clock.now += watchdog.PROBE_INTERVAL
            results.append(self.watchdog.check())
        self.assertEqual(results[:6], ['running'] * 6)
        self.assertEqual(set(results[6:]), {'unresponsive'})
        self.assertEqual((self.helper.stops, self.records()), (0, []))
        status = self.client.status()
        self.assertEqual(status['state'], 'unresponsive')
        episode = status['watchdog']['lastUnresponsive']
        self.assertIn('has not answered for 180 seconds', episode['reason'])
        self.assertIsNone(episode.get('endedAt'))
        self.assertEqual(len(watchdog.load(self.directory)['unresponsive']), 1)
        # One answer clears the state and ends the episode.
        self.helper.healthy = True
        self.clock.now += watchdog.PROBE_INTERVAL
        self.assertEqual(self.watchdog.check(), 'running')
        self.assertIsNotNone(self.client.status()['watchdog']['lastUnresponsive']['endedAt'])
        self.assertEqual(self.helper.stops, 0)

    def test_a_full_guest_is_not_a_restart(self) -> None:
        self.assertEqual(self.watchdog.check(), 'running')
        # Agents hold 32 connections: every probe is closed at once, without a byte.
        self.helper.full = True
        results = []
        for _ in range(40):
            self.clock.now += watchdog.PROBE_INTERVAL
            results.append(self.watchdog.check())
        self.assertEqual(set(results), {'running'})
        self.assertEqual((self.helper.stops, self.helper.launches, self.records()), (0, 0, []))
        # A panic in the same boot is a proven death: it still restarts.
        self.panic()
        self.assertEqual(self.watchdog.check(), 'restarted')
        self.assertEqual(self.helper.stops, 1)

    def test_restarts_wait_ten_minutes_and_stop_at_the_daily_limit(self) -> None:
        start = self.clock.now
        self.panic()
        self.assertEqual(self.watchdog.check(), 'restarted')
        self.panic()
        self.clock.now = start + 60
        self.assertEqual(self.watchdog.check(), 'limited')
        self.assertEqual(self.helper.stops, 1)
        held = self.client.status()['watchdog']['held']
        self.assertEqual(held['until'], start + 600)
        # Restarts two to four, each a little more than ten minutes after the last one.
        for at in (601, 1202, 1803):
            self.clock.now = start + at
            self.panic()
            self.assertEqual(self.watchdog.check(), 'restarted')
        self.clock.now = start + 2404
        self.panic()
        self.assertEqual(self.watchdog.check(), 'limited')
        self.assertEqual(self.helper.stops, 4)
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

    def test_a_vm_that_the_mac_restart_stopped_starts_again(self) -> None:
        self.helper.state = 'stopped'
        # The VM started an hour ago; the Mac booted a minute ago.
        (self.directory / 'intent.json').write_text(json.dumps({'state': 'running', 'by': 'start', 'at': time.time() - 3600}))
        with patch.object(watchdog, 'host_boot_time', return_value=time.time() - 60):
            self.assertEqual(self.watchdog.check(), 'restarted')
            self.assertEqual(self.helper.launches, 1)
            self.assertEqual(self.records()[0]['kind'], 'host_restart')
            self.assertEqual(self.finished[0]['chats'], ['chat-1'])
            self.assertNotIn('restart', self.client.intent())
            # Its new intent is from this boot: a later stop in this boot keeps it stopped.
            self.helper.state = 'stopped'
            self.clock.now += 3600
            self.assertEqual(self.watchdog.check(), 'stopped')
            self.assertEqual(self.helper.launches, 1)

    def test_a_vm_stopped_on_purpose_before_the_mac_restart_stays_stopped(self) -> None:
        self.helper.state = 'stopped'
        self.client._write_intent('stopped', 'stop')
        stopped = self.client.intent()['at']
        with patch.object(watchdog, 'host_boot_time', return_value=stopped + 60):
            self.assertEqual(self.watchdog.check(), 'stopped')
        with patch.object(watchdog, 'host_boot_time', return_value=None):
            self.client._write_intent('running', 'start')
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
        for step in range(7):
            (self.directory / 'console.log').write_text(f'boot {step}\n')
            watchdog._keep_console(self.directory, self.clock.now + step)
        kept = sorted(path.read_text() for path in self.directory.glob('console-crash-*.log'))
        # The five newest copies stay.
        self.assertEqual(kept, ['boot 2\n', 'boot 3\n', 'boot 4\n', 'boot 5\n', 'boot 6\n'])

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


class UpgradeSpace(unittest.TestCase):
    """The upgrade backup is an APFS clone, so its allocated size needs no free space."""

    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory(prefix='vm-upgrade-')
        self.addCleanup(temporary.cleanup)
        self.directory = Path(temporary.name)
        (self.directory / 'config.json').write_text('{}')
        (self.directory / 'helper').touch(mode=0o700)
        # 8 MiB of allocated blocks: the old rule wanted this much above the reserve.
        (self.directory / 'data.raw').write_bytes(b'x' * 8 * 1024 * 1024)
        (self.directory / 'system.raw').write_bytes(b'x')
        path = Path(vm.__file__).with_name('upgrade-linux-vm-layr.py')
        spec = importlib.util.spec_from_file_location('upgrade_linux_vm_layr', path)
        assert spec is not None and spec.loader is not None
        self.upgrade = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(self.upgrade)

    def run_with_free(self, free: int) -> Mock:
        usage = SimpleNamespace(total=free * 2, used=free, free=free)
        copy = Mock(side_effect=RuntimeError('clone reached'))
        with patch.object(vm.Client, 'call', return_value={'providers': []}), \
                patch.object(self.upgrade.shutil, 'disk_usage', return_value=usage), \
                patch.object(self.upgrade.subprocess, 'run', copy):
            # The space rule belongs to the reboot path; the in-place path makes no backup.
            self.upgrade.upgrade(self.directory, self.directory / 'helper', self.directory, 'space-1',
                                 reboot=True)
        return copy

    def test_the_reserve_alone_admits_the_clone_backup(self) -> None:
        with self.assertRaisesRegex(RuntimeError, 'clone reached'):
            self.run_with_free(self.upgrade.BACKUP_RESERVE_BYTES + 1024 * 1024)

    def test_less_than_the_reserve_stops_before_the_backup(self) -> None:
        with self.assertRaisesRegex(vm.LinuxVMError, 'must exceed 20 GiB'):
            self.run_with_free(self.upgrade.BACKUP_RESERVE_BYTES - 1)
        self.assertFalse((self.directory / 'updates/space-1/receipt.json').exists())


class UpgradeOrphans(unittest.TestCase):
    """An approved lost provider stops the reboot path only while its process can still run."""

    HANDLE = 'linux-worker:efb541b3-cdaa-408c-bea2-aaedeb87171d'

    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory(prefix='vm-upgrade-')
        self.addCleanup(temporary.cleanup)
        self.directory = Path(temporary.name)
        (self.directory / 'config.json').write_text('{}')
        (self.directory / 'helper').touch(mode=0o700)
        for name in ('system.raw', 'data.raw'):
            (self.directory / name).write_bytes(b'x')
        path = Path(vm.__file__).with_name('upgrade-linux-vm-layr.py')
        spec = importlib.util.spec_from_file_location('upgrade_linux_vm_layr', path)
        assert spec is not None and spec.loader is not None
        self.upgrade = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(self.upgrade)

    def run_with(self, row: dict[str, Any], approved: set[str]) -> None:
        usage = SimpleNamespace(total=2**50, used=0, free=2**49)
        copy = Mock(side_effect=RuntimeError('clone reached'))
        with patch.object(vm.Client, 'call', return_value={'providers': [row]}), \
                patch.object(self.upgrade.shutil, 'disk_usage', return_value=usage), \
                patch.object(self.upgrade.subprocess, 'run', copy):
            self.upgrade.upgrade(self.directory, self.directory / 'helper', self.directory,
                                 'orphan-1', approved, reboot=True)

    def lost(self, reason: str) -> dict[str, Any]:
        # The row that a VM panic left on igor-mbp, 2026-10-21.
        return {'handle': self.HANDLE, 'state': 'lost', 'reason': reason,
                'agentId': 'efb541b3-cdaa-408c-bea2-aaedeb87171d'}

    def test_an_approved_agent_handle_whose_process_is_gone_admits_the_backup(self) -> None:
        with self.assertRaisesRegex(RuntimeError, 'clone reached'):
            self.run_with(self.lost('native_process_lost'), {self.HANDLE})

    def test_the_same_lost_provider_without_approval_blocks(self) -> None:
        with self.assertRaisesRegex(vm.LinuxVMError, 'prevent a VM restart'):
            self.run_with(self.lost('native_process_lost'), set())

    def test_an_approved_provider_whose_process_can_still_run_blocks(self) -> None:
        for reason in ('timeout', 'outcome_unknown', 'supervisor_lost'):
            with self.subTest(reason=reason), \
                    self.assertRaisesRegex(vm.LinuxVMError, 'prevent a VM restart'):
                self.run_with(self.lost(reason), {self.HANDLE})

    def test_an_approved_legacy_uuid_handle_keeps_its_rule(self) -> None:
        handle = 'efb541b3-cdaa-408c-bea2-aaedeb87171d'
        row = {'handle': handle, 'state': 'lost', 'reason': 'outcome_unknown'}
        with self.assertRaisesRegex(RuntimeError, 'clone reached'):
            self.run_with(row, {handle})

    def test_an_approved_handle_must_have_the_provider_format(self) -> None:
        for handle in ('linux-worker:lead', 'linux-worker:EFB541B3-CDAA-408C-BEA2-AAEDEB87171D',
                       'worker:efb541b3-cdaa-408c-bea2-aaedeb87171d', ''):
            with self.subTest(handle=handle), self.assertRaisesRegex(ValueError, 'linux-worker'):
                self.run_with(self.lost('native_process_lost'), {handle})
        self.assertFalse((self.directory / 'updates').exists())


if __name__ == '__main__':
    unittest.main()
