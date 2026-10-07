"""Persistent Linux VM host and bounded JSON-RPC guest client.

A backend exit closes client channels. It does not stop the helper or guest processes.
Mutation calls are never retried after a response is lost.
"""
from __future__ import annotations

import base64
from contextlib import contextmanager
from dataclasses import dataclass, asdict
import fcntl
import gzip
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import socket
import subprocess
import tarfile
import tempfile
import time
from typing import Any, Iterator
import uuid
import zlib

from codex_state import state_dir as studio_state_dir

_GIB = 1024 ** 3
_IMAGE_URL = 'https://cloud-images.ubuntu.com/releases/noble/release-20260926/ubuntu-24.04-server-cloudimg-arm64.tar.gz'
_IMAGE_SHA256 = '1800eb56b6b839a08c020551e16fbd8c95b2772452ecce3737a9ad25fe594126'
_KERNEL_URL = 'https://cloud-images.ubuntu.com/releases/noble/release-20260926/unpacked/ubuntu-24.04-server-cloudimg-arm64-vmlinuz-generic'
_KERNEL_SHA256 = '71fe6776ef591ea452661a5933e11d2044c5df6aaf76cf39b8d9655fa4e47904'
_INITRD_URL = 'https://cloud-images.ubuntu.com/releases/noble/release-20260926/unpacked/ubuntu-24.04-server-cloudimg-arm64-initrd-generic'
_INITRD_SHA256 = 'f77f65eccd619be537e44a7bb2664048e27f20fc8c8e7d4eeb5cf63d0c8cd213'
_MAX_FRAME = 2 * 1024 * 1024


class LinuxVMError(RuntimeError):
    def __init__(self, message: str, *, uncertain: bool = False):
        super().__init__(message)
        self.uncertain = uncertain


@dataclass(frozen=True)
class Settings:
    cpus: int = 4
    memoryBytes: int = 4 * _GIB
    systemDiskBytes: int = 16 * _GIB
    dataDiskBytes: int = 128 * _GIB

    def validate(self) -> None:
        values = asdict(self)
        if any(type(value) is not int for value in values.values()):
            raise LinuxVMError('VM resource limits must be integers.')
        if not 1 <= self.cpus <= (os.cpu_count() or 1):
            raise LinuxVMError('VM CPU count exceeds the available host CPUs.')
        if not _GIB <= self.memoryBytes <= 64 * _GIB:
            raise LinuxVMError('VM memory must be between 1 and 64 GiB.')
        if platform.system() == 'Darwin':
            host_memory = int(_run(['sysctl', '-n', 'hw.memsize'], timeout=5).strip())
            if self.memoryBytes > host_memory * 3 // 4:
                raise LinuxVMError('VM memory exceeds 75 percent of host memory.')
        if not 8 * _GIB <= self.systemDiskBytes <= 128 * _GIB:
            raise LinuxVMError('VM system disk must be between 8 and 128 GiB.')
        if not 8 * _GIB <= self.dataDiskBytes <= 1024 * _GIB:
            raise LinuxVMError('VM data disk must be between 8 and 1024 GiB.')


def _run(argv: list[str], *, timeout: float, **kwargs: Any) -> str:
    try:
        result = subprocess.run(argv, check=False, capture_output=True, text=True,
                                timeout=timeout, **kwargs)
    except (OSError, subprocess.TimeoutExpired) as exc:
        raise LinuxVMError(f'VM command failed ({argv[0]}): {exc}') from exc
    if result.returncode:
        raise LinuxVMError(f'VM command failed ({argv[0]}): {result.stderr[-2000:].strip()}')
    return result.stdout


def _atomic_json(path: Path, value: Any) -> None:
    descriptor, name = tempfile.mkstemp(dir=path.parent, prefix=path.name + '.')
    try:
        with os.fdopen(descriptor, 'w') as output:
            json.dump(value, output)
            output.flush()
            os.fsync(output.fileno())
        os.replace(name, path)
    finally:
        Path(name).unlink(missing_ok=True)


def _require_space(directory: Path, *, create: bool) -> int:
    floor_name = 'CODEX_WORKSPACE_MIN_FREE_BYTES' if create else 'CODEX_WORKSPACE_AGENT_MIN_FREE_BYTES'
    try:
        floor = int(os.environ.get(floor_name, str((20 if create else 5) * _GIB)))
    except ValueError as exc:
        raise LinuxVMError(f'{floor_name} must be a non-negative integer.') from exc
    if floor < 0:
        raise LinuxVMError(f'{floor_name} must be a non-negative integer.')
    free = shutil.disk_usage(directory).free
    if free < floor:
        raise LinuxVMError(f'Not enough free disk space for the Linux VM: {free} free, {floor} required (bytes).')
    return free


def _remaining(deadline: float, maximum: float) -> float:
    remaining = deadline - time.monotonic()
    if remaining <= 0:
        raise LinuxVMError('The Linux VM operation exceeded its timeout.')
    return min(maximum, remaining)


def _version(executable: str, name: str, timeout: float = 15) -> str:
    output = _run([executable, '--version'], timeout=timeout)
    match = re.search(r'(?<![\d.])(\d+\.\d+\.\d+(?:-[a-zA-Z0-9.]+)?)', output)
    if not match:
        raise LinuxVMError(f'Cannot identify the installed {name} CLI version.')
    return match[1]


def _extract_raw(archive: Path, target: Path, maximum: int, *, timeout: float = 180) -> None:
    """Extract only the raw disk; preserve zero extents without trusting tar paths."""
    deadline = time.monotonic() + timeout
    with tarfile.open(archive, 'r:gz') as source:
        candidates = [member for member in source if member.isfile() and member.name.endswith('.img')]
        if len(candidates) != 1 or candidates[0].size > maximum:
            raise LinuxVMError('The Ubuntu archive has no unique raw disk within the configured limit.')
        member = candidates[0]
        stream = source.extractfile(member)
        if stream is None:
            raise LinuxVMError('Cannot read the Ubuntu raw disk.')
        with target.open('xb') as output:
            output.truncate(maximum)
            header = stream.read(1024 * 1024)
            if header[1080:1082] != b'\x53\xef':
                raise LinuxVMError('The Ubuntu image is not a raw ext4 root filesystem.')
            remaining = member.size
            block = header
            while remaining:
                if time.monotonic() > deadline:
                    raise LinuxVMError('Ubuntu raw disk extraction exceeded its timeout.')
                if not block:
                    raise LinuxVMError('The Ubuntu disk archive is truncated.')
                if block.strip(b'\0'):
                    output.write(block)
                else:
                    output.seek(len(block), os.SEEK_CUR)
                remaining -= len(block)
                block = stream.read(min(1024 * 1024, remaining))
            output.flush()
            os.fsync(output.fileno())
        target.chmod(0o600)


def _download(url: str, checksum: str, target: Path, maximum: int, timeout: float) -> None:
    _run(['curl', '--fail', '--location', '--silent', '--show-error', '--connect-timeout', '20',
          '--max-time', str(timeout), '--max-filesize', str(maximum), '-o', str(target), url], timeout=timeout)
    with target.open('rb') as source:
        digest = hashlib.file_digest(source, 'sha256').hexdigest()
    if digest != checksum:
        raise LinuxVMError(f'The Ubuntu SHA-256 checksum does not match: {target.name}.')


def _kernel_image(compressed: bytes) -> bytes:
    """Accept an arm64 Image or the gzip payload in Ubuntu's EFI zboot wrapper."""
    if compressed[56:60] == b'ARM\x64':
        return compressed
    offset = compressed.find(b'\x1f\x8b\x08')
    if offset < 0:
        raise LinuxVMError('The Ubuntu kernel has no supported gzip Image payload.')
    decoder = zlib.decompressobj(16 + zlib.MAX_WBITS)
    try:
        image = decoder.decompress(compressed[offset:], 128 * 1024 * 1024 + 1)
    except zlib.error as exc:
        raise LinuxVMError('The Ubuntu kernel gzip payload is invalid.') from exc
    if not decoder.eof or len(image) > 128 * 1024 * 1024 or image[56:60] != b'ARM\x64':
        raise LinuxVMError('The Ubuntu kernel payload is not a bounded arm64 Image.')
    return image


def _cloud_config(guest_dir: Path, codex_version: str, claude_version: str) -> dict[str, Any]:
    """Return cloud-init data. Guest service installation is a reviewed guest contract."""
    files = []
    for path in sorted(guest_dir.rglob('*')):
        if (path.is_file() and not path.is_symlink() and '__pycache__' not in path.parts
                and path.suffix in {'.py', '.sh', '.service'} and not path.name.startswith('test')):
            files.append({'path': '/opt/codex-studio/vm/guest/' + path.relative_to(guest_dir).as_posix(),
                          'permissions': '0644', 'encoding': 'gz+b64',
                          'content': base64.b64encode(gzip.compress(path.read_bytes())).decode()})
    if not (guest_dir / 'install.sh').is_file():
        raise LinuxVMError('The Linux VM guest install.sh payload is unavailable.')
    scripts = guest_dir.parents[1] / 'scripts'
    for name in ['codex_workspace_images.py', 'codex_workspace_linux.py', 'codex_process_supervisor.py',
                 'codex_open_file_limit.py']:
        source = scripts / name
        if not source.is_file():
            raise LinuxVMError(f'The Linux VM runtime payload is unavailable: {name}.')
        files.append({'path': '/opt/codex-studio/scripts/' + name, 'permissions': '0644',
                      'encoding': 'gz+b64', 'content': base64.b64encode(gzip.compress(source.read_bytes())).decode()})
    bridge = scripts / 'claude_bridge'
    for source in sorted(bridge.iterdir()):
        if source.is_file() and (source.name in {'package.json', 'package-lock.json'} or
                                 (source.suffix == '.mjs' and not source.name.endswith('.test.mjs') and
                                  source.name != 'vitest.config.mjs')):
            files.append({'path': '/opt/codex-studio/claude_bridge/' + source.name, 'permissions': '0644',
                          'encoding': 'gz+b64', 'content': base64.b64encode(gzip.compress(source.read_bytes())).decode()})
    if not any(entry['path'].endswith('/claude_bridge/package-lock.json') for entry in files):
        raise LinuxVMError('The pinned Claude bridge dependency manifest is unavailable.')
    script = f'''#!/bin/bash
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive
printf 'Acquire::Retries "2"; Acquire::http::Timeout "30"; Acquire::https::Timeout "30";\\n' > /etc/apt/apt.conf.d/99studio-timeouts
mkdir -p /var/lib/codex-studio
apt-mark hold linux-generic linux-image-generic || true
trap 'printf "%s\\n" "Provision failed at line $LINENO" > /var/lib/codex-studio/provision-error; echo "STUDIO_PROVISION_ERROR: line $LINENO failed" >&2' ERR
timeout --kill-after=5 15 timedatectl set-ntp true
timeout --kill-after=5 120 bash -c 'until [ "$(timedatectl show --property=NTPSynchronized --value)" = yes ]; do sleep 1; done'
timeout --kill-after=5 300 apt-get update
timeout --kill-after=5 600 apt-get install -y --no-install-recommends git rsync btrfs-progs python3 python3-venv curl xz-utils ca-certificates util-linux lsof
if ! blkid /dev/vdb; then mkfs.btrfs -f -L studio-data /dev/vdb; fi
mkdir -p /var/lib/codex-studio
if ! grep -q 'LABEL=studio-data' /etc/fstab; then echo 'LABEL=studio-data /var/lib/codex-studio btrfs defaults,nofail,user_subvol_rm_allowed 0 0' >> /etc/fstab; fi
mountpoint -q /var/lib/codex-studio || mount /var/lib/codex-studio
if [ -f /var/lib/codex-studio/provision-ready ]; then exit 0; fi
rm -f /var/lib/codex-studio/provision-error
btrfs filesystem resize max /var/lib/codex-studio
curl --fail --location --connect-timeout 20 --max-time 180 -o /tmp/node.tar.xz https://nodejs.org/dist/v22.15.0/node-v22.15.0-linux-arm64.tar.xz
curl --fail --location --connect-timeout 20 --max-time 60 -o /tmp/node-sums https://nodejs.org/dist/v22.15.0/SHASUMS256.txt
(cd /tmp && grep ' node-v22.15.0-linux-arm64.tar.xz$' node-sums | sed 's/node-v22.15.0-linux-arm64.tar.xz/node.tar.xz/' | sha256sum -c -)
tar -xJf /tmp/node.tar.xz -C /usr/local --strip-components=1
rm /tmp/node.tar.xz /tmp/node-sums
export npm_config_fetch_timeout=30000 npm_config_fetch_retries=2
export npm_config_fetch_retry_mintimeout=1000 npm_config_fetch_retry_maxtimeout=5000
export npm_config_update_notifier=false
timeout --kill-after=5 300 npm --prefix /opt/codex-studio/claude_bridge ci --ignore-scripts --omit=optional --no-audit --no-fund
echo STUDIO_PROVISION_CODEX
timeout --kill-after=5 180 npm install -g --foreground-scripts --no-audit --no-fund @openai/codex@{codex_version}
echo STUDIO_PROVISION_CLAUDE
timeout --kill-after=5 180 npm install -g --foreground-scripts --no-audit --no-fund @anthropic-ai/claude-code@{claude_version}
test "$(codex --version)" = "codex-cli {codex_version}"
test "$(claude --version | cut -d' ' -f1)" = "{claude_version}"
# install.sh belongs to the guest component and defines its service boundary.
timeout --kill-after=5 120 bash /opt/codex-studio/vm/guest/install.sh
codex --version > /var/lib/codex-studio/provider-versions
claude --version >> /var/lib/codex-studio/provider-versions
touch /var/lib/codex-studio/provision-ready
rm -f /var/lib/codex-studio/provision-error
echo STUDIO_PROVISION_READY
'''
    files.append({'path': '/opt/codex-studio/provision.sh', 'permissions': '0700', 'content': script})
    files.append({'path': '/etc/systemd/system/codex-studio-provision.service', 'permissions': '0644', 'content': '''[Unit]
Description=Provision the Codex Studio Linux VM
After=network-online.target
Wants=network-online.target
ConditionPathExists=!/var/lib/codex-studio/provision-ready

[Service]
Type=oneshot
ExecStart=/bin/bash /opt/codex-studio/provision.sh
TimeoutStartSec=1200
TimeoutStopSec=5
KillMode=control-group
RemainAfterExit=yes
StandardOutput=journal+console
StandardError=journal+console

[Install]
WantedBy=multi-user.target
'''})
    return {'hostname': 'studio-linux', 'manage_etc_hosts': True, 'ssh_pwauth': False,
            'disable_root': True, 'users': [{'name': 'studio', 'lock_passwd': True,
                                          'shell': '/bin/bash'}],
            'write_files': files, 'runcmd': [['systemctl', 'daemon-reload'],
                ['systemctl', 'enable', 'codex-studio-provision.service'],
                ['timeout', '--kill-after=5', '1210', 'systemctl', 'start', 'codex-studio-provision.service']]}


class Client:
    def __init__(self, state_dir: str | Path | None = None, *, helper: str | Path | None = None):
        state = studio_state_dir()
        self.state_dir = Path(state_dir or os.environ.get('CODEX_LINUX_VM_STATE_DIR') or state / 'linux-vm').expanduser().resolve()
        root = Path(__file__).resolve().parents[1]
        packaged = root.parent / 'studio-linux-vm'
        self.helper = Path(helper or os.environ.get('CODEX_LINUX_VM_HELPER') or
                           (packaged if packaged.is_file() else root / 'desktop/native/linux-vm/studio-linux-vm'))
        self.guest_dir = root / 'vm/guest'
        self.socket_path = self.state_dir / 'control.sock'
        if len(os.fsencode(self.socket_path)) >= 104:
            raise LinuxVMError('VM socket path exceeds the macOS limit. Use a shorter state directory.')

    @contextmanager
    def _lock(self, timeout: float = 15) -> Iterator[None]:
        self.state_dir.mkdir(mode=0o700, parents=True, exist_ok=True)
        self.state_dir.chmod(0o700)
        with (self.state_dir / 'client.lock').open('a') as lease:
            deadline = time.monotonic() + timeout
            while True:
                try:
                    fcntl.flock(lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    break
                except BlockingIOError:
                    if time.monotonic() >= deadline:
                        raise LinuxVMError('Another VM operation exceeds the client lock timeout.')
                    time.sleep(0.05)
            try:
                yield
            finally:
                fcntl.flock(lease, fcntl.LOCK_UN)

    def _channel(self, timeout: float, *, guest: bool = False) -> socket.socket:
        channel = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        channel.settimeout(timeout)
        try:
            channel.connect(str(self.state_dir / "guest.sock" if guest else self.socket_path))
        except OSError as exc:
            channel.close()
            raise LinuxVMError(f'The Linux VM helper is unavailable: {exc}') from exc
        return channel

    @staticmethod
    def _read(channel: socket.socket, deadline: float, buffer: bytearray) -> dict[str, Any]:
        while b'\n' not in buffer:
            channel.settimeout(max(0.001, deadline - time.monotonic()))
            if time.monotonic() >= deadline:
                raise LinuxVMError('The Linux VM response exceeded its timeout.', uncertain=True)
            try:
                block = channel.recv(65536)
            except OSError as exc:
                raise LinuxVMError(f'The Linux VM response failed: {exc}', uncertain=True) from exc
            if not block:
                raise LinuxVMError('The Linux VM connection closed before its response.', uncertain=True)
            buffer.extend(block)
            first_line = buffer.find(b'\n')
            if (first_line if first_line >= 0 else len(buffer)) > _MAX_FRAME:
                raise LinuxVMError('The Linux VM response exceeds the frame limit.', uncertain=True)
        line, _, remainder = buffer.partition(b'\n')
        buffer[:] = remainder
        try:
            value = json.loads(line)
        except ValueError as exc:
            raise LinuxVMError('The Linux VM returned invalid JSON.', uncertain=True) from exc
        if not isinstance(value, dict):
            raise LinuxVMError('The Linux VM returned an invalid response.', uncertain=True)
        return value

    def _host(self, method: str, timeout: float = 5) -> dict[str, Any]:
        identity = str(uuid.uuid4())
        with self._channel(timeout) as channel:
            channel.sendall(json.dumps({'id': identity, 'method': method, 'params': {}}).encode() + b'\n')
            response = self._read(channel, time.monotonic() + timeout, bytearray())
            if response.get('id') != identity:
                raise LinuxVMError('The VM helper returned a different request identity.', uncertain=True)
            if 'error' in response:
                raise LinuxVMError(str(response['error']))
            return response['result']

    def status(self) -> dict[str, Any]:
        try:
            result = self._host('host.status')
        except LinuxVMError:
            if self.socket_path.exists() or (self.state_dir / 'host.lock').exists():
                # A stale socket is not evidence that a running helper can be replaced.
                with (self.state_dir / 'host.lock').open('a') as lease:
                    try:
                        fcntl.flock(lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    except BlockingIOError:
                        raise LinuxVMError('The VM helper owns its lease but does not respond.')
            result = {'state': 'stopped'}
        if self.state_dir.exists():
            result['allocatedDiskBytes'] = sum(path.stat().st_blocks * 512 for path in self.state_dir.glob('*.raw'))
            result['freeHostBytes'] = shutil.disk_usage(self.state_dir).free
        return result

    def create(self, settings: Settings | None = None, *, timeout: float = 1500) -> dict[str, Any]:
        if not 0 < timeout <= 3600:
            raise LinuxVMError('VM create timeout must be between 0 and 3600 seconds.')
        deadline = time.monotonic() + timeout
        settings = settings or Settings(**self.get_settings())
        settings.validate()
        with self._lock(_remaining(deadline, 15)):
            if self.status()['state'] != 'stopped':
                raise LinuxVMError('Stop the Linux VM before changing its resource limits.')
            # A backend termination can leave its private image staging directory.
            # The client lock proves that no other create operation uses it.
            for abandoned in self.state_dir.glob('create-*'):
                _remaining(deadline, 1)
                if abandoned.is_dir() and not abandoned.is_symlink():
                    shutil.rmtree(abandoned)
            manifest = self.state_dir / 'config.json'
            if manifest.exists():
                current = json.loads(manifest.read_text())
                for name, field in [('system.raw', 'systemDiskBytes'), ('data.raw', 'dataDiskBytes')]:
                    if getattr(settings, field) < current[field]:
                        raise LinuxVMError('VM disks cannot shrink. Keep the existing disk limit.')
                    with (self.state_dir / name).open('r+b') as disk:
                        size = getattr(settings, field)
                        if size > disk.seek(0, os.SEEK_END):
                            disk.seek(size - 1)
                            disk.write(b"\0")
                _atomic_json(manifest, asdict(settings))
                return self.status()
            _require_space(self.state_dir, create=True)
            codex_version = _version(os.environ.get('CODEX_BIN') or shutil.which('codex') or 'codex', 'Codex', _remaining(deadline, 15))
            claude_version = _version(os.environ.get('CLAUDE_BIN') or shutil.which('claude') or 'claude', 'Claude', _remaining(deadline, 15))
            config = _cloud_config(self.guest_dir, codex_version, claude_version)
            staging = Path(tempfile.mkdtemp(prefix='create-', dir=self.state_dir))
            try:
                archive = staging / 'ubuntu.tar.gz'
                _download(_IMAGE_URL, _IMAGE_SHA256, archive, 1024 ** 3, _remaining(deadline, 900))
                _download(_KERNEL_URL, _KERNEL_SHA256, staging / 'vmlinuz', 64 * 1024 * 1024, _remaining(deadline, 120))
                _download(_INITRD_URL, _INITRD_SHA256, staging / 'initrd', 128 * 1024 * 1024, _remaining(deadline, 120))
                (staging / 'kernel').write_bytes(_kernel_image((staging / 'vmlinuz').read_bytes()))
                (staging / 'vmlinuz').unlink()
                _require_space(self.state_dir, create=True)
                _extract_raw(archive, staging / 'system.raw', settings.systemDiskBytes, timeout=_remaining(deadline, 180))
                archive.unlink()
                with (staging / 'data.raw').open('xb') as disk:
                    disk.truncate(settings.dataDiskBytes)
                (staging / 'data.raw').chmod(0o600)
                seed = staging / 'seed'
                seed.mkdir()
                (seed / 'user-data').write_text('#cloud-config\n' + json.dumps(config))
                (seed / 'meta-data').write_text('instance-id: studio-linux-v1\nlocal-hostname: studio-linux\n')
                _run(['hdiutil', 'makehybrid', '-iso', '-joliet', '-default-volume-name', 'cidata',
                      '-o', str(staging / 'seed.iso'), str(seed)], timeout=_remaining(deadline, 60))
                _remaining(deadline, 1)
                for name in ['system.raw', 'data.raw', 'seed.iso', 'kernel', 'initrd']:
                    os.replace(staging / name, self.state_dir / name)
                _atomic_json(manifest, asdict(settings))
                _atomic_json(self.state_dir / 'image.json', {'url': _IMAGE_URL, 'sha256': _IMAGE_SHA256,
                                                          'kernelSha256': _KERNEL_SHA256, 'initrdSha256': _INITRD_SHA256,
                                                          'codex': codex_version, 'claude': claude_version})
            finally:
                shutil.rmtree(staging)
        return self.status()

    def get_settings(self) -> dict[str, int]:
        for name in ['config.json', 'settings.json']:
            path = self.state_dir / name
            if path.exists():
                return asdict(Settings(**json.loads(path.read_text())))
        return asdict(Settings(cpus=min(4, os.cpu_count() or 1)))

    def set_settings(self, values: dict[str, int]) -> dict[str, int]:
        settings = Settings(**values)
        settings.validate()
        if (self.state_dir / 'config.json').exists():
            self.create(settings)
        else:
            with self._lock():
                _atomic_json(self.state_dir / 'settings.json', asdict(settings))
        return asdict(settings)

    def ensure_running(self, settings: Settings | dict[str, int] | None = None, *, timeout: float = 1500) -> dict[str, Any]:
        if not 0 < timeout <= 3600:
            raise LinuxVMError('VM start timeout must be between 0 and 3600 seconds.')
        deadline = time.monotonic() + timeout
        if platform.system() != 'Darwin' or platform.machine() != 'arm64':
            raise LinuxVMError('Linux VM workspaces require macOS on Apple silicon.')
        if isinstance(settings, dict):
            settings = Settings(**settings)
        if not (self.state_dir / 'config.json').exists():
            self.create(settings, timeout=_remaining(deadline, timeout))
        elif settings is not None and asdict(settings) != self.get_settings():
            self.create(settings, timeout=_remaining(deadline, timeout))
        launched = None
        with self._lock(_remaining(deadline, 15)):
            _remaining(deadline, 1)
            state = self.status()
            if state['state'] == 'stopped':
                _require_space(self.state_dir, create=False)
                if not self.helper.is_file() or not os.access(self.helper, os.X_OK):
                    raise LinuxVMError('The signed Linux VM helper is unavailable. Build desktop/native/linux-vm first.')
                log_path = self.state_dir / 'helper.log'
                if log_path.exists() and log_path.stat().st_size > 1024 * 1024:
                    log_path.write_bytes(log_path.read_bytes()[-512 * 1024:])
                with log_path.open('ab') as log:
                    launched = subprocess.Popen([str(self.helper), 'serve', str(self.state_dir)], stdin=subprocess.DEVNULL,
                                     stdout=log, stderr=log, start_new_session=True, close_fds=True)
                start_deadline = min(deadline, time.monotonic() + 15)
                while time.monotonic() < start_deadline:
                    if launched.poll() is not None:
                        details = log_path.read_text(errors='replace')[-2000:]
                        raise LinuxVMError(f'The Linux VM helper exited with code {launched.returncode}: {details}')
                    try:
                        self._host('host.status', timeout=1)
                        break
                    except LinuxVMError:
                        time.sleep(0.05)
                else:
                    raise LinuxVMError('The Linux VM helper did not publish its control socket within 15 seconds.', uncertain=True)
        while time.monotonic() < deadline:
            console = self.state_dir / 'console.log'
            if console.exists():
                diagnostics = console.read_text(errors='replace')
                if diagnostics.rfind('STUDIO_PROVISION_ERROR:') > diagnostics.rfind('STUDIO_PROVISION_READY'):
                    raise LinuxVMError('Linux VM provisioning failed. ' + diagnostics[-2000:])
            if launched is not None and launched.poll() is not None:
                details = (self.state_dir / 'helper.log').read_text(errors='replace')[-2000:]
                raise LinuxVMError(f'The Linux VM helper exited with code {launched.returncode}: {details}')
            state = self.status()
            if state['state'] == 'failed':
                raise LinuxVMError(f"The Linux VM failed: {state.get('error')}")
            if state['state'] == 'running':
                try:
                    health = self.call('health', {}, timeout=min(5, max(0.01, deadline - time.monotonic())))
                except LinuxVMError:
                    pass
                else:
                    self._wait_guest_clock(deadline)
                    return {**state, 'health': health}
            time.sleep(0.25)
        diagnostics = self.state_dir / ('console.log' if (self.state_dir / 'console.log').exists() else 'helper.log')
        details = diagnostics.read_text(errors='replace')[-2000:] if diagnostics.exists() else ''
        raise LinuxVMError(f'The Linux VM did not become ready within {timeout} seconds. {details}')

    def _wait_guest_clock(self, deadline: float) -> None:
        # Direct Linux boot starts without an RTC. TLS needs a current guest clock.
        seconds = max(1, int(_remaining(deadline, 120)))
        result = self.call('exec', {'argv': ['timeout', '--kill-after=5', str(seconds), 'bash', '-c',
            'until [ "$(timedatectl show --property=NTPSynchronized --value)" = yes ]; do sleep 1; done'],
            'cwd': '/home/studio', 'timeoutSeconds': seconds + 5}, timeout=_remaining(deadline, seconds + 10))
        if result.get('exitCode') != 0:
            raise LinuxVMError('The Linux VM clock did not synchronize within its timeout.')

    def stop(self, *, timeout: float = 40) -> dict[str, Any]:
        with self._lock():
            if self.status()['state'] == 'stopped':
                return {'state': 'stopped'}
            self._host('host.stop')
            deadline = time.monotonic() + timeout
            while time.monotonic() < deadline:
                if self.status()['state'] == 'stopped':
                    return {'state': 'stopped'}
                time.sleep(0.1)
        raise LinuxVMError('The Linux VM did not stop within its timeout.', uncertain=True)

    def stream(self, method: str, params: dict[str, Any] | None = None, *, request_id: str | None = None,
               timeout: float = 60) -> Iterator[dict[str, Any]]:
        identity = request_id if request_id is not None else str(uuid.uuid4())
        if not isinstance(identity, str) or not 1 <= len(identity) <= 128:
            raise LinuxVMError('Guest request IDs must have 1 to 128 characters.')
        if not 0 < timeout <= 3600:
            raise LinuxVMError('Guest request timeout must be between 0 and 3600 seconds.')
        if method in {'workspace.startBase', 'workspace.create', 'upload.begin', 'upload.commit', 'sync.push'}:
            _require_space(self.state_dir, create=False)
        sent = False
        deadline = time.monotonic() + timeout
        try:
            with self._channel(min(timeout, 15), guest=True) as channel:
                buffer = bytearray()
                frame = json.dumps({'id': identity, 'method': method, 'params': params or {}}).encode() + b'\n'
                if len(frame) > _MAX_FRAME:
                    raise LinuxVMError('The Linux VM request exceeds the frame limit.')
                sent = True
                channel.sendall(frame)
                while True:
                    response = self._read(channel, deadline, buffer)
                    if response.get('id') != identity:
                        raise LinuxVMError('The guest returned a different request identity.', uncertain=True)
                    if 'error' in response:
                        error = response['error']
                        code = error.get('code') if isinstance(error, dict) else None
                        raise LinuxVMError(f"Linux guest {method}: {error}", uncertain=code == 'outcome_unknown')
                    yield response
                    if 'result' in response:
                        return
        except LinuxVMError:
            raise
        except (OSError, ValueError) as exc:
            raise LinuxVMError(f'Linux guest {method} failed: {exc}', uncertain=sent) from exc

    def call(self, method: str, params: dict[str, Any] | None = None, *, request_id: str | None = None,
             timeout: float = 60) -> Any:
        for response in self.stream(method, params, request_id=request_id, timeout=timeout):
            if 'result' in response:
                return response['result']
        raise LinuxVMError('The Linux guest returned no result.', uncertain=True)


    def close(self) -> None:
        """Connections close after each request. No VM lifecycle action occurs."""

    def __enter__(self) -> 'Client':
        return self

    def __exit__(self, *args: Any) -> None:
        self.close()

    request = call


def connect(state_dir: str | Path | None = None, **kwargs: Any) -> Client:
    """Return a reconnectable guest client. Each request owns one Unix connection."""
    return Client(state_dir, **kwargs)


def ensure_running(settings: Settings | dict[str, int] | None = None, **kwargs: Any) -> dict[str, Any]:
    return Client().ensure_running(settings, **kwargs)


def get_settings() -> dict[str, int]:
    return Client().get_settings()


def set_settings(values: dict[str, int]) -> dict[str, int]:
    return Client().set_settings(values)


def status() -> dict[str, Any]:
    return Client().status()


def stop(**kwargs: Any) -> dict[str, Any]:
    return Client().stop(**kwargs)


def main() -> None:
    import argparse
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('command', choices=['create', 'start', 'stop', 'status', 'call'])
    parser.add_argument('--state-dir')
    parser.add_argument('--helper')
    parser.add_argument('--settings', help='JSON resource limits')
    parser.add_argument('--method')
    parser.add_argument('--params', default='{}')
    args = parser.parse_args()
    client = Client(args.state_dir, helper=args.helper)
    try:
        if args.command == 'create':
            result = client.create(Settings(**json.loads(args.settings)) if args.settings else None)
        elif args.command == 'start':
            result = client.ensure_running(json.loads(args.settings) if args.settings else None)
        elif args.command == 'stop':
            result = client.stop()
        elif args.command == 'status':
            result = client.status()
        else:
            if not args.method:
                parser.error('--method is required for call')
            result = client.call(args.method, json.loads(args.params))
        print(json.dumps(result))
    except LinuxVMError as exc:
        print(json.dumps({'error': str(exc), 'uncertain': exc.uncertain}))
        raise SystemExit(1)


if __name__ == '__main__':
    main()
