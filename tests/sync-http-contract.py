"""Exercise the real HTTP adapter in an isolated runtime fixture."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
from studio_api.testing import read_test_state

root = Path(__file__).resolve().parents[1]
with tempfile.TemporaryDirectory() as directory:
    process = subprocess.Popen(
        ['python3', '-B', str(root / 'tests/simple-ui-fixture.py'), directory],
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        env={**os.environ, 'CODEX_BOARD_STATE_DIR': directory + '/board'},
    )
    try:
        port = process.stdout.readline().strip()
        assert port.isdigit(), process.stderr.read()
        origin = 'http://127.0.0.1:' + port
        def get(path):
            # Native startup can defer read-only snapshots; the browser retries
            # this same documented response. Never retry mutation requests here.
            deadline = time.monotonic() + 10
            while True:
                try:
                    with urllib.request.urlopen(origin + path, timeout=10) as response:
                        return json.load(response)
                except urllib.error.HTTPError as error:
                    error.close()
                    if error.code != 503 or time.monotonic() >= deadline:
                        raise
                    time.sleep(.1)
        identity = get('/api/sync/identity')
        state = read_test_state(get)
        protocol = get('/api/sync/protocol')
        assert protocol['protocolVersion'] == 3
        assert protocol['supportedVersions'] == [3]
        for removed in ('/api/sync/generations', '/api/transcript/stream'):
            try:
                get(removed)
                raise AssertionError(f'removed route still exists: {removed}')
            except urllib.error.HTTPError as error:
                assert error.code in (404, 405)
        status_file = Path(directory) / 'codex-swarm-status.legacy-contract.json'
        status_file.write_text(json.dumps([{
            'name': 'Legacy fixture', 'threadId': 'a' * 36, 'runId': 'b' * 36,
            'wave': 'legacy-contract', 'launcherPid': os.getpid(),
            'turnStatus': 'running', 'cwd': directory,
        }]))
        deadline = time.monotonic() + 5
        projection = get('/api/sync/pull?scope=state')
        while time.monotonic() < deadline:
            payload = json.loads(projection['documents'][0]['payload'])
            if any(row['name'] == 'Legacy fixture' for row in payload['threads']):
                break
            time.sleep(.05)
            projection = get('/api/sync/pull?scope=state')
        assert any(row['name'] == 'Legacy fixture' for row in payload['threads'])
        payload = json.loads(projection['documents'][0]['payload'])
        assert 'runtime' in payload and 'threads' in payload and 'token' not in payload
        assert any(row['name'] == 'Legacy fixture' for row in payload['threads'])
        assert projection['workspaceId'] == identity['workspaceId']
        entities = get('/api/sync/pull?scope=state%3Aentities%3Av1&after=0&limit=100')
        assert entities['workspaceId'] == identity['workspaceId']
        assert entities['documents'] and all(row['id'].startswith('entity:') for row in entities['documents'])
        rows = list(entities['documents'])
        checkpoint = entities['checkpoint']['seq']
        while not any(row['id'].startswith('entity:agent:') for row in rows) and checkpoint < entities['maxSeq']:
            entities = get(f'/api/sync/pull?scope=state%3Aentities%3Av1&after={checkpoint}&limit=100')
            rows.extend(entities['documents'])
            checkpoint = entities['checkpoint']['seq']
        agent = next(json.loads(row['payload'])['value'] for row in rows
                     if row['id'].startswith('entity:agent:'))
        assert {'id', 'name', 'status'} <= agent.keys()
        assert 'nativeToolCatalog' not in agent and 'accountHistory' not in agent
        row = {'newDocumentState': {'id': 'phone:lead', 'seq': 0, '_deleted': False,
               'payload': json.dumps({'text': 'draft', 'session': 'lead', 'device': 'phone', 'updated': 1})}}
        def push(rows, workspace):
            request = urllib.request.Request(
                origin + '/api/sync/drafts', data=json.dumps({'rows': rows}).encode(),
                headers={'Content-Type': 'application/json', 'Origin': origin,
                         'X-Canvas-Token': state.token, 'X-Canvas-Workspace': workspace},
            )
            return json.load(urllib.request.urlopen(request, timeout=10))
        assert push([row], identity['workspaceId']) == []
        drafts = get('/api/sync/pull?scope=drafts')
        assert len(drafts['documents']) == 1
        for rows, workspace, status in [([row], 'wrong', 409), ([{}], identity['workspaceId'], 400)]:
            try:
                push(rows, workspace)
                raise AssertionError('Invalid push accepted')
            except urllib.error.HTTPError as error:
                assert error.code == status, error.read()
        lead_id = next(item['id'] for item in state.values('agent') if item.get('isLead'))
        lead = lead_id
        def voice(action, **body):
            request = urllib.request.Request(origin + '/api/voice/' + action,
                data=json.dumps({'agent': lead, **body}).encode(),
                headers={'Content-Type': 'application/json', 'X-Canvas-Token': state.token,
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
