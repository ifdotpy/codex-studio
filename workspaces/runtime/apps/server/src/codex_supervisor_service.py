"""Install independent supervisor services without the desktop UI."""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import plistlib
import shutil
import signal
import sqlite3
import subprocess
import sys
import time
import tempfile
from typing import Any, Callable, cast
from urllib.error import URLError
from urllib.parse import urlsplit
from urllib.request import HTTPRedirectHandler, ProxyHandler, build_opener

from codex_layout import REPOSITORY_ROOT


def _resource_path(resources: Path, relocated: str, legacy: str) -> Path:
    current = resources / relocated
    return current if current.exists() else resources / legacy


def _runtime_source(resources: Path) -> Path:
    return _resource_path(resources, 'workspaces/runtime/apps/server/src', 'scripts')


def _supervisor_script(resources: Path) -> Path:
    return _runtime_source(resources) / 'codex_process_supervisor.py'


def _canvas_launcher(resources: Path) -> Path:
    return _runtime_source(resources) / 'codex-canvas'


def _canvas_launchers(resources: Path) -> tuple[Path, ...]:
    # A backend started before the relocation keeps the root compatibility
    # launcher in its command line while the checkout already has the new tree.
    return (_canvas_launcher(resources), resources / 'scripts' / 'codex-canvas')


def _supervisor_launcher(resources: Path) -> Path:
    return _runtime_source(resources) / 'codex-supervisor'


def _recovery_script(resources: Path) -> Path:
    relocated = resources / 'workspaces/client/apps/desktop/recover_backend.py'
    if relocated.is_file():
        return relocated
    packaged = resources.parent / 'recover_backend.py'
    if packaged.is_file():
        return packaged
    return resources / 'desktop/recover_backend.py'


def _default_resources() -> Path:
    source = Path(__file__).resolve().parent
    for candidate in (source, *source.parents):
        if (candidate / 'workspaces/runtime/apps/server/src/codex_layout.py').is_file():
            return candidate
    if source.name == 'scripts':
        return source.parent
    return REPOSITORY_ROOT


def atomic_write(path: Path, data: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(path.name + '.' + str(os.getpid()) + '.tmp')
    try:
        fd = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(fd, 'wb') as stream:
            stream.write(data)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
        fd = os.open(path.parent, os.O_RDONLY)
        try:
            os.fsync(fd)
        finally:
            os.close(fd)
    finally:
        temporary.unlink(missing_ok=True)


class NoRedirect(HTTPRedirectHandler):
    def redirect_request(self, req: Any, fp: Any, code: int, msg: str,
                         headers: Any, newurl: str) -> None:
        return None


def desktop(url: str, state: Path) -> dict[str, Any] | None:
    parsed = urlsplit(url)
    if parsed.scheme != 'http' or parsed.hostname not in {'127.0.0.1', 'localhost', '::1'}:
        raise ValueError('Enable the supervisor through a local HTTP URL.')
    try:
        with build_opener(ProxyHandler({}), NoRedirect()).open(url.rstrip('/') + '/api/desktop', timeout=3) as response:
            result = json.load(response)
    except URLError as error:
        if isinstance(error.reason, ConnectionRefusedError):
            return None
        raise
    if (not isinstance(result, dict) or result.get('application') != 'codex-agents'
            or result.get('protocol') != 1 or result.get('stateDir') != str(state)
            or type(result.get('pid')) is not int or result['pid'] < 1):
        raise ValueError('The backend identity does not match the selected state directory.')
    return cast(dict[str, Any], result)


def idle(state: Path) -> dict[str, int]:
    """Read current activity, never open a runtime or modify its database."""
    counts: dict[str, int] = {}
    database = state / 'canvas.sqlite3'
    if not database.exists():
        return counts
    db = sqlite3.connect(database.as_uri() + '?mode=ro', uri=True, timeout=3)
    try:
        db.execute('BEGIN')
        tables = {row[0] for row in db.execute("SELECT name FROM sqlite_master WHERE type='table'")}
        checks = {
            'runtime_agents': "COALESCE(json_extract(record,'$.inFlight'),0) OR json_extract(record,'$.status') IN ('running','starting','queued','stopping','waiting','approval')",
            'runtime_monitors': "json_extract(record,'$.status') IN ('running','starting','approval','lost')",
            'user_terminals': "json_extract(record,'$.status')='running'",
            'runtime_server_exec': "json_extract(record,'$.status') IN ('starting','running')",
            'runtime_events': "status IN ('pending','reserved','dispatching','uncertain')",
        }
        for table, condition in checks.items():
            if table in tables:
                counts[table] = db.execute('SELECT COUNT(*) FROM ' + table + ' WHERE ' + condition).fetchone()[0]
    finally:
        db.close()
    if any(counts.values()):
        raise RuntimeError('The first cutover requires an idle backend: ' + json.dumps(counts, sort_keys=True))
    return counts


def health(state: Path) -> dict[str, Any]:
    from codex_process_supervisor import status
    read = cast(Callable[[Path], dict[str, Any]], status)
    result = read(state)
    if result.get('protocol') != 1 or result.get('stateDir') != str(state):
        raise RuntimeError('The supervisor identity is incompatible.')
    if result.get('recovery', {}).get('blocked') or result.get('recovery', {}).get('degraded'):
        raise RuntimeError('Supervisor crash recovery needs operator attention.')
    return result


def wait_ready(state: Path, timeout: float = 60) -> dict[str, Any]:
    deadline = time.monotonic() + timeout
    while True:
        try:
            return health(state)
        except (OSError, RuntimeError, ValueError):
            if time.monotonic() >= deadline:
                raise RuntimeError('The supervisor did not become ready before the deadline.') from None
            time.sleep(.2)


def run(arguments: list[str]) -> subprocess.CompletedProcess[str]:
    return subprocess.run(arguments, check=True, capture_output=True, text=True, timeout=30)


def launch_agent(label: str, arguments: list[str], log: Path) -> bytes:
    return plistlib.dumps({'Label': label, 'ProgramArguments': arguments,
                          'RunAtLoad': True, 'KeepAlive': True, 'ProcessType': 'Interactive',
                          'ThrottleInterval': 10, 'LimitLoadToSessionType': 'Aqua',
                          'AbandonProcessGroup': True, 'StandardOutPath': str(log),
                          'StandardErrorPath': str(log)})


def systemd_quote(value: str, *, command: bool = False) -> str:
    if any(char in value for char in '\r\n\0'):
        raise ValueError('Service values cannot contain newlines or NUL.')
    value = value.replace('\\', '\\\\').replace('"', '\\"').replace('%', '%%')
    if command:
        value = value.replace('$', '$$')
    return '"' + value + '"'


def linux_units(resources: Path, state: Path, python: str, port: int, restart_env: dict[str, str] | None = None) -> dict[str, bytes]:
    def command(*args: str) -> str:
        return ' '.join(systemd_quote(arg, command=True) for arg in args)
    environment = '\n'.join('Environment=' + systemd_quote(key + '=' + value) for key, value in {
        **(restart_env or {}),
        'CODEX_AGENTS_STATE_DIR': str(state), 'CODEX_AGENTS_SUPERVISOR_MODE': '1',
        'PATH': os.environ.get('PATH', '/usr/local/bin:/usr/bin:/bin'),
    }.items())
    supervisor = f'''[Unit]
Description=Codex Studio process supervisor

[Service]
Type=exec
{environment}
ExecStart={command(python, '-B', str(_supervisor_script(resources)), '--state', str(state), '--wait-for-lease')}
Restart=always
RestartSec=2
KillMode=process
TimeoutStopSec=30

[Install]
WantedBy=default.target
'''
    working_directory = str(resources).replace('%', '%%')
    backend = f'''[Unit]
Description=Codex Studio server
Wants=codex-studio-supervisor.service
After=network-online.target codex-studio-supervisor.service

[Service]
Type=exec
WorkingDirectory={working_directory}
{environment}
ExecStartPre={command(python, '-B', str(_supervisor_launcher(resources)), 'wait', '--state', str(state))}
ExecStart={command(python, '-B', str(_canvas_launcher(resources)), '--port', str(port))}
Restart=always
RestartSec=5
KillMode=process
TimeoutStartSec=90
TimeoutStopSec=30

[Install]
WantedBy=default.target
'''
    return {'codex-studio-supervisor.service': supervisor.encode(), 'codex-studio.service': backend.encode()}


def install_macos(resources: Path, state: Path, python: str, config: dict[str, Any]) -> None:
    suffix = hashlib.sha256(str(state).encode()).hexdigest()[:16]
    domain = 'gui/' + str(os.getuid())
    agents = Path.home() / 'Library/LaunchAgents'
    jobs = {
        'supervisor': [python, '-B', str(_supervisor_script(resources)), '--state', str(state), '--wait-for-lease'],
        'recovery': [python, '-B', str(_recovery_script(resources)), '--config', str(state / 'background-recovery.json')],
    }
    # Save the next backend's mode without replacing its existing launch paths.
    atomic_write(state / 'background-recovery.json', (json.dumps(config, indent=2) + '\n').encode())
    for kind, arguments in jobs.items():
        label = 'local.codex.agents.' + kind + '.' + suffix
        plist = agents / (label + '.plist')
        try:
            run(['/bin/launchctl', 'print', domain + '/' + label])
        except subprocess.CalledProcessError:
            atomic_write(plist, launch_agent(label, arguments, state / (kind + '.log')))
            run(['/bin/launchctl', 'bootstrap', domain, str(plist)])
        # Never unload or restart a registered supervisor or recovery owner.
        run(['/bin/launchctl', 'print', domain + '/' + label])


def enable(resources: Path, state: Path, port: int) -> dict[str, Any]:
    if sys.platform not in {'darwin', 'linux'}:
        raise ValueError('Supervisor service installation supports macOS and Linux.')
    import fcntl
    resources, state = resources.expanduser().resolve(), state.expanduser().resolve()
    state.mkdir(mode=0o700, parents=True, exist_ok=True)
    with (state / 'supervisor-enable.lock').open('a+') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        return _enable(resources, state, port)


def _enable(resources: Path, state: Path, port: int) -> dict[str, Any]:
    import fcntl
    url = 'http://127.0.0.1:' + str(port)
    before = desktop(url, state)
    config_path = state / 'background-recovery.json'
    config = json.loads(config_path.read_text()) if config_path.exists() else {}
    if config and (config.get('version') != 1 or config.get('stateDir') != str(state)):
        raise ValueError('The recovery configuration uses a different state directory.')
    if sys.platform == 'darwin' and config:
        resources = Path(config['resources']).resolve()
        python = config['python']
        if config.get('port') != port:
            raise ValueError('The selected port differs from the recovery configuration.')
    else:
        python = sys.executable
    initial = before is None or before.get('supervisorMode') is not True
    from codex_process_supervisor import process_start_time, process_launch_command
    birth = cast(Callable[[int], str | None], process_start_time)
    arguments = cast(Callable[[int], list[str]], process_launch_command)
    started = birth(before['pid']) if before else None
    if before and (started is None or not any(Path(arg).resolve() in _canvas_launchers(resources) for arg in arguments(before['pid']))):
        raise RuntimeError('Cannot prove the backend process identity; no process was signaled.')
    if before and sys.platform == 'linux':
        main_pid = int(run(['systemctl', '--user', 'show', 'codex-studio.service', '--property=MainPID', '--value']).stdout.strip())
        if main_pid != before['pid']:
            raise RuntimeError('The backend is not owned by codex-studio.service; no service was changed.')
    if initial:
        idle(state)
        if before is None:
            with (state / 'runtime.lock').open('a+') as lease:
                fcntl.flock(lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
        if (state / 'supervisor.sock').exists() and health(state).get('handles'):
            raise RuntimeError('The first cutover requires an empty supervisor handle journal.')
    for script in (_supervisor_script(resources), _canvas_launcher(resources), _supervisor_launcher(resources)):
        if not script.is_file():
            raise ValueError('The installed resources are missing ' + str(script))
    if sys.platform == 'darwin' and not _recovery_script(resources).is_file():
        raise ValueError('The installed resources are missing ' + str(_recovery_script(resources)))
    if sys.platform == 'darwin':
        if not config:
            codex = shutil.which('codex')
            if not codex:
                raise ValueError('Install Codex and add it to PATH before enabling recovery.')
            config = {'version': 1, 'stateDir': str(state), 'resources': str(resources),
                      'python': python, 'codex': codex, 'port': port,
                      'environment': {'PATH': os.environ.get('PATH', '/usr/bin:/bin')}, 'unsetEnvironment': []}
        config.update(enabled=True, supervisorEnabled=True)
        install_macos(resources, state, python, config)
    else:
        units = linux_units(resources, state, python, port, (before or {}).get('restartEnvironment', {}))
        with tempfile.TemporaryDirectory(prefix='codex-services-') as temporary:
            paths = []
            for name, content in units.items():
                path = Path(temporary) / name
                path.write_bytes(content)
                paths.append(str(path))
            run(['systemd-analyze', '--user', 'verify', *paths])
        for name, content in units.items():
            atomic_write(Path.home() / '.config/systemd/user' / name, content)
        run(['systemctl', '--user', 'daemon-reload'])
        run(['systemctl', '--user', 'enable', '--now', 'codex-studio-supervisor.service'])
        run(['systemctl', '--user', 'enable', 'codex-studio.service'])
    ready = wait_ready(state)
    if initial:
        if ready.get('handles'):
            raise RuntimeError('The first cutover requires an empty supervisor handle journal.')
        idle(state)
        current = desktop(url, state)
        if before and (current or {}).get('pid') != before['pid']:
            raise RuntimeError('The backend changed during setup; no backend was signaled. Run enable again.')
        if before and birth(before['pid']) != started:
            raise RuntimeError('The backend process identity changed; no process was signaled.')
        if sys.platform == 'linux' and not (current and current.get('supervisorMode') is True):
            run(['systemctl', '--user', 'restart' if before else 'start', 'codex-studio.service'])
        elif before:
            os.kill(before['pid'], signal.SIGTERM)
    deadline = time.monotonic() + 90
    while time.monotonic() < deadline:
        current = desktop(url, state)
        if (current and current.get('supervisorMode') is True
                and (current.get('supervisor') or {}).get('protocol') == 1):
            return {'stateDir': str(state), 'previousPid': (before or {}).get('pid'),
                    'pid': current['pid'], 'supervisorMode': True, 'protocol': 1}
        time.sleep(.25)
    raise RuntimeError('The backend did not enter supervisor mode before the deadline. Inspect service logs; do not start another backend.')


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('action', choices=('enable', 'wait', 'status'))
    parser.add_argument('--state', type=Path, default=Path(os.environ.get('CODEX_AGENTS_STATE_DIR', '~/.local/state/codex-agents')))
    parser.add_argument('--resources', type=Path, default=_default_resources())
    parser.add_argument('--port', type=int, default=int(os.environ.get('CODEX_DESKTOP_PORT', '4620')))
    args = parser.parse_args()
    if not 1024 <= args.port <= 65535:
        parser.error('The port must be between 1024 and 65535.')
    state = args.state.expanduser().resolve()
    try:
        result = (enable(args.resources.expanduser().resolve(), state, args.port) if args.action == 'enable'
                  else wait_ready(state) if args.action == 'wait' else health(state))
    except (OSError, ValueError, RuntimeError, subprocess.SubprocessError) as error:
        parser.exit(1, 'codex-supervisor: ' + str(error) + '\n')
    print(json.dumps(result))
