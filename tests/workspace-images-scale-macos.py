"""Run the real macOS engine against a 200,000-file synthetic repository."""

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


def git(repo, *args):
    return subprocess.run(['git', '-C', str(repo), *args], check=True,
                          capture_output=True, text=True, timeout=600).stdout.strip()


def write_files(root, count, start=0):
    root.mkdir(parents=True, exist_ok=True)
    for index in range(start, start + count):
        (root / f'f{index:06d}.txt').write_text(f'file {index}\n')


def write_hardlinks(repo, count, archive_path):
    seed_data = b'shared scale payload\n'
    seeds = {}
    archive_started = time.monotonic()
    with tarfile.open(archive_path, 'w', format=tarfile.GNU_FORMAT) as archive:
        for index in range(count):
            folder = f'payload/d{index % 200:03d}'
            seed = seeds.get(folder)
            if seed is None:
                seed = folder + '/.scale-file-seed'
                info = tarfile.TarInfo(seed)
                info.size = len(seed_data)
                archive.addfile(info, io.BytesIO(seed_data))
                seeds[folder] = seed
            info = tarfile.TarInfo(f'{folder}/f{index:06d}.txt')
            info.type = tarfile.LNKTYPE
            info.linkname = seed
            archive.addfile(info)
    archive_seconds = time.monotonic() - archive_started
    extract_started = time.monotonic()
    subprocess.run(['tar', '-xf', str(archive_path), '-C', str(repo)], check=True, timeout=900)
    for folder in seeds:
        (repo / seeds[folder]).unlink()
    archive_path.unlink()
    return {'archive': archive_seconds, 'extract': time.monotonic() - extract_started}


def make_nested_repo(path, count):
    path.mkdir(parents=True)
    git(path, 'init', '-q')
    git(path, 'config', 'user.name', 'Scale Test')
    git(path, 'config', 'user.email', 'scale@example.invalid')
    write_files(path / 'nested-files', count)
    git(path, 'add', '-A')
    git(path, 'commit', '-m', 'nested base')
    return git(path, 'rev-parse', 'HEAD')


def format_steps(values):
    result = {}
    for key, value in values.items():
        if key in {'gitCalls', 'directCommands'}:
            result[key] = {command: {'count': row['count'],
                                     'seconds': round(row['seconds'], 3)}
                           for command, row in value.items()}
        elif isinstance(value, dict):
            result[key] = {name: round(seconds, 3) for name, seconds in value.items()}
        else:
            result[key] = round(value, 3)
    return result


def main():
    if sys.platform != 'darwin' or shutil.which('diskutil') is None:
        raise SystemExit('requires macOS diskutil')
    file_count = int(sys.argv[1]) if len(sys.argv) > 1 else 200000
    if file_count not in {50000, 200000}:
        raise SystemExit('file count must be 50000 or 200000')
    temp = tempfile.TemporaryDirectory(prefix=f'workspace-scale-{file_count}-')
    root = pathlib.Path(temp.name)
    repo, store = root / 'repo', root / 'store'
    previous_store = os.environ.get('CODEX_WORKSPACE_STORE')
    os.environ['CODEX_WORKSPACE_STORE'] = str(store)
    agent_id = f'scale-{file_count}'
    timings = {}
    try:
        setup_started = time.monotonic()
        repo.mkdir()
        nested_heads = {}
        for name in ('nested-a', 'nested-b', 'nested-c'):
            nested_heads[name] = make_nested_repo(repo / name, 1000)
        nested_seconds = time.monotonic() - setup_started
        root_git_started = time.monotonic()
        git(repo, 'init', '-q')
        git(repo, 'config', 'user.name', 'Scale Test')
        git(repo, 'config', 'user.email', 'scale@example.invalid')
        (repo / '.gitignore').write_text('nested-a/\nnested-b/\nnested-c/\n')
        # Tar creates distinct hard-linked paths without a Python open per file.
        fixture_seconds = write_hardlinks(repo, file_count - 3000, root / 'files.tar')
        git(repo, 'add', '-A')
        for name, head in nested_heads.items():
            git(repo, 'update-index', '--add', '--cacheinfo', f'160000,{head},{name}')
        git(repo, 'commit', '-m', 'synthetic 200k base')
        root_git_seconds = time.monotonic() - root_git_started - sum(fixture_seconds.values())
        timings.update({'nestedRepos': nested_seconds, 'fixtureFiles': fixture_seconds,
                        'rootGitAddCommit': root_git_seconds})

        done = threading.Event()
        build_result = []
        backend = images._get_backend()
        base_steps = {'repositoryDiscovery': 0.0, 'open': 0.0, 'copy': 0.0,
                      'seal': 0.0, 'repoSetup': 0.0, 'gitCalls': {},
                      'directCommands': {}}
        saved_base_methods = {}
        for method, key_name in (('open_base_staging', 'open'), ('copy_base_tree', 'copy'),
                                 ('seal_base', 'seal')):
            original = getattr(backend, method)
            saved_base_methods[method] = original

            def timed_base_method(*args, _original=original, _key=key_name, **kwargs):
                tick = time.monotonic()
                try:
                    return _original(*args, **kwargs)
                finally:
                    base_steps[_key] += time.monotonic() - tick

            setattr(backend, method, timed_base_method)
        original_prepare = images._prepare_repo

        def timed_prepare(*args, **kwargs):
            tick = time.monotonic()
            try:
                return original_prepare(*args, **kwargs)
            finally:
                base_steps['repoSetup'] += time.monotonic() - tick

        images._prepare_repo = timed_prepare
        original_discovery = images._git_repositories

        def timed_discovery(*args, **kwargs):
            tick = time.monotonic()
            try:
                return original_discovery(*args, **kwargs)
            finally:
                base_steps['repositoryDiscovery'] += time.monotonic() - tick

        images._git_repositories = timed_discovery
        original_base_git = images._git
        original_base_command = images._command

        def timed_base_git(repo_path, *args, **kwargs):
            tick = time.monotonic()
            try:
                return original_base_git(repo_path, *args, **kwargs)
            finally:
                command = str(args[0]) if args else '(empty)'
                row = base_steps['gitCalls'].setdefault(command, {'count': 0, 'seconds': 0.0})
                row['count'] += 1
                row['seconds'] += time.monotonic() - tick

        images._git = timed_base_git

        def timed_base_command(args, **kwargs):
            caller = sys._getframe(1).f_code.co_name
            started = time.monotonic()
            try:
                return original_base_command(args, **kwargs)
            finally:
                if caller != '_git':
                    name = caller + ':' + str(args[0])
                    row = base_steps['directCommands'].setdefault(
                        name, {'count': 0, 'seconds': 0.0})
                    row['count'] += 1
                    row['seconds'] += time.monotonic() - started

        images._command = timed_base_command
        build_started = time.monotonic()
        images.start_base_build(repo, lambda value: (build_result.append(value), done.set()))
        if not done.wait(900) or build_result[-1]['state'] != 'ready':
            raise RuntimeError(f'base build failed: {build_result[-1] if build_result else None}')
        base_seconds = time.monotonic() - build_started
        images._prepare_repo = original_prepare
        images._git_repositories = original_discovery
        images._git = original_base_git
        images._command = original_base_command
        for method, original in saved_base_methods.items():
            setattr(backend, method, original)
        base_measured = sum(value for value in base_steps.values() if isinstance(value, (int, float)))
        base_steps['other'] = max(0.0, base_seconds - base_measured)
        timings['baseSteps'] = format_steps(base_steps)

        changed = repo / 'payload' / 'updates'
        write_files(changed, 100, start=300000)
        steps = {'clone': 0.0, 'attach': 0.0, 'delta': 0.0,
                 'indexCopy': 0.0, 'refSync': 0.0, 'pathStage': 0.0,
                 'snapshotCommit': 0.0, 'gitCalls': {}, 'directCommands': {},
                 'helperSteps': {}}
        saved_methods = {}
        for method, key in (('clone_workspace', 'clone'), ('mount_workspace', 'attach'),
                            ('sync_delta', 'delta')):
            original = getattr(backend, method)
            saved_methods[method] = original

            def timed_backend(*args, _original=original, _key=key, **kwargs):
                tick = time.monotonic()
                try:
                    return _original(*args, **kwargs)
                finally:
                    steps[_key] += time.monotonic() - tick

            setattr(backend, method, timed_backend)
        original_git = images._git

        def timed_git(repo_path, *args, **kwargs):
            tick = time.monotonic()
            try:
                return original_git(repo_path, *args, **kwargs)
            finally:
                command = str(args[0]) if args else '(empty)'
                row = steps['gitCalls'].setdefault(command, {'count': 0, 'seconds': 0.0})
                row['count'] += 1
                row['seconds'] += time.monotonic() - tick
                if 'commit' in args and 'studio snapshot' in args:
                    steps['snapshotCommit'] += time.monotonic() - tick

        images._git = timed_git
        original_command = images._command

        def timed_command(args, **kwargs):
            caller = sys._getframe(1).f_code.co_name
            started = time.monotonic()
            try:
                return original_command(args, **kwargs)
            finally:
                if caller != '_git':
                    name = caller + ':' + str(args[0])
                    row = steps['directCommands'].setdefault(
                        name, {'count': 0, 'seconds': 0.0})
                    row['count'] += 1
                    row['seconds'] += time.monotonic() - started

        images._command = timed_command

        saved_helpers = {}
        for helper_name, key in (('_repo_state_list', 'repoList'),
                                 ('_prepare_repo', 'prepareRepo'),
                                 ('_stageable_paths', 'stageCandidates')):
            original_helper = getattr(images, helper_name)
            saved_helpers[helper_name] = original_helper

            def timed_helper(*args, _original=original_helper, _key=key, **kwargs):
                started = time.monotonic()
                try:
                    return _original(*args, **kwargs)
                finally:
                    steps['helperSteps'][_key] = steps['helperSteps'].get(_key, 0.0) + (
                        time.monotonic() - started)

            setattr(images, helper_name, timed_helper)
        original_copy_index = images._copy_index

        def timed_copy_index(*args, **kwargs):
            tick = time.monotonic()
            try:
                return original_copy_index(*args, **kwargs)
            finally:
                steps['indexCopy'] += time.monotonic() - tick

        images._copy_index = timed_copy_index
        original_sync_refs = images._sync_refs

        def timed_sync_refs(*args, **kwargs):
            tick = time.monotonic()
            try:
                return original_sync_refs(*args, **kwargs)
            finally:
                steps['refSync'] += time.monotonic() - tick

        images._sync_refs = timed_sync_refs
        original_add = images._git

        def timed_git(repo_path, *args, **kwargs):
            tick = time.monotonic()
            try:
                return original_add(repo_path, *args, **kwargs)
            finally:
                if args and args[0] in {'add', 'diff'}:
                    steps['pathStage'] += time.monotonic() - tick

        images._git = timed_git
        create_started = time.monotonic()
        try:
            original_repositories = images._git_repositories
            images._git_repositories = lambda *_args, **_kwargs: (_ for _ in ()).throw(
                AssertionError('create walked the full repository'))
            workspace = images.create_workspace(repo, agent_id)
        finally:
            images._git_repositories = original_repositories
            images._git = original_git
            images._command = original_command
            images._copy_index = original_copy_index
            images._sync_refs = original_sync_refs
            for helper_name, original_helper in saved_helpers.items():
                setattr(images, helper_name, original_helper)
            for method, original in saved_methods.items():
                setattr(backend, method, original)
        create_seconds = time.monotonic() - create_started
        measured = sum(value for value in steps.values() if isinstance(value, (int, float)))
        steps['repoSetup'] = max(0.0, create_seconds - measured)
        if not workspace['snapshotCommit']:
            raise AssertionError('100 changed files did not create the snapshot commit')
        output_steps = format_steps(steps)
        print(json.dumps({'files': file_count, 'setupSteps': timings,
                          'baseSeconds': round(base_seconds, 3),
                          'createSeconds': round(create_seconds, 3),
                          'createSteps': output_steps},
                         sort_keys=True))
    finally:
        try:
            images.remove_workspace(agent_id, force=True)
        except (OSError, RuntimeError, ValueError):
            pass
        for base_file in store.glob('bases/*/base.json') if store.exists() else ():
            state = images._read_json(base_file, {}) or {}
            for item in state.get('protectedRefs', []):
                source_repo = pathlib.Path(state['repoRoot']) / item['path']
                subprocess.run(['git', '-C', str(source_repo), 'update-ref', '-d', item['ref']],
                               capture_output=True, check=False)
            for version in (base_file.parent / 'versions').glob('*'):
                try:
                    Backend().remove_base_version(version)
                except (OSError, RuntimeError):
                    pass
        if previous_store is None:
            os.environ.pop('CODEX_WORKSPACE_STORE', None)
        else:
            os.environ['CODEX_WORKSPACE_STORE'] = previous_store
        temp.cleanup()


if __name__ == '__main__':
    main()
