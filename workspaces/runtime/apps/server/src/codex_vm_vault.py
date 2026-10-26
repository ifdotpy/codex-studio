"""The project vault in the sidebar (docs/vault.md): the VM writer's report of each imported
layr project, kept on the host so that the sidebar never waits for the VM, pulls of the latest
vault state into a new Mac folder, and the setup of a new vault, all through the layr-desk
command."""
from __future__ import annotations

import json
import os
from pathlib import Path
import re
import shlex
import shutil
import subprocess
import sys
import threading
import time
from typing import Any

INTERVAL = 120.0
PULL_TIMEOUT = 3600
# Jobs of each kind that a repeated request id still finds.
JOBS_KEPT = 50
SETUP_TIMEOUT = 3600
LOGIN_TIMEOUT = 900
# A vault name: the reader role studio-vault-reader-<prefix> has at most 64 characters.
PREFIX = re.compile(r'[A-Za-z0-9][A-Za-z0-9_-]{0,43}')
PROFILE = re.compile(r'[A-Za-z0-9][A-Za-z0-9_.@+-]{0,127}')
REGION = re.compile(r'[a-z]{2}(-[a-z]+)+-[0-9]')


def _folder(state_dir: Path, project_id: str) -> Path:
    return state_dir / 'layr-projects' / project_id


def _load(path: Path) -> dict[str, Any]:
    try:
        data = json.loads(path.read_text())
    except FileNotFoundError:
        return {}
    return data if isinstance(data, dict) else {}


def _write(path: Path, data: dict[str, Any]) -> None:
    temporary = path.with_name(path.name + '.tmp')
    temporary.write_text(json.dumps(data, sort_keys=True))
    os.replace(temporary, path)


def store_site(state_dir: Path, project_id: str, site: str) -> None:
    """The Desk site that serves the project's vault (from layr-desk's deploy command)."""
    if not site.startswith('https://') or any(c.isspace() for c in site):
        raise ValueError('The Desk site must be an https URL')
    folder = _folder(state_dir, project_id)
    folder.mkdir(parents=True, exist_ok=True)
    _write(folder / 'vault-site.json', {'site': site.rstrip('/') + '/'})


def vault_view(state_dir: Path, project_id: str) -> dict[str, Any] | None:
    """The sidebar's vault report: host records only, no VM call. None before the first
    check. The time of the last check stays out, so an unchanged view keeps its version."""
    folder = _folder(state_dir, project_id)
    try:
        status = _load(folder / 'vault.json')
        site = _load(folder / 'vault-site.json').get('site')
    except (OSError, ValueError):
        return {'projectId': project_id, 'state': 'error', 'reason': 'The vault records are unreadable'}
    if not status:
        return None
    if not status.get('configured'):
        return {'projectId': project_id, 'state': 'none'}
    view: dict[str, Any] = {'projectId': project_id, 'state': 'error' if status.get('lastError') else 'ok',
                            'prefix': status['prefix']}
    if status.get('lastPush'):
        view['lastPush'] = status['lastPush']
    if status.get('lastError'):
        view['reason'] = str(status['lastError'])[:300]
    if isinstance(site, str):
        view['site'] = site
    try:
        mac = _load(folder / 'vault-mac.json')
    except (OSError, ValueError):
        mac = {}
    if isinstance(mac.get('unlocked'), bool):
        view['unlocked'] = mac['unlocked']
    if isinstance(mac.get('disk'), str):
        view['disk'] = mac['disk']
    return view


def _record(client: Any, project_id: str) -> dict[str, Any] | None:
    """The writer's report from the VM, or None when the VM cannot say now."""
    from codex_linux_vm import LinuxVMError
    try:
        result = client.call('vault.status', {'projectId': project_id}, timeout=30)
    except LinuxVMError as error:
        return {'configured': False} if error.code == 'not_found' else None
    return {'configured': True, 'prefix': result['prefix'], 'lastPush': result.get('lastPush'),
            'lastError': result.get('lastError')}


def views(state_dir: Path) -> dict[str, dict[str, Any] | None]:
    """The sidebar's vault report of every imported project."""
    return {record.parent.name: vault_view(state_dir, record.parent.name)
            for record in sorted((state_dir / 'layr-projects').glob('*/project.json'))}


def check(client: Any) -> bool:
    """Ask the VM about the vault of every imported project; True when a view changed."""
    changed = False
    for record in sorted((client.state_dir / 'layr-projects').glob('*/project.json')):
        try:
            project_id = json.loads(record.read_text())['projectId']
        except (OSError, ValueError, KeyError, TypeError):
            continue
        folder = record.parent
        before = vault_view(client.state_dir, folder.name)
        status = _record(client, project_id)
        if status is None or status == _load(folder / 'vault.json'):
            continue
        _write(folder / 'vault.json', status)
        changed = changed or vault_view(client.state_dir, folder.name) != before
    return changed


def tick(runtime: Any) -> None:
    """Every INTERVAL seconds, check the vaults in the runtime's worker pool: never under the
    runtime lock, never twice at once, and not while the VM is stopped."""
    import platform
    now = time.monotonic()
    if platform.system() != 'Darwin' or runtime.__dict__.get('_vault_busy') \
            or now - runtime.__dict__.get('_vault_checked', -INTERVAL) < INTERVAL:
        return
    from codex_linux_workspaces import client as vm_client
    try:
        client = vm_client(runtime)
    except (RuntimeError, OSError, ValueError):
        return  # Without a VM client there is no layr project to check.
    runtime._vault_busy, runtime._vault_checked = True, now

    def run() -> None:
        try:
            if client.status().get('state') == 'running':
                check(client)
            try:
                mac_check(client.state_dir)
            except Exception as error:  # noqa: BLE001 - this Mac's part waits for the next tick
                print('vault check of this Mac skipped: ' + type(error).__name__ + ': ' + str(error)[:300],
                      file=sys.stderr)
            # A new report or a new Desk site (codex_vault.py site) changes what the sidebar shows.
            current = views(client.state_dir)
            if current != runtime.__dict__.get('_vault_views'):
                from codex_vm_agents import refresh_readiness
                refresh_readiness(runtime)
                runtime._vault_views = current
        except Exception as error:  # noqa: BLE001 - the next tick tries again
            print('vault check skipped: ' + type(error).__name__ + ': ' + str(error)[:300], file=sys.stderr)
        finally:
            runtime._vault_busy = False
    runtime.pool.submit(run)


def bundled_layr_desk() -> Path:
    """The layr-desk that the Studio app carries (Resources/workspace/bin)."""
    return Path(__file__).resolve().parent.parent / 'bin' / 'layr-desk'


def layr_desk() -> str:
    """The layr-desk command: STUDIO_LAYR_DESK_BIN, else the app's own, else PATH, else cargo's
    folder."""
    found = os.environ.get('STUDIO_LAYR_DESK_BIN')
    if found:
        return found
    if bundled_layr_desk().is_file():
        return str(bundled_layr_desk())
    found = shutil.which('layr-desk')
    if found:
        return found
    cargo = Path.home() / '.cargo/bin/layr-desk'
    if cargo.exists():
        return str(cargo)
    raise RuntimeError('The layr-desk command is not installed (cargo install --path crates/layr-desk)')


def _desk(command: list[str], **options: Any) -> subprocess.CompletedProcess[str]:
    return subprocess.run(command, capture_output=True, text=True, timeout=60, **options)


def _settings() -> Path:
    """layr-desk's settings of vaults: gateway.json and one folder of keys per vault."""
    return Path.home() / '.config/studio-vault'


def _jobs(runtime: Any, name: str) -> tuple[dict[str, dict[str, Any]], threading.Lock]:
    jobs: dict[str, dict[str, Any]] = runtime.__dict__.setdefault(name, {})
    lock: threading.Lock = runtime.__dict__.setdefault(name + '_lock', threading.Lock())
    return jobs, lock


def _keep(jobs: dict[str, dict[str, Any]], request_id: str, job: dict[str, Any]) -> None:
    jobs[request_id] = job
    while len(jobs) > JOBS_KEPT:
        jobs.pop(next(iter(jobs)))


def _reason(stderr: str) -> str:
    """layr-desk's error with its causes, on one line; AWS replies hold no secret."""
    text = stderr.strip()
    at = text.rfind('Error: ')
    text = text[at + len('Error: '):] if at >= 0 else (text.splitlines() or ['layr-desk failed'])[-1]
    return ' '.join(text.split())[:500]


def ensure_disk(command: str, site: str) -> str | None:
    """Keep the Desk disk agent (layr-desk service) on this Mac for `site`, running the
    layr-desk that the app carries, so no command line step is needed; return where the disk is
    mounted. A layr-desk outside the app (a development build) only reports."""
    status = json.loads(_desk([command, 'service', 'status'], stdin=subprocess.DEVNULL).stdout or '{}')
    own = Path(command).resolve() == bundled_layr_desk().resolve()
    current = status.get('installed') and status.get('site') == site \
        and Path(str(status.get('program'))).resolve() == Path(command).resolve()
    if own and not current:
        done = _desk([command, 'service', 'install', '--site', site], stdin=subprocess.DEVNULL)
        if done.returncode:
            raise RuntimeError('layr-desk service install: ' + (done.stderr or done.stdout).strip()[-300:])
        status = json.loads(_desk([command, 'service', 'status'], stdin=subprocess.DEVNULL).stdout or '{}')
    at = status.get('at') if status.get('installed') and status.get('site') == site else None
    return at if isinstance(at, str) else None


def mac_check(state_dir: Path) -> None:
    """What this Mac knows about each vault with a Desk site: whether it keeps the vault key
    (layr-desk unlock) and where the Desk disk shows the vault. One layr-desk call per site; it
    reads the Keychain and the gateway, not the VM."""
    projects: dict[str, dict[str, Any]] = {}
    for record in sorted((state_dir / 'layr-projects').glob('*/project.json')):
        view = vault_view(state_dir, record.parent.name)
        if view and view.get('site') and view.get('prefix'):
            projects[record.parent.name] = view
    sites = sorted({view['site'] for view in projects.values()})
    if not sites:
        return
    command = layr_desk()
    # One disk, for the first site; another site's vaults still unlock and pull.
    disk = ensure_disk(command, sites[0].rstrip('/'))
    for site in sites:
        done = _desk([command, 'status', '--site', site, '--json'], stdin=subprocess.DEVNULL)
        if done.returncode:
            raise RuntimeError('layr-desk status: ' + (done.stderr or done.stdout).strip()[-300:])
        unlocked = {p['name']: bool(p['unlocked']) for p in json.loads(done.stdout)['projects']}
        for name, view in projects.items():
            if view['site'] != site:
                continue
            mac: dict[str, Any] = {'unlocked': unlocked.get(view['prefix'], False)}
            if disk and site == sites[0]:
                mac['disk'] = str(Path(disk) / view['prefix'])
            if _load(_folder(state_dir, name) / 'vault-mac.json') != mac:
                _write(_folder(state_dir, name) / 'vault-mac.json', mac)


def unlock(runtime: Any, project_id: str, words: str) -> dict[str, Any]:
    """Keep the vault key on this Mac (layr-desk unlock) from the paper words, which go to
    layr-desk on stdin only and appear in no reply or log."""
    from codex_linux_workspaces import client as vm_client
    state_dir = vm_client(runtime).state_dir
    view = vault_view(state_dir, project_id)
    if not view or not view.get('site') or not view.get('prefix'):
        raise ValueError('The project has no vault with a Desk site')
    done = _desk([layr_desk(), 'unlock', '--site', view['site'], '--project', view['prefix'],
                  '--paper-file', '-'], input=words)
    if done.returncode:
        lines = (done.stderr or done.stdout).strip().splitlines()
        reason = next((line.strip() for line in lines if 'does not open' in line or 'checksum' in line
                       or 'not a word' in line), lines[-1] if lines else 'layr-desk failed')
        # A wrong word would show in the reason: name no word.
        raise ValueError(re.sub(r'"[^"]*"', 'one word', reason).removeprefix('Error: ')[:300])
    mac_check(state_dir)
    from codex_vm_agents import refresh_readiness
    refresh_readiness(runtime)
    return {'unlocked': True}


def _target(parent: Path, prefix: str) -> Path:
    stamp = time.strftime('%Y-%m-%d %H.%M')
    for number in range(1, 100):
        target = parent / (f'{prefix} {stamp}' + ('' if number == 1 else f' {number}'))
        if not os.path.lexists(target):
            return target
    raise RuntimeError('No free folder name for the pull')


def start_pull(runtime: Any, project_id: str, request_id: str, to: str | None = None) -> dict[str, Any]:
    """Pull the latest vault state into a new folder below `to` (Downloads by default). A
    repeated request id returns the same job, so a lost reply never starts a second pull."""
    pulls, lock = _jobs(runtime, '_vault_pulls')
    with lock:
        if request_id in pulls:
            return dict(pulls[request_id])
        from codex_linux_workspaces import client as vm_client
        view = vault_view(vm_client(runtime).state_dir, project_id)
        if not view or view['state'] == 'none':
            raise ValueError('The project has no vault')
        if not view.get('site'):
            raise ValueError('The Desk site of this vault is not known; run codex_vault.py site')
        parent = Path(to).expanduser() if to else Path.home() / 'Downloads'
        if not parent.is_absolute() or not parent.is_dir():
            raise ValueError('The pull folder must be an existing folder')
        target = _target(parent, view['prefix'])
        command = [layr_desk(), 'pull', '--site', view['site'], '--project', view['prefix'],
                   '--to', str(target), '--json']
        job: dict[str, Any] = {'requestId': request_id, 'state': 'running', 'folder': str(target)}
        _keep(pulls, request_id, job)

    def run() -> None:
        try:
            done = subprocess.run(command, stdin=subprocess.DEVNULL, capture_output=True, text=True,
                                  timeout=PULL_TIMEOUT)
            if done.returncode:
                lines = (done.stderr or done.stdout).strip().splitlines()
                update = {'state': 'failed', 'error': (lines[-1] if lines else 'layr-desk failed').removeprefix('Error: ')[:500]}
            else:
                result = json.loads(done.stdout)
                update = {'state': 'done', 'seq': result['seq'], 'files': result['files'], 'bytes': result['bytes']}
        except (OSError, ValueError, KeyError, subprocess.TimeoutExpired) as error:
            update = {'state': 'failed', 'error': f'{type(error).__name__}: {str(error)[:400]}'}
        with lock:
            job.update(update)
    runtime.pool.submit(run)
    return dict(job)


def suggested_prefix(source: str) -> str:
    """A vault name from the project folder's name: lower case, - for other characters."""
    name = re.sub(r'[^a-z0-9_-]+', '-', Path(source).name.lower()).strip('-_')[:44].strip('-_')
    return name or 'project'


def _new_vault(state_dir: Path, project_id: str, prefix: str | None) -> tuple[str, str]:
    """The Mac folder and the vault name of a project without a vault."""
    view = vault_view(state_dir, project_id)
    if view and view['state'] != 'none':
        raise ValueError('The project has a vault already')
    source = _load(_folder(state_dir, project_id) / 'project.json').get('source')
    if not isinstance(source, str):
        raise ValueError('The project has no Mac folder')
    prefix = prefix or suggested_prefix(source)
    if not PREFIX.fullmatch(prefix) or prefix in ('api', 'assets'):
        raise ValueError('A vault name has up to 44 letters, digits, - and _, and is not api or assets')
    return source, prefix


def setup_facts(runtime: Any, project_id: str, prefix: str | None = None) -> dict[str, Any]:
    """What the setup dialog needs: the vault name, whether this Mac has its keys (layr-desk
    keys), the Desk site of the gateway, and the AWS profiles with their sign-in state."""
    from codex_linux_workspaces import client as vm_client
    _source, prefix = _new_vault(vm_client(runtime).state_dir, project_id, prefix)
    gateway = _load(_settings() / 'gateway.json')
    done = _desk([layr_desk(), 'aws-profiles', '--json'], stdin=subprocess.DEVNULL)
    if done.returncode:
        raise RuntimeError('layr-desk aws-profiles: ' + _reason(done.stderr or done.stdout))
    host, region, profile = gateway.get('host'), gateway.get('region'), gateway.get('profile')
    return {'prefix': prefix, 'keys': (_settings() / prefix / 'recipient.txt').is_file(),
            'site': f'https://{host}/' if isinstance(host, str) else None,
            'region': region if isinstance(region, str) else 'eu-central-1',
            'gatewayProfile': profile if isinstance(profile, str) else None,
            'profiles': json.loads(done.stdout)['profiles']}


def _refresh(runtime: Any) -> None:
    """Show a new vault in the sidebar now, not at the next tick."""
    from codex_linux_workspaces import client as vm_client
    from codex_vm_agents import refresh_readiness
    client = vm_client(runtime)
    check(client)
    mac_check(client.state_dir)
    refresh_readiness(runtime)


def start_setup(runtime: Any, project_id: str, request_id: str, prefix: str, profile: str,
                region: str) -> dict[str, Any]:
    """Make the project's vault with an AWS profile (layr-desk deploy): the stack, vault.json,
    the paper slot, the gateway registration, and the VM writer's keys through codex_vault.py
    configure. A repeated request id reports the same job; 'signin' says that the profile needs
    a sign-in (start_login) before a new request."""
    jobs, lock = _jobs(runtime, '_vault_setups')
    with lock:
        if request_id in jobs:
            return dict(jobs[request_id])
        if not PROFILE.fullmatch(profile) or not REGION.fullmatch(region):
            raise ValueError('Name an AWS profile and a region')
        from codex_linux_workspaces import client as vm_client
        source, prefix = _new_vault(vm_client(runtime).state_dir, project_id, prefix)
        if not (_settings() / prefix / 'recipient.txt').is_file():
            raise ValueError('Make the paper key of the vault first')
        writer = shlex.join([sys.executable, str(Path(__file__).resolve().parent / 'codex_vault.py'),
                             'configure', source])
        command = [layr_desk(), 'deploy', '--project', prefix, '--profile', profile, '--region', region,
                   '--writer', writer]
        job: dict[str, Any] = {'requestId': request_id, 'state': 'running', 'prefix': prefix}
        _keep(jobs, request_id, job)

    def run() -> None:
        update: dict[str, Any]
        try:
            with subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                                  stderr=subprocess.PIPE, text=True) as process:
                timer = threading.Timer(SETUP_TIMEOUT, process.kill)
                timer.start()
                lines = []
                assert process.stderr is not None and process.stdout is not None
                for line in process.stderr:
                    lines.append(line)
                    if line.startswith('layr-desk: '):
                        with lock:
                            job['progress'] = line.removeprefix('layr-desk: ').strip()[:200]
                output, code = process.stdout.read(), process.wait()
                timer.cancel()
            if code == 3:
                update = {'state': 'signin', 'error': _reason(''.join(lines))}
            elif code:
                update = {'state': 'failed', 'error': _reason(''.join(lines))}
            else:
                update = {'state': 'done', 'site': json.loads(output.strip().splitlines()[-1])['site']}
        except (OSError, ValueError, KeyError, IndexError) as error:
            update = {'state': 'failed', 'error': f'{type(error).__name__}: {str(error)[:400]}'}
        if update['state'] == 'done':
            try:
                _refresh(runtime)
            except Exception as error:  # noqa: BLE001 - the next tick shows the vault
                print('vault refresh after the setup skipped: ' + str(error)[:300], file=sys.stderr)
        with lock:
            job.pop('progress', None)
            job.update(update)
    runtime.pool.submit(run)
    return dict(job)


def start_login(runtime: Any, profile: str, request_id: str) -> dict[str, Any]:
    """Sign in to an AWS profile (layr-desk aws-login): the job shows the sign-in page and its
    code until the person confirms them in the browser."""
    jobs, lock = _jobs(runtime, '_vault_logins')
    with lock:
        if request_id in jobs:
            return dict(jobs[request_id])
        if not PROFILE.fullmatch(profile):
            raise ValueError('Name an AWS profile')
        command = [layr_desk(), 'aws-login', '--profile', profile, '--json']
        job: dict[str, Any] = {'requestId': request_id, 'state': 'running'}
        _keep(jobs, request_id, job)

    def run() -> None:
        update: dict[str, Any]
        try:
            with subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                                  stderr=subprocess.PIPE, text=True) as process:
                timer = threading.Timer(LOGIN_TIMEOUT, process.kill)
                timer.start()
                assert process.stderr is not None and process.stdout is not None
                for line in process.stdout:
                    shown = json.loads(line)
                    if isinstance(shown.get('page'), str) and shown['page'].startswith('https://'):
                        with lock:
                            job.update(page=shown['page'], code=str(shown.get('code', ''))[:40])
                stderr, code = process.stderr.read(), process.wait()
                timer.cancel()
            update = {'state': 'done'} if code == 0 else {'state': 'failed', 'error': _reason(stderr)}
        except (OSError, ValueError) as error:
            update = {'state': 'failed', 'error': f'{type(error).__name__}: {str(error)[:400]}'}
        with lock:
            job.update(update)
    runtime.pool.submit(run)
    return dict(job)
