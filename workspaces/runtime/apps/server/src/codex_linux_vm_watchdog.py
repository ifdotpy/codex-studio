"""Restart the Linux VM after its guest dies (docs/linux-vm-workspaces.md, "Guest crash watchdog").

`codex_linux_vm.Client.status` reports `failed` for a proven death behind a live helper (a
kernel panic in the console of the current boot, or the helper's own failure), and
`unresponsive` for no answer during UNANSWERED_SECONDS after the guest answered in this boot.
This module probes the guest, records both, and restarts only a failed VM, through the normal
start path with limits. No answer does not prove death: a busy guest can refuse connections.
It never starts a VM that a person or a command stopped.
"""
from __future__ import annotations

from datetime import datetime, timezone
import json
from pathlib import Path
import platform
import threading
import time
from typing import Any, Callable
import uuid

from codex_linux_vm import Client, LinuxVMError, _atomic_json

PROBE_INTERVAL = 30.0
PROBE_TIMEOUT = 15.0
MIN_RESTART_INTERVAL = 10 * 60
DAILY_RESTART_LIMIT = 4
RESTART_TIMEOUT = 1800.0
# A restart that a backend stop interrupted between its stop and its start ends this late.
RESUME_SECONDS = 30 * 60
CRASH_LOGS_KEPT = 5
RECORDS_KEPT = 50
TAIL_BYTES = 4096
RECORD_FILE = 'watchdog.json'
_DAY = 24 * 3600

Record = dict[str, Any]


def _utc(at: float) -> str:
    return datetime.fromtimestamp(at, timezone.utc).strftime('%Y-%m-%dT%H:%M:%SZ')


def load(state_dir: Path) -> dict[str, Any]:
    """The restart records of the VM in this state directory, oldest first."""
    try:
        value = json.loads((state_dir / RECORD_FILE).read_text())
    except (OSError, ValueError):
        return {'restarts': []}
    if not isinstance(value, dict) or not isinstance(value.get('restarts'), list):
        return {'restarts': []}
    value['restarts'] = [row for row in value['restarts']
                         if isinstance(row, dict) and isinstance(row.get('at'), (int, float))]
    return value


def _save(state_dir: Path, value: dict[str, Any]) -> None:
    value['restarts'] = value['restarts'][-RECORDS_KEPT:]
    if isinstance(value.get('unresponsive'), list):
        value['unresponsive'] = value['unresponsive'][-RECORDS_KEPT:]
    _atomic_json(state_dir / RECORD_FILE, value)


def next_allowed(restarts: list[Record], now: float) -> tuple[float | None, str | None]:
    """When the limits permit the next restart, and the limit that holds it."""
    times = sorted(row['at'] for row in restarts)
    if times and now - times[-1] < MIN_RESTART_INTERVAL:
        return times[-1] + MIN_RESTART_INTERVAL, f'one automatic restart in {MIN_RESTART_INTERVAL // 60} minutes'
    day = [at for at in times if now - at < _DAY]
    if len(day) >= DAILY_RESTART_LIMIT:
        return day[-DAILY_RESTART_LIMIT] + _DAY, f'{DAILY_RESTART_LIMIT} automatic restarts in 24 hours'
    return None, None


def summary(state_dir: Path, *, now: float | None = None) -> dict[str, Any] | None:
    """The part of the records that `status()` shows."""
    saved = load(state_dir)
    restarts = saved['restarts']
    episodes = saved.get('unresponsive') if isinstance(saved.get('unresponsive'), list) else []
    if not restarts and not saved.get('held') and not episodes:
        return None
    now = time.time() if now is None else now
    result: dict[str, Any] = {'restartsInLastDay': sum(1 for row in restarts if now - row['at'] < _DAY)}
    if restarts:
        result['lastRestart'] = {key: restarts[-1].get(key) for key in (
            'id', 'time', 'kind', 'reason', 'stage', 'outcome', 'error', 'consoleLog')}
    if saved.get('held'):
        result['held'] = saved['held']
    if episodes:
        result['lastUnresponsive'] = episodes[-1]
    return result


def _keep_console(state_dir: Path, at: float) -> tuple[str | None, str]:
    """Copy the console of the failed boot before the next boot clears it; its tail."""
    try:
        data = (state_dir / 'console.log').read_bytes()
    except FileNotFoundError:
        return None, ''
    name = 'console-crash-' + datetime.fromtimestamp(at, timezone.utc).strftime('%Y%m%dT%H%M%SZ') + '.log'
    target = state_dir / name
    target.write_bytes(data)
    target.chmod(0o600)
    for old in sorted(state_dir.glob('console-crash-*.log'))[:-CRASH_LOGS_KEPT]:
        old.unlink(missing_ok=True)
    return name, data[-TAIL_BYTES:].decode(errors='replace')


class Watchdog:
    """One check per call: probe health, then restart a failed VM within the limits."""

    def __init__(self, client: Client, *, on_stopped: Callable[[Record], list[str]] | None = None,
                 on_finished: Callable[[Record], None] | None = None,
                 clock: Callable[[], float] = time.time) -> None:
        self.client = client
        self.on_stopped = on_stopped
        self.on_finished = on_finished
        self.clock = clock

    def check(self) -> str:
        """Return what the check did: absent, unknown, a VM state, busy, limited,
        recovered, restarted, failed or cancelled."""
        client = self.client
        if not (client.state_dir / 'config.json').exists():
            return 'absent'
        try:
            status = client.status()
            answered = False
            # Probe an unresponsive guest too: one answer clears that state.
            if status['state'] in {'running', 'unresponsive'}:
                answered = self.probe(status)
                status = client.status()
        except LinuxVMError:
            return 'unknown'
        if status['state'] == 'stopped':
            return self.resume()
        if status['state'] == 'running':
            self._hold(None)
            self._unresponsive(None)
            if answered:
                self._settle()
        if status['state'] == 'unresponsive':
            # Shown and recorded, never restarted: a full or slow guest gives no answer too.
            self._unresponsive(status)
        if status['state'] != 'failed':
            return str(status['state'])
        return self.restart(status)

    def _unresponsive(self, status: dict[str, Any] | None) -> None:
        """Open an episode for an unresponsive guest, or end the open one."""
        saved = load(self.client.state_dir)
        stored = saved.get('unresponsive')
        episodes: list[Record] = [row for row in stored if isinstance(row, dict)] if isinstance(stored, list) else []
        current = episodes[-1] if episodes and not episodes[-1].get('endedAt') else None
        if status is None:
            if current is None:
                return
            current['endedAt'] = self.clock()
        else:
            failure = status.get('guestFailure') or {}
            if current is not None and current.get('helperPid') == status.get('pid'):
                return
            episodes.append({'since': failure.get('since'), 'detectedAt': self.clock(),
                             'time': _utc(self.clock()), 'helperPid': status.get('pid'),
                             'reason': status.get('error')})
        saved['unresponsive'] = episodes
        _save(self.client.state_dir, saved)

    def probe(self, status: dict[str, Any]) -> bool:
        error = self.client.ping(PROBE_TIMEOUT)
        self.client.record_health(status.get('pid'), error, now=self.clock())
        return error is None

    def _settle(self) -> None:
        """Close a restart whose start a backend stop interrupted after the launch: the
        guest answers, so the VM must not start again after a later stop."""
        restart = self.client.intent().get('restart')
        if not isinstance(restart, str):
            return
        record = next((row for row in load(self.client.state_dir)['restarts'] if row.get('id') == restart), None)
        if record is not None and record.get('stage') in {'stopping', 'starting'}:
            record.update(stage='done', outcome='restarted', finishedAt=self.clock())
            self._update(record)
            if self.on_finished is not None:
                self.on_finished(record)
        self._finish_intent(restart)

    def _hold(self, held: dict[str, Any] | None) -> None:
        saved = load(self.client.state_dir)
        if saved.get('held') != held:
            saved['held'] = held
            _save(self.client.state_dir, saved)

    def _update(self, record: Record) -> None:
        saved = load(self.client.state_dir)
        rows = [row for row in saved['restarts'] if row.get('id') != record['id']]
        saved['restarts'] = [*rows, record]
        saved['held'] = None
        _save(self.client.state_dir, saved)

    def restart(self, status: dict[str, Any]) -> str:
        failure = status.get('guestFailure') or {
            'kind': 'helper_failed', 'reason': 'The VM helper failed: ' + str(status.get('error') or 'no reason')}
        try:
            with self.client.maintenance():
                return self._restart(failure, status.get('pid'))
        except LinuxVMError as error:
            if error.code in {'maintenance_busy', 'lock_busy'}:
                return 'busy'
            raise

    def _restart(self, failure: dict[str, Any], pid: Any) -> str:
        now = self.clock()
        until, rule = next_allowed(load(self.client.state_dir)['restarts'], now)
        if until is not None:
            self._hold({'reason': failure['reason'], 'rule': rule, 'until': until, 'untilTime': _utc(until)})
            return 'limited'
        record: Record = {'id': str(uuid.uuid4()), 'at': now, 'time': _utc(now), 'kind': failure['kind'],
                          'reason': failure['reason'], 'helperPid': pid, 'stage': 'stopping'}

        def before_stop(_state: dict[str, Any]) -> None:
            record['consoleLog'], record['consoleTail'] = _keep_console(self.client.state_dir, now)
            # The record and the intent are saved before the stop: a backend that stops
            # during the restart leaves a restart that the next backend can finish.
            self._update(record)
            self.client._write_intent('running', 'watchdog', record['id'])

        try:
            if not self.client.stop_failed(pid, before_stop):
                return 'recovered'
        except LinuxVMError as error:
            if 'consoleTail' not in record:
                raise
            # The stop has no proven result. The intent stays: if the helper exits
            # later, resume() starts the VM.
            record['error'] = str(error)[:500]
            self._update(record)
            return 'stop_failed'
        record.update(stage='starting', stoppedAt=self.clock())
        if self.on_stopped is not None:
            record['chats'] = self.on_stopped(record)
        self._update(record)
        return self._start(record)

    def _finish_intent(self, restart: str) -> None:
        try:
            self.client.finish_restart(restart)
        except LinuxVMError:
            # A held client lock: the next resume() clears it, as the record is final.
            pass

    def _start(self, record: Record) -> str:
        try:
            self.client.ensure_running(timeout=RESTART_TIMEOUT, watchdog_restart=record['id'])
        except Exception as error:
            # A stop on purpose before the launch, or one that ended the new helper.
            cancelled = ((isinstance(error, LinuxVMError) and error.code == 'stopped_on_purpose')
                         or self.client.intent().get('state') == 'stopped')
            try:
                running = (not cancelled and self.client.status()['state'] == 'running'
                           and self.client.ping(PROBE_TIMEOUT) is None)
            except LinuxVMError:
                running = False
            # A late step (host_exec, share mounts) can fail after the guest answered.
            outcome = 'cancelled' if cancelled else 'restarted' if running else 'failed'
            record.update(stage='done' if running else outcome, outcome=outcome,
                          error=str(error)[:500] or type(error).__name__, finishedAt=self.clock())
            if not cancelled:
                self._finish_intent(record['id'])
        else:
            record.update(stage='done', outcome='restarted', finishedAt=self.clock())
            self._finish_intent(record['id'])
        self._update(record)
        if self.on_finished is not None:
            self.on_finished(record)
        return str(record['outcome'])

    def resume(self) -> str:
        """Finish this module's own restart that a backend stop interrupted. A stopped VM
        without one stays stopped."""
        restart = self.client.intent().get('restart')
        if not isinstance(restart, str):
            return 'stopped'
        try:
            with self.client.maintenance():
                record = next((row for row in load(self.client.state_dir)['restarts']
                               if row.get('id') == restart), None)
                if record is None or record.get('stage') not in {'stopping', 'starting'}:
                    self._finish_intent(restart)
                    return 'stopped'
                if self.clock() - record['at'] > RESUME_SECONDS:
                    record.update(stage='failed', outcome='abandoned', finishedAt=self.clock(),
                                  error='The restart did not finish. The VM stays stopped.')
                    self._finish_intent(restart)
                    self._update(record)
                    return 'abandoned'
                record['stage'] = 'starting'
                if 'chats' not in record and self.on_stopped is not None:
                    record['chats'] = self.on_stopped(record)
                self._update(record)
                return self._start(record)
        except LinuxVMError as error:
            if error.code in {'maintenance_busy', 'lock_busy'}:
                return 'busy'
            raise


def _notice(record: Record) -> str:
    when = time.strftime('%H:%M', time.localtime(record.get('finishedAt') or record['at']))
    lead = f"The Linux VM stopped working ({record['reason']}). "
    if record.get('outcome') == 'restarted':
        return (lead + f'Studio restarted it at {when}. Your next message starts this chat again. '
                'If a turn was running, its result is unknown: review it before you continue.')
    if record.get('outcome') == 'cancelled':
        return lead + 'It was stopped on purpose, so Studio did not start it again.'
    return (lead + f"Studio could not restart it: {record.get('error') or 'no reason'}. "
            'Start the VM again, then send your next message.')


def drop_providers(runtime: Any, record: Record) -> list[str]:
    """End the provider connections of the dead boot through the normal transport loss:
    in-flight turns become interrupted with an unknown outcome, and nothing is sent again."""
    with runtime.lock:
        servers = dict(runtime.__dict__.get('linux_servers', {}))
        # The next message opens a new provider, even if a reader still waits on the old one.
        runtime.__dict__.setdefault('offline_linux_agents', set()).update(servers)
    for server in servers.values():
        try:
            server.fail_transport('The Linux VM stopped working (' + str(record['reason'])
                                  + '). Studio restarted it; outcome unknown.')
        except Exception:
            continue
    return sorted(servers)


def post_notices(runtime: Any, record: Record) -> None:
    """Show the restart in each chat that had a provider in the dead boot."""
    text = _notice(record)
    with runtime.lock:
        if runtime.closed:
            return
        with runtime.db() as db:
            for agent_id in record.get('chats') or []:
                try:
                    agent = runtime.agent(agent_id, db)
                except ValueError:
                    continue
                if agent.get('deletedAt'):
                    continue
                runtime.item(db, agent_id, 'linux-vm-restart:' + record['id'], 'system', text,
                             'Linux VM', nativeNotice='error')


class _Service:
    def __init__(self, runtime: Any, client: Client, interval: float) -> None:
        self.runtime = runtime
        self.interval = interval
        self.stop = threading.Event()
        self.watchdog = Watchdog(client,
                                 on_stopped=lambda record: drop_providers(runtime, record),
                                 on_finished=lambda record: post_notices(runtime, record))
        self.thread = threading.Thread(target=self.run, name='studio-vm-watchdog', daemon=True)

    def run(self) -> None:
        while not self.stop.wait(self.interval) and not self.runtime.closed:
            try:
                self.runtime._vm_watchdog_last = {'at': time.time(), 'result': self.watchdog.check()}
            except Exception as error:
                self.runtime._vm_watchdog_last = {'at': time.time(), 'error': str(error)[:300]}


_START_LOCK = threading.Lock()


def start(runtime: Any, *, interval: float = PROBE_INTERVAL) -> None:
    """Start the watchdog thread of a macOS backend that owns the VM: its VM state directory
    is inside the runtime's state directory. A runtime with another root (a test) never
    restarts the VM of the user's backend."""
    if platform.system() != 'Darwin':
        return
    from codex_linux_workspaces import client
    try:
        vm_client: Client = client(runtime)
    except (RuntimeError, OSError):
        # A state path too long for the VM sockets, or one in .claude: no VM to watch.
        return
    if not vm_client.state_dir.is_relative_to(Path(runtime.root).resolve()):
        return
    with _START_LOCK:
        if runtime.__dict__.get('_vm_watchdog') is None:
            service = runtime._vm_watchdog = _Service(runtime, vm_client, interval)
            service.thread.start()


def close(runtime: Any) -> None:
    """Stop the checks. A restart in progress continues in its daemon thread; it writes to
    the runtime only under its lock while the runtime is open."""
    service = runtime.__dict__.get('_vm_watchdog')
    if service is not None:
        service.stop.set()
