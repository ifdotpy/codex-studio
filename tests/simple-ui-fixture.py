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
    c.runtime.item(db, lead['id'], lead['id'] + ':reply', 'assistant', 'I assigned 40 workers to the review. Seven workers are active and 15 have finished.\n\nWorker 07 found a failed check. I will collect the remaining results before I prepare the release report.\n\n**Evidence:** all reports remain available. [unsafe](javascript:alert(1)) <img src=\"https://invalid.example/track\" onerror=\"alert(1)\">', 'Lead')
    c.runtime.item(db, lead['id'], lead['id'] + ':tool', 'tool', 'Hidden tool fixture', 'Tool')
    c.runtime.put(db, 'requests', {'id': 'async-question', 'method': 'agent/asyncQuestion', 'agent': lead['id'], 'epoch': 0, 'status': 'pending', 'params': {'questions': [{'id': '0', 'question': 'Which scope?', 'options': [{'label': 'One file'}, {'label': 'All files'}]}]}})
with c.runtime.lock, c.runtime.db() as db:
    chat_sender = c.runtime.agent(child['id'], db)
    chat_sender['autoWake'] = True
    c.runtime.put(db, 'agents', chat_sender)
for i in range(105):
    c.runtime.chat_message(child['id'], 'parent', f'Earlier finding {i}', f'fixture-earlier-{i}')
c.runtime.chat_message(child['id'], 'parent', 'A private update before the final answer.', 'fixture-private')
c.runtime.chat_message(child['id'], 'broadcast', 'Release checks are ready for review.', 'fixture-broadcast')
# Enough actual rooms to exercise the bounded sidebar at production-like sizes.
roster = c.runtime.team(lead['id'])['agents']
for sender in [r for r in roster if r['name'] in {'Worker 38', 'Worker 39'}]:
    with c.runtime.lock, c.runtime.db() as db:
        a = c.runtime.agent(sender['id'], db)
        a['autoWake'] = True
        c.runtime.put(db, 'agents', a)
    for recipient in roster:
        if recipient['id'] not in {sender['id'], lead['id']}:
            c.runtime.chat_message(sender['id'], recipient['id'], 'Review coordination', 'room-fixture:' + sender['id'] + recipient['id'])
with c.runtime.lock, c.runtime.db() as db:
    a = c.runtime.agent(lead['id'], db)
    a.update(compactions=2, contextUsage={'tokens':80000,'window':200000,'at':__import__('time').time()})
    c.runtime.put(db, 'agents', a)
other = c.runtime.create({'name': 'Other project', 'cwd': str(c.root), 'prompt': 'Separate task'}, defer=True)
c.runtime.create({'name': 'Standalone reviewer', 'cwd': str(c.root), 'prompt': 'Review', 'role': 'reviewer', 'model': 'gpt-5.6-luna'}, defer=True)
c.runtime.connect().gate.set()
server = make_server(c, port=int(sys.argv[2]) if len(sys.argv) > 2 else 0)
print(server.server_port, flush=True)
try:
    server.serve_forever()
finally:
    c.runtime.close()
