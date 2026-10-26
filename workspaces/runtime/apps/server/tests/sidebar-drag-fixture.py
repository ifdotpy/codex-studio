#!/usr/bin/env python3
"""Isolated sidebar fixture. No native model requests or user state."""

# Support direct execution without runner-provided PYTHONPATH.
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT, SERVER_TESTS_ROOT

import importlib.util
import json
import os
from pathlib import Path
import sys
import time
import uuid

sys.dont_write_bytecode = True
root = REPOSITORY_ROOT
sys.path.insert(0, str(SERVER_TESTS_ROOT))
from test_isolation import isolate_api_schema_cache
isolate_api_schema_cache()
sys.path.insert(0, str(SERVER_SOURCE_ROOT))
from codex_canvas import Canvas, make_server
from codex_runtime import Runtime
from codex_peer_teams import manage
from studio_api.context import api_schema_cache_key, read_cached_or_compute_api_schema_hash
from studio_api.schema import api_schema_hash, openapi_document

# Build the real schema before runtime threads compete for the Python interpreter.
read_cached_or_compute_api_schema_hash(
    api_schema_cache_key(), lambda: api_schema_hash(openapi_document()))
spec = importlib.util.spec_from_file_location('runtime_fixture', SERVER_TESTS_ROOT / 'runtime-contract.py')
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class QuietRuntime(Runtime):
    def schedule(self):
        while not self.closed:
            self.changed.wait(.1)
            self.changed.clear()


canvas = Canvas(Path(sys.argv[1]))
canvas.runtime = rt = QuietRuntime(canvas.root, fixture.FakeServer)
if os.environ.get('SIDEBAR_ALIAS_FIXTURE'):
    access = rt.paired_access()
    with rt.db() as db:
        peer = {"id": "office", "clientId": "office", "serverId": "office", "kind": "server",
                "label": "office-node", "origin": "https://office-node.tailf00fa0.ts.net",
                "publicKey": "fixture-key", "tailscaleUser": "owner", "status": "revoked",
                "created": 1, "lastAccess": None, "revoked": 1}
        db.execute("INSERT INTO runtime_access_clients VALUES(?,?)", ("office", json.dumps(peer)))
paths = {}
for name in ('Project A', 'Project B', 'Project C'):
    path = canvas.root / name
    path.mkdir()
    paths[name] = str(path.resolve())
    rt.projects({'path': paths[name]})
project = paths['Project A']
folders = [str(uuid.uuid4()), str(uuid.uuid4())]
for i, name in enumerate(('Folder A', 'Folder B')):
    rt.projects({'action': 'add_folder', 'path': project, 'folder_id': folders[i], 'name': name, 'expected_revision': i})
leads = {}
for name in ('Team source', 'Team peer', 'Destination', 'Old hidden', 'Old folder hidden', 'Pinned', 'Recent', 'Unread', 'Running'):
    a = rt.create({'name': name, 'cwd': project, 'prompt': name}, defer=True)
    with rt.lock, rt.db() as db:
        a.update(created=time.time() - 172800, updated=time.time() - 172800, status='idle', autoWake=True)
        if name == 'Old folder hidden':
            a['projectFolder'] = folders[0]
        if name == 'Pinned':
            a['pinned'] = True
        if name == 'Recent':
            a['updated'] = time.time()
            if os.environ.get('SIDEBAR_ALIAS_FIXTURE'):
                a['provider'] = 'claude'
        if name == 'Unread':
            a.update(hasUnread=True, unreadCount=1)
        if name == 'Running':
            a.update(status='running', inFlight=True)
        rt.put(db, 'agents', a)
    leads[name] = a
child = rt.create({'name': 'Original worker', 'prompt': 'Saved worker task', 'role': 'reviewer'}, parent=leads['Team source']['id'], defer=True)
with rt.lock, rt.db() as db:
    child.update(status='completed', autoWake=False)
    rt.put(db, 'agents', child)
    rt.item(db, leads['Team source']['id'], 'source-history', 'assistant', 'Saved source history.', 'Agent')
team = str(uuid.uuid4())
manage(rt, {'action': 'save', 'path': project, 'team_id': team, 'members': [leads['Team source']['id'], leads['Team peer']['id']],
            'name': 'Team A', 'expected_revision': 0, 'request_id': str(uuid.uuid4())})
server = make_server(canvas, port=0)
server.context.start_api_schema_hash().result(timeout=120)
print(server.server_address[1], flush=True)
try:
    server.serve_forever()
finally:
    rt.close()
