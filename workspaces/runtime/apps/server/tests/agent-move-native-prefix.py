#!/usr/bin/env python3
"""Compare native request prefixes across isolated Codex app-servers."""
import importlib.util
import json
import os
from pathlib import Path
import shutil
import sys
import tempfile
import threading

repo = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(repo / 'scripts'))
sys.path.insert(0, str(repo / 'tests'))
spec = importlib.util.spec_from_file_location('native_move_probe', repo / 'tests/native-primitives-integration.py')
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)

from codex_account_transfer import AccountTransfers
from codex_exact_history import export_codex, extend_codex_history, frozen_codex_parameters, unpack_codex
from codex_runtime import AppServer, THREAD_CONFIG, TOOLS


def completed(events, turn):
    fixture.n.until(lambda: any(event.get('method') == 'turn/completed'
        and event['params']['turn']['id'] == turn for event in events), 'native move turn completed')


with fixture.native_server() as (source, _unused, provider, notifications, _start):
    provider.release.set()
    source_home = Path(os.environ['CODEX_HOME'])
    native_config = {**THREAD_CONFIG, 'model_provider': 'local-probe'}
    thread = source.call('thread/start', {
        'cwd': str(source_home), 'model': 'gpt-5.6-sol',
        'developerInstructions': 'Frozen developer instructions for a move.',
        'config': native_config, 'dynamicTools': TOOLS,
        'sandbox': 'danger-full-access', 'approvalPolicy': 'never',
    })['thread']['id']
    for index in range(2):
        turn = source.call('turn/start', {'threadId': thread,
            'input': [{'type': 'text', 'text': 'old request ' + str(index)}]})['turn']['id']
        completed(notifications, turn)
    old_request = provider.requests[-1]
    native = source.call('thread/read', {'threadId': thread, 'includeTurns': False})['thread']
    source.call('thread/unsubscribe', {'threadId': thread})
    with tempfile.TemporaryDirectory(prefix='studio-move-target-probe-') as directory:
        root = Path(directory)
        home = root / 'home'
        home.mkdir()
        shutil.copyfile(source_home / 'config.toml', home / 'config.toml')
        archive = root / 'history.zip'
        original = Path(native['path'])
        export_codex(source_home, original, archive)
        original_bytes = original.read_bytes()
        staged = unpack_codex(archive, root / 'staging')
        transfer = AccountTransfers.__new__(AccountTransfers)
        transfer.copy_lock = threading.Lock()
        copied = transfer.copy_history(root / 'staging', home / 'sessions' / '.studio-moves' / 'probe', staged)
        assert copied.read_bytes() == original_bytes
        frozen = frozen_codex_parameters(copied)
        events = []
        target = AppServer(root, events.append, lambda _: None, lambda: None, home=home, isolated=True)
        try:
            target.call('thread/resume', {
                'threadId': thread, 'path': str(copied), 'cwd': str(root),
                'model': frozen['model'], 'developerInstructions': frozen['developerInstructions'],
                'baseInstructions': frozen['baseInstructions'], 'config': native_config,
                'sandbox': 'danger-full-access', 'approvalPolicy': 'never',
            })
            turn = target.call('turn/start', {'threadId': thread, 'cwd': str(root),
                'input': [{'type': 'text', 'text': 'new turn target context'}]})['turn']['id']
            completed(events, turn)
            new_request = provider.requests[-1]
            result = {key: old_request.get(key) == new_request.get(key)
                for key in ('instructions', 'tools', 'reasoning', 'model', 'prompt_cache_key')}
            result.update(inputPrefix=old_request['input'] == new_request['input'][:len(old_request['input'])],
                oldItems=len(old_request['input']), newItems=len(new_request['input']))
            assert all(result[key] for key in
                ('instructions', 'tools', 'reasoning', 'model', 'prompt_cache_key', 'inputPrefix')), result
            target_snapshot = target.call('thread/read', {'threadId': thread, 'includeTurns': False})['thread']
            target.call('thread/unsubscribe', {'threadId': thread})
            returned_archive = root / 'return.zip'
            export_codex(home, Path(target_snapshot['path']), returned_archive)
            returned_staging = root / 'return-staging'
            returned = unpack_codex(returned_archive, returned_staging)
            returned = transfer.copy_history(returned_staging, source_home / 'sessions' / '.studio-moves' / 'return-probe', returned)
            extend_codex_history(source, source_home, thread, returned, root / 'previous-native.jsonl')
            source.call('thread/resume', {
                'threadId': thread, 'cwd': str(source_home),
                'model': frozen['model'], 'developerInstructions': frozen['developerInstructions'],
                'baseInstructions': frozen['baseInstructions'], 'config': native_config,
                'sandbox': 'danger-full-access', 'approvalPolicy': 'never',
            })
            turn = source.call('turn/start', {'threadId': thread, 'cwd': str(source_home),
                'input': [{'type': 'text', 'text': 'return turn source context'}]})['turn']['id']
            completed(notifications, turn)
            returned_request = provider.requests[-1]
            result['returnPrefix'] = new_request['input'] == returned_request['input'][:len(new_request['input'])]
            result['returnSettings'] = all(new_request.get(key) == returned_request.get(key)
                for key in ('instructions', 'tools', 'reasoning', 'model', 'prompt_cache_key'))
            assert result['returnPrefix'], result
            assert result['returnSettings'], result
            local = source.call('thread/read', {'threadId': thread, 'includeTurns': False})['thread']
            source.call('thread/unsubscribe', {'threadId': thread})
            local_archive = root / 'local.zip'
            export_codex(source_home, Path(local['path']), local_archive)
            local_copy = unpack_codex(local_archive, root / 'local-staging')
            extend_codex_history(source, source_home, thread, local_copy, root / 'before-local.jsonl')
            source.call('thread/resume', {
                'threadId': thread, 'cwd': str(root), 'model': frozen['model'],
                'developerInstructions': frozen['developerInstructions'], 'baseInstructions': frozen['baseInstructions'],
                'config': native_config, 'sandbox': 'danger-full-access', 'approvalPolicy': 'never',
            })
            turn = source.call('turn/start', {'threadId': thread, 'cwd': str(root),
                'input': [{'type': 'text', 'text': 'local move folder context'}]})['turn']['id']
            completed(notifications, turn)
            local_request = provider.requests[-1]
            result['localPrefix'] = returned_request['input'] == local_request['input'][:len(returned_request['input'])]
            result['localSettings'] = all(returned_request.get(key) == local_request.get(key)
                for key in ('instructions', 'tools', 'reasoning', 'model', 'prompt_cache_key'))
            result['siblingLoaded'] = _unused in source.call('thread/loaded/list', {'limit': 100})['data']
            assert all(result[key] for key in ('localPrefix', 'localSettings', 'siblingLoaded')), result
            print(json.dumps(result))
        finally:
            target.close()
            for stream in (target.proc.stdin, target.proc.stdout, target.proc.stderr):
                if stream is not None:
                    stream.close()
