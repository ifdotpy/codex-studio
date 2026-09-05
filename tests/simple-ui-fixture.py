#!/usr/bin/env python3
"""Isolated 40-worker UI fixture. No model calls or user state."""
import importlib.util
from pathlib import Path
import sys

sys.dont_write_bytecode = True
skill = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(skill / 'scripts'))
from codex_canvas import Canvas, make_server
from codex_runtime import Runtime

spec = importlib.util.spec_from_file_location('fixture', skill / 'tests/runtime-contract.py')
m = importlib.util.module_from_spec(spec)
spec.loader.exec_module(m)
c = Canvas(Path(sys.argv[1]))
c.runtime = Runtime(c.root, m.FakeServer)
with c.runtime.lock, c.runtime.db() as db:
    lead = c.runtime.create({'name': 'Release lead', 'cwd': str(c.root), 'prompt': 'Review the release'}, defer=True)
    lead.update(autoWake=True, status='waiting')
    c.runtime.put(db, 'agents', lead)
# Commit the root before create() opens another database connection.
for i in range(40):
    child = c.runtime.create({'name': f'Worker {i:02}', 'prompt': 'Review one component', 'role': 'reviewer'}, parent=lead['id'], defer=True)
    with c.runtime.lock, c.runtime.db() as db:
        child['status'] = 'failed' if i == 7 else 'running' if i < 8 else 'queued' if i < 25 else 'completed'
        c.runtime.put(db, 'agents', child)
        c.runtime.item(db, child['id'], child['id'] + ':reply', 'assistant', f'Worker {i:02} report <script>', 'Agent')
with c.runtime.lock, c.runtime.db() as db:
    c.runtime.item(db, lead['id'], lead['id'] + ':user', 'user', 'Review the release. Split the work across the team and report the blockers.', 'You')
    c.runtime.item(db, lead['id'], lead['id'] + ':reply', 'assistant', 'I assigned 40 workers to the review. Seven workers are active and 15 have finished.\n\nWorker 07 found a failed check. I will collect the remaining results before I prepare the release report.', 'Lead')
    c.runtime.item(db, lead['id'], lead['id'] + ':tool', 'tool', 'Hidden tool fixture', 'Tool')
other = c.runtime.create({'name': 'Other project', 'cwd': str(c.root), 'prompt': 'Separate task'}, defer=True)
server = make_server(c, port=int(sys.argv[2]) if len(sys.argv) > 2 else 0)
print(server.server_port, flush=True)
try:
    server.serve_forever()
finally:
    c.runtime.close()
