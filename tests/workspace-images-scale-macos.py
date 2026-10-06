"""Measure image workspace start and the first status on large folders."""

import io
import json
import os
import pathlib
import shutil
import subprocess
import sys
import tarfile
import tempfile
import threading
import time

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1] / 'scripts'))

import codex_workspace_images as images
from codex_workspace_macos import Backend


def git(root, *args):
    return subprocess.run(['git', '-C', str(root), *args], check=True,
                          capture_output=True, text=True, timeout=1800).stdout.strip()


def make_files(root, count, archive_path):
    payload = b'scale payload\n'
    with tarfile.open(archive_path, 'w', format=tarfile.GNU_FORMAT) as archive:
        for index in range(count):
            folder = f'payload/d{index % 200:03d}'
            info = tarfile.TarInfo(f'{folder}/f{index:06d}.txt')
            info.size = len(payload)
            archive.addfile(info, io.BytesIO(payload))
    root.mkdir(parents=True)
    subprocess.run(['tar', '-xf', str(archive_path), '-C', str(root)], check=True, timeout=900)
    archive_path.unlink()


def main():
    if sys.platform != 'darwin' or not shutil.which('diskutil'):
        raise SystemExit('requires macOS diskutil')
    count = int(sys.argv[1]) if len(sys.argv) > 1 else 200000
    if count not in {50000, 200000}:
        raise SystemExit('path count must be 50000 or 200000')
    temp = tempfile.TemporaryDirectory(prefix=f'workspace-copy-scale-{count}-')
    root = pathlib.Path(temp.name)
    folder, store = root / 'folder', root / 'store'
    old_store = os.environ.get('CODEX_WORKSPACE_STORE')
    os.environ['CODEX_WORKSPACE_STORE'] = str(store)
    agent_id = f'copy-scale-{count}'
    try:
        make_files(folder, count, root / 'files.tar')
        git(folder, 'init', '-q')
        git(folder, 'config', 'user.name', 'Scale Test')
        git(folder, 'config', 'user.email', 'scale@example.invalid')
        git(folder, 'add', '-A')
        git(folder, 'commit', '-m', f'{count} path fixture')

        done = threading.Event()
        outcome = []
        images.start_base_build(folder, lambda value: (outcome.append(value), done.set()))
        if not done.wait(1800) or outcome[-1]['state'] != 'ready':
            raise RuntimeError(f'base build failed: {outcome[-1] if outcome else None}')

        changed = folder / 'payload/d000/f000000.txt'
        changed.write_text('user edited this file\n')
        git(folder, 'add', 'payload/d000/f000000.txt')
        start = time.monotonic()
        workspace = images.create_workspace(folder, agent_id)
        agent_start_seconds = time.monotonic() - start
        agent_root = pathlib.Path(workspace['path'])
        status_start = time.monotonic()
        status = git(agent_root, 'status', '--porcelain')
        first_status_seconds = time.monotonic() - status_start
        if status:
            raise AssertionError(f'workspace status was not clean after the user staged a file: {status[:200]}')
        if (agent_root / 'payload/d000/f000000.txt').read_text() != 'user edited this file\n':
            raise AssertionError("workspace did not include the user's edit")
        print(json.dumps({'paths': count, 'agentStartSeconds': round(agent_start_seconds, 3),
                          'firstGitStatusSeconds': round(first_status_seconds, 3)}, sort_keys=True))
    finally:
        try:
            images.remove_workspace(agent_id)
        except (OSError, RuntimeError, ValueError):
            pass
        if store.exists():
            for state_file in store.glob('bases/*/base.json'):
                state = images._read_json(state_file, {}) or {}
                versions = state_file.parent / 'versions'
                for version in versions.iterdir() if versions.exists() else ():
                    try:
                        Backend().remove_base_version(version)
                    except (OSError, RuntimeError):
                        pass
        if old_store is None:
            os.environ.pop('CODEX_WORKSPACE_STORE', None)
        else:
            os.environ['CODEX_WORKSPACE_STORE'] = old_store
        temp.cleanup()


if __name__ == '__main__':
    main()
