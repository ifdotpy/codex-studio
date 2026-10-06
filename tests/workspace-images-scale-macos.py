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
from codex_workspace_macos import Backend


def git(root, *args):
    return subprocess.run(['git', '-C', str(root), *args], check=True,
                          capture_output=True, text=True, timeout=1800).stdout.strip()


def timed(owner, name, key, buckets):
    original = getattr(owner, name)

    def wrapper(*args, **kwargs):
        started = time.monotonic()
        try:
            return original(*args, **kwargs)
        finally:
            bucket = buckets.setdefault(key, 0.0)
            buckets[key] = bucket + time.monotonic() - started

    setattr(owner, name, wrapper)
    return lambda: setattr(owner, name, original)


def install_timings(backend, buckets):
    restorers = []
    for owner, name, key in (
        (backend, 'copy_base_tree', 'baseCopy'),
        (backend, 'clone_workspace', 'clone'),
        (backend, 'mount_workspace', 'attach'),
        (images, '_repo_snapshots', 'repositorySnapshot'),
        (images, '_git_repositories', 'repositoryDiscovery'),
        (images, '_git_dirty_paths', 'sourceGitStatus'),
        (images, '_git_head', 'headLookup'),
        (images, '_index_fingerprint', 'indexFingerprint'),
        (images, '_index_entries', 'indexEntryRead'),
        (images, '_git_metadata_fingerprint', 'gitMetadataFingerprint'),
        (images, '_detected_paths', 'gitStatusAndPaths'),
        (images, '_copy_exact_paths', 'pathCopy'),
        (images, '_sync_git_directories', 'gitDirectorySync'),
        (images, '_sync_detected', 'delta'),
        (images, '_repo_index_metadata', 'indexDeltaAndRefresh'),
        (images, '_refresh_changed_paths', 'restat'),
    ):
        restorers.append(timed(owner, name, key, buckets))
    git_original = images._git

    def git_timed(repo, *arguments, **kwargs):
        key = 'headDiff' if arguments[:2] == ('diff', '--no-renames') else None
        started = time.monotonic()
        try:
            return git_original(repo, *arguments, **kwargs)
        finally:
            if key:
                buckets[key] = buckets.get(key, 0.0) + time.monotonic() - started

    images._git = git_timed
    restorers.append(lambda: setattr(images, '_git', git_original))
    return restorers


def make_files(root, count, archive_path):
    payload = b'scale payload\n'
    with tarfile.open(archive_path, 'w', format=tarfile.GNU_FORMAT) as archive:
        for index in range(count):
            folder = f'payload/d{index % 200:03d}'
            info = tarfile.TarInfo(f'{folder}/f{index:06d}.txt')
            info.size = len(payload)
            archive.addfile(info, io.BytesIO(payload))
    root.mkdir(parents=True)
    subprocess.run(['tar', '-xf', str(archive_path), '-C', str(root)], check=True, timeout=1800)
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

        backend = images._get_backend()
        timings = {}
        restorers = install_timings(backend, timings)
        phase_start = time.monotonic()
        done = threading.Event()
        outcome = []
        images.start_base_build(folder, lambda value: (outcome.append(value), done.set()))
        if not done.wait(1800) or outcome[-1]['state'] != 'ready':
            raise RuntimeError(f'base build failed: {outcome[-1] if outcome else None}')
        print(json.dumps({'phase': 'base-build', 'paths': count,
                          'seconds': round(time.monotonic() - phase_start, 3),
                          'steps': {key: round(value, 3) for key, value in timings.items()}}),
              flush=True)

        baseline_id = agent_id + '-baseline'
        timings.clear()
        phase_start = time.monotonic()
        baseline = images.create_workspace(folder, baseline_id)
        baseline_create_seconds = time.monotonic() - phase_start
        baseline_root = pathlib.Path(baseline['path'])
        status_start = time.monotonic()
        baseline_status = git(baseline_root, 'status', '--porcelain')
        baseline_status_seconds = time.monotonic() - status_start
        print(json.dumps({'phase': 'first-git-status-no-staged-change', 'paths': count,
                          'seconds': round(baseline_status_seconds, 3),
                          'createSeconds': round(baseline_create_seconds, 3),
                          'agentStartSteps': {key: round(value, 3)
                                              for key, value in timings.items()},
                          'status': baseline_status.splitlines()}, sort_keys=True), flush=True)
        images.remove_workspace(baseline_id)

        root_added = folder / 'root-event-probe.txt'
        root_added.write_text('temporary root file\n')
        git(folder, 'add', 'root-event-probe.txt')
        root_added.unlink()
        git(folder, 'add', '-A', '--', 'root-event-probe.txt')
        changed = folder / 'payload/d000/f000000.txt'
        changed.write_text('user edited this file\n')
        git(folder, 'add', 'payload/d000/f000000.txt')
        timings.clear()
        start = time.monotonic()
        workspace = images.create_workspace(folder, agent_id)
        agent_start_seconds = time.monotonic() - start
        print(json.dumps({'phase': 'agent-start', 'paths': count,
                          'seconds': round(agent_start_seconds, 3),
                          'steps': {key: round(value, 3) for key, value in timings.items()}}),
              flush=True)
        agent_root = pathlib.Path(workspace['path'])
        status_start = time.monotonic()
        status = git(agent_root, 'status', '--porcelain')
        first_status_seconds = time.monotonic() - status_start
        print(json.dumps({'phase': 'first-git-status-after-user-git-add', 'paths': count,
                          'seconds': round(first_status_seconds, 3)}), flush=True)
        print(json.dumps({'paths': count, 'agentStartSeconds': round(agent_start_seconds, 3),
                          'firstGitStatusSeconds': round(first_status_seconds, 3),
                          'agentStartSteps': {key: round(value, 3)
                                              for key, value in timings.items()}}, sort_keys=True),
              flush=True)
        expected_status = ['M  payload/d000/f000000.txt']
        if status.splitlines() != expected_status:
            raise AssertionError(f'unexpected workspace status: {status[:200]}')
        if (agent_root / 'payload/d000/f000000.txt').read_text() != 'user edited this file\n':
            raise AssertionError("workspace did not include the user's edit")
    finally:
        for restore in locals().get('restorers', []):
            restore()
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
