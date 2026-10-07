#!/usr/bin/env python3
"""Opt-in Studio caller proof against an already provisioned, isolated VM.

Run with scripts/codex_python.py --exec. This test does not start a backend,
stop the VM, or change the host account credentials. It deletes its guest workers.
"""
import argparse
import concurrent.futures
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import uuid
from unittest.mock import patch


def git(root, *arguments):
    return subprocess.check_output(['git', '-C', str(root), *arguments], text=True).strip()


def progress(name):
    print('Studio VM check: ' + name, flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--state-dir', required=True)
    parser.add_argument('--helper', required=True)
    parser.add_argument('--codex-home', default=str(Path.home() / '.codex'))
    parser.add_argument('--claude', action='store_true')
    args = parser.parse_args()
    auth = (Path(args.codex_home) / 'auth.json').read_bytes()
    os.environ['CODEX_LINUX_VM_STATE_DIR'] = args.state_dir
    os.environ['CODEX_LINUX_VM_HELPER'] = args.helper
    spec = importlib.util.spec_from_file_location('studio_native_fixture', Path(__file__).with_name('worker-defaults-contract.py'))
    fixture = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(fixture)
    from codex_linux_vm import connect
    from codex_linux_vm_exec import execute
    from codex_linux_vm_fetch import fetch
    from codex_linux_workspaces import build_base, dispose, resources
    from codex_agent_management import manage_agent
    remote = connect(args.state_dir, helper=args.helper)
    assert remote.status()['state'] == 'running', 'Use a VM that is already provisioned'
    metrics = {'stateDir': args.state_dir}
    with tempfile.TemporaryDirectory(prefix='studio-linux-caller-') as name:
        root = Path(name)
        profile = root / 'profile'
        profile.mkdir(mode=0o700)
        (profile / 'auth.json').write_bytes(auth)
        (profile / 'auth.json').chmod(0o600)
        os.environ['CODEX_HOME'] = str(profile)
        project = root / 'project'
        project.mkdir()
        git(project, 'init', '-b', 'main')
        git(project, 'config', 'user.name', 'Studio VM test')
        git(project, 'config', 'user.email', 'studio-test@example.invalid')
        (project / 'tracked').write_text('base\n')
        (project / 'deleted').write_text('delete me\n')
        git(project, 'add', 'tracked', 'deleted')
        git(project, 'commit', '-m', 'Create test source')
        (project / 'tracked').write_text('staged\n')
        git(project, 'add', 'tracked')
        (project / 'tracked').write_text('dirty\n')
        (project / 'untracked').write_text('host only\n')
        original_head = git(project, 'rev-parse', 'HEAD')
        runtime = fixture.ControlledRuntime(root / 'runtime', fixture.f.FakeServer)
        runtime._linux_vm_client = remote
        runtime.catalog = lambda account='default': fixture.CATALOG
        workers = []

        def command(worker, *argv):
            output, error = io.BytesIO(), io.BytesIO()
            code = execute(remote, worker['id'], worker['cwd'], list(argv), output=output, error=error)
            assert code == 0, error.getvalue().decode(errors='replace')
            return output.getvalue().decode().strip()

        def spawn(lead):
            ready = threading.Event()
            completed = runtime.image_base_completed
            def observed(agent_id, status):
                try:
                    completed(agent_id, status)
                finally:
                    ready.set()
            runtime.image_base_completed = observed
            try:
                result = runtime.spawn_agents(runtime.agent(lead['id']), {'agents':[
                    {'name':'Linux native test', 'prompt':'Native caller proof', 'environment':'linux'}]}, str(uuid.uuid4()))
                worker_id = result['agents'][0]['id']
                workers.append(worker_id)
                assert ready.wait(1900), 'Studio base callback did not complete'
                worker = runtime.agent(worker_id)
                assert worker['environment'] == 'linux', worker.get('imageWorkspaceError')
                assert worker['imageWorkspaceReady'], worker
                return worker
            finally:
                runtime.image_base_completed = completed

        try:
            lead = runtime.new_lead({'cwd':str(project)})
            worker = spawn(lead)
            assert command(worker, 'git', 'status', '--porcelain') == git(project, 'status', '--porcelain')
            assert command(worker, 'cat', 'tracked') == 'dirty'
            metrics['sourceIndexAndDirtyFiles'] = True
            progress('source index and dirty files')
            (project / 'deleted').unlink()
            (project / 'tracked').write_text('later\n')
            build_base(runtime, project)
            later = spawn(lead)
            assert command(later, 'cat', 'tracked') == 'later'
            assert command(later, 'python3', '-c', 'from pathlib import Path;print(Path("deleted").exists())') == 'False'
            assert command(worker, 'cat', 'tracked') == 'dirty'
            metrics['sourceDeltaAndSnapshotIsolation'] = True
            progress('source delta and snapshot isolation')
            server = runtime.connect_agent(worker)
            catalog = server.call('model/list', {}, timeout=30)
            selected = next((item for item in catalog['data'] if item.get('isDefault')), catalog['data'][0])
            effort, native_effort = runtime.validate_execution(catalog, selected['model'], selected['defaultReasoningEffort'], False)
            with runtime.lock, runtime.db() as db:
                worker = runtime.agent(worker['id'], db)
                worker.update(model=selected['model'], effort=effort, nativeEffort=native_effort, yoloMode=True)
                runtime.put(db, 'agents', worker)
            prepared = runtime.prepare_locked(worker)
            if isinstance(prepared, concurrent.futures.Future):
                prepared.result(timeout=120)
            worker = runtime.agent(worker['id'])
            assert worker['threadId']
            server = runtime.connect_agent(worker)
            server.call('thread/read', {'threadId':worker['threadId']}, timeout=30)
            completed = threading.Event()
            notification = runtime.notification
            def observe(message, *identity):
                notification(message, *identity)
                if message.get('method') == 'turn/completed' and message.get('params', {}).get('threadId') == worker['threadId']:
                    completed.set()
            runtime.notification = observe
            with runtime.lock, runtime.db() as db:
                db.execute("UPDATE runtime_events SET status='delivered' WHERE agent=?", (worker['id'],))
            runtime.send(worker['id'], 'Create branch result. Write native-result.txt with linux-native-ok. '
                         'Commit all source changes on result. Use the native command tool. '
                         'Do not spawn agents. Report the commit hash.', message_id=str(uuid.uuid4()), resume=True)
            runtime.dispatch(worker['id'])
            assert completed.wait(360), 'The native Codex turn did not complete'
            assert command(worker, 'cat', 'native-result.txt') == 'linux-native-ok'
            assert command(worker, 'git', 'branch', '--show-current') == 'result'
            assert command(worker, 'git', 'status', '--porcelain') == ''
            metrics['codexNativeModelCommit'] = True
            progress('native Codex model commit')
            runtime.notification = notification
            handle = 'linux-worker:' + worker['id']
            before = remote.request('provider.rpc', {'handle':handle, 'action':'info'})
            runtime.close()
            runtime = fixture.ControlledRuntime(root / 'runtime', fixture.f.FakeServer)
            runtime._linux_vm_client = remote
            runtime.catalog = lambda account='default': fixture.CATALOG
            restored_server = runtime.connect_agent(runtime.agent(worker['id']))
            after = remote.request('provider.rpc', {'handle':handle, 'action':'info'})
            assert before['pid'] == after['pid'] and before['generation'] == after['generation']
            restored_server.call('thread/read', {'threadId':worker['threadId']}, timeout=30)
            metrics['studioRestartSameNativeProcess'] = True
            progress('Studio restart with the same native process')
            metrics['codexThreadId'] = worker['threadId']
            commit = command(worker, 'git', 'rev-parse', 'HEAD')
            fetch(remote, worker['id'], worker['cwd'], 'result', project)
            assert git(project, 'rev-parse', 'FETCH_HEAD') == commit
            assert git(project, 'rev-parse', 'HEAD') == original_head
            assert (project / 'tracked').read_text() == 'later\n'
            metrics['guestCommitAndHostFetch'] = commit
            progress('guest commit and host fetch')
            report = resources(runtime)
            assert report['allocatedDiskBytes'] > 0 and report['memory']['totalBytes'] > 0
            assert report['disk']['freeBytes'] > 0
            metrics['diskAndMemoryReport'] = True
            with runtime.lock, runtime.db() as db:
                current = runtime.agent(worker['id'], db)
                current.update(status='completed', inFlight=False, autoWake=False)
                runtime.put(db, 'agents', current)
                db.execute("UPDATE runtime_events SET status='delivered' WHERE agent=?", (worker['id'],))
            archived = manage_agent(runtime, lead['id'], {'action':'archive', 'agent_id':worker['id'], 'reason':'Native proof complete'})
            assert archived['status'] == 'archived', archived
            restored = manage_agent(runtime, lead['id'], {'action':'restore', 'agent_id':worker['id']})
            assert restored['status'] == 'restored'
            assert runtime.agent(worker['id'])['status'] == 'paused'
            assert command(worker, 'git', 'rev-parse', 'HEAD') == commit
            metrics['archiveAndRestore'] = True
            progress('archive and restore')
            if args.claude:
                from codex_claude import auth_metadata
                account = {'provider':'claude', 'claudeOptions':{}}
                account.update(auth_metadata(account, force=True))
                assert account['status'] == 'ready', 'The host Claude profile must be signed in'
                claude = spawn(lead)
                original_get = runtime.accounts.get
                with patch.object(runtime.accounts, 'get', side_effect=lambda key: account if key == 'native-claude' else original_get(key)):
                    with runtime.lock, runtime.db() as db:
                        current = runtime.agent(claude['id'], db)
                        current.update(accountKey='native-claude', provider='claude')
                        runtime.put(db, 'agents', current)
                    bridge = runtime.connect_agent(runtime.agent(claude['id']))
                    catalog = bridge.call('model/list', {}, timeout=30)
                    runtime.catalog = lambda key='default': catalog if key == 'native-claude' else fixture.CATALOG
                    selected = next((item for item in catalog['data'] if item.get('isDefault')), catalog['data'][0])
                    effort, native_effort = runtime.validate_execution(catalog, selected['model'], selected['defaultReasoningEffort'], False)
                    with runtime.lock, runtime.db() as db:
                        current = runtime.agent(claude['id'], db)
                        current.update(model=selected['model'], effort=effort, nativeEffort=native_effort, yoloMode=True)
                        runtime.put(db, 'agents', current)
                    prepared = runtime.prepare_locked(current)
                    if isinstance(prepared, concurrent.futures.Future):
                        prepared.result(timeout=120)
                    claude = runtime.agent(claude['id'])
                    completed = threading.Event()
                    notification = runtime.notification
                    def observe_claude(message, *identity):
                        notification(message, *identity)
                        if message.get('method') == 'turn/completed' and message.get('params', {}).get('threadId') == claude['threadId']:
                            completed.set()
                    runtime.notification = observe_claude
                    with runtime.lock, runtime.db() as db:
                        db.execute("UPDATE runtime_events SET status='delivered' WHERE agent=?", (claude['id'],))
                    runtime.send(claude['id'], 'Reply linux-claude-ok. Do not use tools.', message_id=str(uuid.uuid4()), resume=True)
                    runtime.dispatch(claude['id'])
                    assert completed.wait(360), 'The native Claude turn did not complete'
                    assert not runtime.agent(claude['id']).get('nativeFailureHold'), 'The Claude turn failed'
                    metrics['claudeNativeModelTurn'] = True
                    progress('native Claude model turn')
                    runtime.notification = notification
        finally:
            for worker_id in workers:
                dispose(runtime, worker_id, remove=True)
            runtime.close()
    print(json.dumps(metrics, sort_keys=True))


if __name__ == '__main__':
    main()
