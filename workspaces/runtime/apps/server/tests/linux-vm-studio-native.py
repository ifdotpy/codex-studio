#!/usr/bin/env python3
"""Opt-in Studio caller proof against an already provisioned, isolated VM.

Run with workspaces/runtime/apps/server/src/codex_python.py --exec. This test does not start a backend,
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


def git(root, *arguments):
    return subprocess.check_output(['git', '-C', str(root), *arguments], text=True).strip()


def progress(name):
    print('Studio VM check: ' + name, flush=True)


def run_turn(runtime, agent, prompt):
    completed = threading.Event()
    notification, start_error = runtime.notification, runtime.start_error
    def observe(message, *identity):
        notification(message, *identity)
        if message.get('method') == 'turn/completed' and message.get('params', {}).get('threadId') == agent['threadId']:
            completed.set()
    def rejected(key, *arguments, **options):
        start_error(key, *arguments, **options)
        if key == agent['id']:
            completed.set()
    runtime.notification, runtime.start_error = observe, rejected
    try:
        with runtime.lock, runtime.db() as db:
            db.execute("UPDATE runtime_events SET status='delivered' WHERE agent=?", (agent['id'],))
        runtime.send(agent['id'], prompt, message_id=str(uuid.uuid4()), resume=True)
        runtime.dispatch(agent['id'])
        assert completed.wait(360), 'The native model turn did not complete'
        result = runtime.agent(agent['id'])
        assert result.get('lastCompletedTurnStatus') == 'completed', result.get('error') or 'The native model turn failed'
    finally:
        runtime.notification, runtime.start_error = notification, start_error


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--state-dir', required=True)
    parser.add_argument('--helper', required=True)
    parser.add_argument('--codex-home', default=str(Path.home() / '.codex'))
    parser.add_argument('--claude', action='store_true')
    args = parser.parse_args()
    host_profile = Path(args.codex_home).expanduser().resolve()
    assert (host_profile / 'auth.json').is_file(), 'The selected host profile is signed out'
    os.environ['CODEX_LINUX_VM_STATE_DIR'] = args.state_dir
    os.environ['CODEX_LINUX_VM_HELPER'] = args.helper
    os.environ['CODEX_HOME'] = str(host_profile)
    spec = importlib.util.spec_from_file_location('studio_native_fixture', Path(__file__).with_name('worker-defaults-contract.py'))
    fixture = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(fixture)
    from codex_linux_vm import connect
    from codex_linux_vm_exec import execute
    from codex_linux_workspaces import dispose
    from codex_vm_agents import prepare, flush_turns, project_id
    remote = connect(args.state_dir, helper=args.helper)
    assert remote.status()['state'] == 'running', 'Use a VM that is already provisioned'
    metrics = {'stateDir': args.state_dir}
    with tempfile.TemporaryDirectory(prefix='studio-layr-caller-') as temporary:
        root = Path(temporary)
        project = root / 'project'
        project.mkdir()
        git(project, 'init', '-b', 'main')
        git(project, 'config', 'user.name', 'Studio VM test')
        git(project, 'config', 'user.email', 'studio-test@example.invalid')
        (project / 'tracked').write_text('base\n')
        git(project, 'add', '.')
        git(project, 'commit', '-m', 'Create test source')
        (project / 'tracked').write_text('dirty\n')
        original_head = git(project, 'rev-parse', 'HEAD')
        runtime = fixture.ControlledRuntime(root / 'runtime', fixture.f.FakeServer)
        runtime._linux_vm_client = remote
        runtime.catalog = lambda account='default': fixture.CATALOG
        agents = []

        def command(agent, *argv, success=True):
            output, error = io.BytesIO(), io.BytesIO()
            code = execute(remote, agent['id'], agent['cwd'], list(argv), output=output, error=error)
            if success:
                assert code == 0, error.getvalue().decode(errors='replace')
                return output.getvalue().decode().strip()
            return code

        def prepare_provider(agent):
            agent = prepare(runtime, agent)
            server = runtime.connect_agent(agent)
            catalog = server.call('model/list', {}, timeout=30)
            selected = next((item for item in catalog['data'] if item.get('isDefault')), catalog['data'][0])
            effort, native_effort = runtime.validate_execution(catalog, selected['model'], selected['defaultReasoningEffort'], False)
            with runtime.lock, runtime.db() as db:
                current = runtime.agent(agent['id'], db)
                current.update(model=selected['model'], effort=effort, nativeEffort=native_effort)
                runtime.put(db, 'agents', current)
            prepared = runtime.prepare_locked(runtime.agent(agent['id']))
            if isinstance(prepared, concurrent.futures.Future):
                prepared.result(timeout=120)
            return runtime.agent(agent['id'])

        def spawn(lead, role='implementer', state=None):
            created = runtime.spawn_agents(runtime.agent(lead['id']), {'agents': [
                {'name': 'Layr proof', 'prompt': 'VM caller proof', 'role': role,
                 **({'base_ref': state} if state else {})}]}, str(uuid.uuid4()))
            agent_id = created['agents'][0]['id']
            agents.append(agent_id)
            return prepare_provider(runtime.agent(agent_id))

        try:
            lead = runtime.new_lead({'cwd': str(project), 'workspaceMode': 'layr'})
            agents.append(lead['id'])
            lead = prepare_provider(lead)
            assert command(lead, 'cat', 'tracked') == 'dirty'
            run_turn(runtime, lead, 'Reply layr-lead-ok. Do not use tools.')
            flush_turns(runtime, runtime.agent(lead['id']))
            assert all(row['status'] == 'saved' for row in runtime.agent(lead['id'])['layrTurnSaves'].values())
            metrics['leadGuestModelTurnAndState'] = True
            progress('lead VM model turn and turn state')
            worker = spawn(lead)
            assert command(worker, 'id', '-u') != command(lead, 'id', '-u')
            assert worker['layrLine'] != lead['layrLine']
            assert command(worker, 'cat', 'tracked') == 'dirty'
            (project / 'tracked').write_text('later Mac edit\n')
            second = spawn(lead)
            assert command(second, 'cat', 'tracked') == 'dirty'
            metrics['ownedLinesAndNoAutomaticSync'] = True
            run_turn(runtime, worker, 'Write native-result.txt with layr-worker-ok. Use layr add and layr commit to save it. '
                     'Do not create a Git branch or spawn agents. Report layr rev-parse HEAD.')
            flush_turns(runtime, runtime.agent(worker['id']))
            assert command(worker, 'cat', 'native-result.txt') == 'layr-worker-ok'
            revision = command(worker, 'layr', 'rev-parse', 'HEAD')
            metrics['workerGuestModelCommit'] = revision
            reviewer = spawn(lead, role='reviewer', state=revision)
            run_turn(runtime, reviewer, 'Read native-result.txt. Confirm it contains layr-worker-ok. Do not change files.')
            flush_turns(runtime, runtime.agent(reviewer['id']))
            assert command(reviewer, 'python3', '-c', "open('forbidden','w').write('no')", success=False) != 0
            metrics['reviewerFilesystemReadOnly'] = True
            task = runtime.work_action(lead['id'], {'action': 'create', 'title': 'VM proof', 'owner': worker['id']}, actor=lead['id'])
            runtime.work_action(worker['id'], {'action': 'submit', 'task_id': task['id'], 'revision': revision,
                'result': 'VM file committed', 'checks': 'Read exact file', 'files': ['native-result.txt']}, actor=worker['id'])
            assert command(lead, 'layr', 'show', revision + ':native-result.txt') == 'layr-worker-ok'
            accepted = runtime.work_action(lead['id'], {'action': 'accept', 'task_id': task['id'],
                'result': 'Reviewed exact state'}, actor=lead['id'])
            assert accepted['status'] == 'accepted'
            assert command(lead, 'cat', 'native-result.txt') == 'layr-worker-ok'
            assert not (project / 'native-result.txt').exists()
            assert git(project, 'rev-parse', 'HEAD') == original_head
            metrics['guardedAcceptanceAndSeparateMacFolder'] = True
            imported = remote.ensure_layr_project(project_id(lead), str(project),
                request_id='layr-project:' + project_id(lead))
            mounted = imported['mount']
            assert mounted['state'] == 'mounted' and mounted['readOnly'] is True
            assert (Path(mounted['path']) / 'main/native-result.txt').read_text().strip() == 'layr-worker-ok'
            assert (Path(mounted['path']) / 'states').is_dir()
            metrics['macReadOnlyShareShowsAcceptedFile'] = True
            handle = 'linux-worker:' + lead['id']
            before = remote.request('provider.rpc', {'handle': handle, 'action': 'info'})
            runtime.close()
            runtime = fixture.ControlledRuntime(root / 'runtime', fixture.f.FakeServer)
            runtime._linux_vm_client = remote
            runtime.catalog = lambda account='default': fixture.CATALOG
            restored = runtime.connect_agent(runtime.agent(lead['id']))
            after = remote.request('provider.rpc', {'handle': handle, 'action': 'info'})
            assert (before['pid'], before['generation']) == (after['pid'], after['generation'])
            restored.call('thread/read', {'threadId': lead['threadId']}, timeout=30)
            metrics['studioRestartSameLeadProvider'] = True
            if args.claude:
                key = runtime.accounts.register_claude()
                claude = runtime.new_lead({'cwd': str(project), 'workspaceMode': 'layr', 'account_key': key})
                agents.append(claude['id'])
                claude = prepare_provider(claude)
                run_turn(runtime, claude, 'Reply layr-claude-ok. Do not use tools.')
                flush_turns(runtime, runtime.agent(claude['id']))
                from codex_linux_vm_credentials import profile_path
                profile = claude['layrHome'] + '/' + profile_path(key, 'claude') + '/.credentials.json'
                assert command(claude, 'python3', '-c',
                    'import json,sys;print("refreshToken" not in json.load(open(sys.argv[1]))["claudeAiOauth"])', profile) == 'True'
                metrics['claudeGuestLeadModelAndAccessOnlyCredentials'] = True
        finally:
            for agent_id in reversed(agents):
                if runtime.agent(agent_id).get('layrReady'):
                    dispose(runtime, agent_id, remove=True)
            runtime.close()
    print(json.dumps(metrics, sort_keys=True))


if __name__ == '__main__':
    main()
