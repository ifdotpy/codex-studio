"""Measure image workspace start and the first status on large folders."""

import json
import io
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
import codex_workspace_macos as macos
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
        phase_start = time.monotonic()
        make_files(folder, count, root / 'files.tar')
        git(folder, 'init', '-q')
        git(folder, 'config', 'user.name', 'Scale Test')
        git(folder, 'config', 'user.email', 'scale@example.invalid')
        git(folder, 'add', '-A')
        git(folder, 'commit', '-m', f'{count} path fixture')
        print(json.dumps({'phase': 'git-fixture', 'paths': count,
                          'seconds': round(time.monotonic() - phase_start, 3)}), flush=True)

        phase_start = time.monotonic()
        done = threading.Event()
        outcome = []
        images.start_base_build(folder, lambda value: (outcome.append(value), done.set()))
        if not done.wait(1800) or outcome[-1]['state'] != 'ready':
            raise RuntimeError(f'base build failed: {outcome[-1] if outcome else None}')
        print(json.dumps({'phase': 'base-build', 'paths': count,
                          'seconds': round(time.monotonic() - phase_start, 3)}), flush=True)

        root_added = folder / 'root-event-probe.txt'
        root_added.write_text('temporary root file\n')
        git(folder, 'add', 'root-event-probe.txt')
        root_added.unlink()
        git(folder, 'add', '-A', '--', 'root-event-probe.txt')
        changed = folder / 'payload/d000/f000000.txt'
        changed.write_text('user edited this file\n')
        git(folder, 'add', 'payload/d000/f000000.txt')
        backend = images._get_backend()
        original_sync = backend.sync_delta
        original_read = macos._read_events
        delta_seconds = []
        event_paths = []

        def tracked_read_events(event_root, since):
            events, newest = original_read(event_root, since)
            event_paths.extend(events)
            return events, newest

        def timed_sync(*args, **kwargs):
            started = time.monotonic()
            try:
                return original_sync(*args, **kwargs)
            finally:
                delta_seconds.append(time.monotonic() - started)

        macos._read_events = tracked_read_events
        backend.sync_delta = timed_sync
        start = time.monotonic()
        try:
            workspace = images.create_workspace(folder, agent_id)
            agent_start_seconds = time.monotonic() - start
        finally:
            backend.sync_delta = original_sync
            macos._read_events = original_read
        print(json.dumps({'phase': 'agent-start', 'paths': count,
                          'seconds': round(agent_start_seconds, 3)}), flush=True)
        agent_root = pathlib.Path(workspace['path'])
        status_start = time.monotonic()
        status = git(agent_root, 'status', '--porcelain')
        first_status_seconds = time.monotonic() - status_start
        print(json.dumps({'phase': 'first-git-status', 'paths': count,
                          'seconds': round(first_status_seconds, 3)}), flush=True)
        if status:
            raise AssertionError(f'workspace status was not clean after the user staged a file: {status[:200]}')
        if (agent_root / 'payload/d000/f000000.txt').read_text() != 'user edited this file\n':
            raise AssertionError("workspace did not include the user's edit")
        root_event_fired = any(pathlib.Path(value).resolve() == folder
                               for value, _flags, _event_id in event_paths)
        print(json.dumps({'paths': count, 'agentStartSeconds': round(agent_start_seconds, 3),
                          'deltaSeconds': round(sum(delta_seconds), 3),
                          'rootEventFired': root_event_fired,
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
