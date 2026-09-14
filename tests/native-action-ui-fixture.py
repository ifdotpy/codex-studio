#!/usr/bin/env python3
"""Native action HTTP fixture. All native requests remain in this process."""
import importlib.util
import json
from pathlib import Path
import sys

sys.dont_write_bytecode = True
repo = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(repo / 'scripts'))
from codex_canvas import Canvas, make_server
from codex_runtime import Runtime
spec = importlib.util.spec_from_file_location('fixture', repo / 'tests/runtime-contract.py')
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
root = Path(sys.argv[1])


class ActionServer(fixture.FakeServer):
    def call(self, method, params, timeout=60):
        if method in {'review/start', 'thread/compact/start'}:
            self.calls.append((method, params))
            with (root / 'native-actions.jsonl').open('a') as output:
                output.write(json.dumps({'method': method, 'params': params}) + '\n')
            self.seq += 1
            turn = {'id': f'action-{self.seq}', 'status': 'inProgress'}
            self.notify({'method': 'turn/started', 'params': {'threadId': params['threadId'], 'turn': turn}})
            self.complete(params['threadId'], turn['id'])
            return {'turn': turn}
        return super().call(method, params, timeout)


canvas = Canvas(root)
canvas.runtime = Runtime(root, ActionServer)
a = canvas.runtime.create({'name': 'Action lead', 'cwd': str(root), 'prompt': 'Fixture'})
fixture.eventually(lambda: canvas.runtime.agent(a['id'])['status'] == 'running')
a = canvas.runtime.agent(a['id'])
canvas.runtime.connect().complete(a['threadId'], a['turnId'])
canvas.runtime.connect().notify({'method': 'thread/tokenUsage/updated', 'params': {
    'threadId': a['threadId'], 'turnId': a['turnId'],
    'tokenUsage': {'total': {'totalTokens': 100}, 'last': {'totalTokens': 100}}}})
server = make_server(canvas)
print(server.server_port, flush=True)
try:
    server.serve_forever()
finally:
    canvas.runtime.close()
