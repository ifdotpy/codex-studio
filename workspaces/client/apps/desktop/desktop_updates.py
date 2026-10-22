"""Local owner of the desktop installer's staged-update files and signature checks.

The installer reads staged update files and uses these local signature checks.
"""
from __future__ import annotations

from collections.abc import Callable
import json
import math
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
from typing import TypedDict, cast

APP_NAME = 'Codex Studio.app'
BUNDLE_ID = 'local.codex.agents'
# Owner: runtime-services/src/peer_updates.rs; desktop_installer_contract_matches_rust_owner
# checks the build-stamp path, tool paths and deadlines against this consumer.
BUILD_STAMP = Path('Contents/Resources/studio-build.json')
# State directory filenames shared with the Rust peer-update writer.
PENDING_FILE = 'pending-update.json'
RECEIPT_FILE = 'update-receipt.json'
UPDATES_DIR = 'updates'
# Tool deadlines, in seconds.
DITTO_SECONDS = 1800
CODESIGN_SECONDS = 300
DITTO = '/usr/bin/ditto'
CODESIGN = '/usr/bin/codesign'
Runner = Callable[[list[str], float], subprocess.CompletedProcess[str]]
class Build(TypedDict):
    version: int
    builtAt: float
    commit: str
    commitTime: int
    branch: str


PendingUpdate = TypedDict('PendingUpdate', {
    'id': str, 'app': str, 'build': Build, 'from': str,
})


class UpdateError(Exception):
    """A step failed. The puller records the reason and skips the build for an hour."""


def run_tool(argv: list[str], timeout: float) -> subprocess.CompletedProcess[str]:
    return subprocess.run(argv, capture_output=True, text=True, timeout=timeout, check=False,
                          stdin=subprocess.DEVNULL)


def valid_build(value: object) -> Build | None:
    """The build stamp, or None when the value is not a complete stamp."""
    if not isinstance(value, dict) or value.get('version') != 1:
        return None
    value = cast(dict[str, object], value)
    built, commit, commit_time, branch = (value.get('builtAt'), value.get('commit'),
                                          value.get('commitTime'), value.get('branch'))
    if (isinstance(built, bool) or not isinstance(built, (int, float)) or not math.isfinite(built) or built <= 0
            or not isinstance(commit, str) or len(commit) not in (40, 64)
            or any(c not in '0123456789abcdef' for c in commit)
            or type(commit_time) is not int or commit_time <= 0
            or not isinstance(branch, str) or len(branch) > 255):
        return None
    return {'version': 1, 'builtAt': built, 'commit': commit, 'commitTime': commit_time, 'branch': branch}


def read_build(app: Path) -> Build | None:
    try:
        return valid_build(json.loads((app / BUILD_STAMP).read_text()))
    except (OSError, ValueError):
        return None


def build_folder(root: Path, build: Build) -> Path:
    return root / UPDATES_DIR / str(build['builtAt'])


def write_json(path: Path, value: object) -> None:
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    descriptor, name = tempfile.mkstemp(dir=path.parent, prefix=path.name + '.')
    try:
        with os.fdopen(descriptor, 'w') as output:
            json.dump(value, output, sort_keys=True)
            output.flush()
            os.fsync(output.fileno())
        os.replace(name, path)
    finally:
        Path(name).unlink(missing_ok=True)


def read_json(path: Path) -> dict[str, object] | None:
    try:
        value = json.loads(path.read_text())
    except (OSError, ValueError):
        return None
    return cast(dict[str, object], value) if isinstance(value, dict) else None


def read_pending(root: Path) -> PendingUpdate | None:
    """The staged update, or None. The app must be a staged copy in this state directory."""
    value = read_json(root / PENDING_FILE)
    if value is None:
        return None
    build = valid_build(value.get('build'))
    app, source, key = value.get('app'), value.get('from'), value.get('id')
    if build is None or not isinstance(app, str) or not isinstance(source, str) or not isinstance(key, str):
        return None
    if Path(app).resolve() != (build_folder(root, build) / APP_NAME).resolve():
        return None
    return {'id': key, 'app': app, 'build': build, 'from': source}


def remove_tree(path: Path) -> None:
    if path.is_symlink() or path.is_file():
        path.unlink(missing_ok=True)
    elif path.exists():
        shutil.rmtree(path, ignore_errors=True)


def _tail(result: subprocess.CompletedProcess[str]) -> str:
    text = ((result.stderr or '') + (result.stdout or '')).strip()
    return text[-300:]


def requirement(app: Path, run: Runner = run_tool) -> str:
    """The designated requirement of a signed app: it names the signing certificate."""
    result = run([CODESIGN, '-d', '-r-', str(app)], CODESIGN_SECONDS)
    if result.returncode == 0:
        for line in ((result.stdout or '') + '\n' + (result.stderr or '')).splitlines():
            if line.startswith('designated => '):
                return line.removeprefix('designated => ').strip()
    raise UpdateError(f'The designated requirement of {app.name} is unavailable')


def verify_app(app: Path, installed: Path, build: Build, run: Runner = run_tool) -> None:
    """Accept only a complete app signed by the certificate of the installed app, with the
    offered build stamp."""
    if app.is_symlink() or not app.is_dir():
        raise UpdateError('The staged app is missing')
    result = run([CODESIGN, '--verify', '--deep', '--strict', str(app)], CODESIGN_SECONDS)
    if result.returncode != 0:
        raise UpdateError('The signature check failed: ' + _tail(result))
    if requirement(app, run) != requirement(installed, run):
        raise UpdateError('Another certificate signed the build')
    if read_build(app) != build:
        raise UpdateError('The build stamp of the app differs from the offer')
