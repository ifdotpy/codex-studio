"""Exercise the real HTTP adapter in an isolated runtime fixture."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time
import urllib.error
import urllib.request

root = Path(__file__).resolve().parents[1]
with tempfile.TemporaryDirectory() as directory:
    process = subprocess.Popen(
        ['python3', '-B', str(root / 'tests/simple-ui-fixture.py'), directory],
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        env={**os.environ, 'CODEX_BOARD_STATE_DIR': directory + '/board'},
    )
    try:
        port = process.stdout.readline().strip()
        origin = 'http://127.0.0.1:' + port
        def get(path):
            return json.load(urllib.request.urlopen(origin + path, timeout=10))
        identity, state = get('/api/sync/identity'), get('/api/state')
        generations = get('/api/sync/generations')
        assert identity['syncProtocol'] == generations['protocol'] == 2
        assert set(generations['generations']) == {'state', 'transcripts', 'drafts'}
        with urllib.request.urlopen(origin + '/api/sync/stream?protocol=2', timeout=10) as stream:
            lines = []
            while True:
                line = stream.readline().decode().rstrip('\r\n')
                if line.startswith('data: '):
                    lines.append(line[6:])
                if not line:
                    break
        event = json.loads(lines[0])
        assert event['protocol'] == 2
        assert event['workspaceId'] == identity['workspaceId']
        assert set(event['generations']) == {'state', 'transcripts', 'drafts'}
        assert all(isinstance(value, int) for value in event['generations'].values())
        before_legacy_thread = generations['generations']['state']
        status_file = Path(directory) / 'codex-swarm-status.legacy-contract.json'
        status_file.write_text(json.dumps([{
            'name': 'Legacy fixture', 'threadId': 'a' * 36, 'runId': 'b' * 36,
            'wave': 'legacy-contract', 'launcherPid': os.getpid(),
            'turnStatus': 'running', 'cwd': directory,
        }]))
        after_legacy_thread = get('/api/sync/generations')
        deadline = time.monotonic() + 5
        while (after_legacy_thread['generations']['state'] <= before_legacy_thread
               and time.monotonic() < deadline):
            time.sleep(.05)
            after_legacy_thread = get('/api/sync/generations')
        assert after_legacy_thread['generations']['state'] > before_legacy_thread
        with urllib.request.urlopen(origin + '/api/sync/stream', timeout=10) as stream:
            while True:
                line = stream.readline().decode().rstrip('\r\n')
                if line.startswith('data: '):
                    assert line == 'data: "RESYNC"'
                    break
        projection = get('/api/sync/pull?scope=state')
        assert projection['generation'] >= after_legacy_thread['generations']['state']
        payload = json.loads(projection['documents'][0]['payload'])
        assert 'runtime' in payload and 'threads' in payload and 'token' not in payload
        assert any(row['name'] == 'Legacy fixture' for row in payload['threads'])
        assert projection['workspaceId'] == identity['workspaceId']
        row = {'newDocumentState': {'id': 'phone:lead', 'seq': 0, '_deleted': False,
               'payload': json.dumps({'text': 'draft', 'session': 'lead', 'device': 'phone', 'updated': 1})}}
        def push(rows, workspace):
            request = urllib.request.Request(
                origin + '/api/sync/drafts', data=json.dumps({'rows': rows}).encode(),
                headers={'Content-Type': 'application/json', 'Origin': origin,
                         'X-Canvas-Token': state['token'], 'X-Canvas-Workspace': workspace},
            )
            return json.load(urllib.request.urlopen(request, timeout=10))
        assert push([row], identity['workspaceId']) == []
        assert len(get('/api/sync/pull?scope=drafts')['documents']) == 1
        for rows, workspace, status in [([row], 'wrong', 409), ([{}], identity['workspaceId'], 400)]:
            try:
                push(rows, workspace)
                raise AssertionError('Invalid push accepted')
            except urllib.error.HTTPError as error:
                assert error.code == status, error.read()
        lead = next(item for item in state['threads'] if item.get('isLead'))['id']
        def voice(action, **body):
            request = urllib.request.Request(origin + '/api/voice/' + action,
                data=json.dumps({'agent': lead, **body}).encode(),
                headers={'Content-Type': 'application/json', 'X-Canvas-Token': state['token'],
                         'X-Canvas-Workspace': identity['workspaceId']})
            with urllib.request.urlopen(request, timeout=10) as response:
                return json.load(response)
        assert isinstance(voice('status')['configured'], bool)
        voice('record', session_id='', event_id='http-voice-one', kind='user', text='Raw recognized words')
        sent = voice('submit', message_id='http-voice-send', record_ids=['http-voice-one'], edited_text='Edited user request')
        assert voice('submit', message_id='http-voice-send', record_ids=['http-voice-one'], edited_text='Edited user request')['id'] == sent['id']
        for action, body in [('speak', {'text': 'spoof'}),
                             ('record', {'session_id': '', 'event_id': 'spoof', 'kind': 'orchestrator', 'text': 'spoof', '_internal': True}),
                             ('submit', {'message_id': 'http-voice-send', 'record_ids': ['http-voice-one'], 'edited_text': 'Changed retry'}),
                             ('start', {})]:
            try:
                voice(action, **body)
                raise AssertionError('Invalid voice request accepted')
            except urllib.error.HTTPError as error:
                with error:
                    assert error.code == 400, error.read()
        print('sync and voice HTTP contracts passed')
    finally:
        process.terminate()
        process.wait(timeout=10)
