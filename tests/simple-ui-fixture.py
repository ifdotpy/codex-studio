#!/usr/bin/env python3
"""Isolated 40-worker UI fixture. No model calls or user state."""
import importlib.util
from pathlib import Path
import sys
import os
import threading

sys.dont_write_bytecode = True
skill = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(skill / 'scripts'))
from codex_canvas import Canvas, make_server
from codex_runtime import Runtime

spec = importlib.util.spec_from_file_location('fixture', skill / 'tests/runtime-contract.py')
m = importlib.util.module_from_spec(spec)
spec.loader.exec_module(m)
c = Canvas(Path(sys.argv[1]))
class BackgroundServer(m.FakeServer):
    def __init__(self, *args):
        super().__init__(*args)
        self.commands = {}

    def call(self, method, params, timeout=60):
        if method == 'command/exec' and 'watch-fixture' in ' '.join(params['command']):
            gate = self.commands.setdefault(params['processId'], threading.Event())
            self.notify({'method': 'command/exec/outputDelta', 'params': {'processId': params['processId'],
                'deltaBase64': m.base64.b64encode(b'Watching the build. Waiting for changes.\n').decode(), 'stream': 'stdout'}})
            gate.wait(120)
            return {'exitCode': 0}
        if method == 'command/exec/terminate' and params['processId'] in self.commands:
            self.commands[params['processId']].set()
            return {}
        return super().call(method, params, timeout)

    def close(self):
        for gate in self.commands.values():
            gate.set()
        super().close()

c.runtime = Runtime(c.root, BackgroundServer if os.environ.get('BACKGROUND_UI_FIXTURE') else m.FakeServer)
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
    c.runtime.item(db, lead['id'], lead['id'] + ':reply', 'assistant', 'I assigned 40 workers to the review. Seven workers are active and 15 have finished.\n\nWorker 07 found a failed check. I will collect the remaining results before I prepare the release report.\n\n| Area | Result | Next step |\n| :--- | :--- | :--- |\n| Message delivery | Passed | Review retry evidence |\n| Context and limits | Passed | Check account reset time |\n| Mobile dialogs | Needs a fix | Worker 07 owns the change |\n\n**Evidence:** all reports remain available. [unsafe](javascript:alert(1)) <img src=\"https://invalid.example/track\" onerror=\"alert(1)\">', 'Lead')
    c.runtime.item(db, lead['id'], lead['id'] + ':tool', 'tool', 'Hidden tool fixture', 'Tool')
    c.runtime.put(db, 'requests', {'id': 'async-question', 'method': 'agent/asyncQuestion', 'agent': lead['id'], 'epoch': 0, 'status': 'pending', 'params': {'questions': [{'id': '0', 'question': 'Which scope?', 'options': [{'label': 'One file'}, {'label': 'All files'}]}]}})
with c.runtime.lock, c.runtime.db() as db:
    chat_sender = c.runtime.agent(child['id'], db)
    chat_sender['autoWake'] = True
    c.runtime.put(db, 'agents', chat_sender)
updates = {
    100: "The message retry check passes. A lost HTTP response produces one stored message, even after the user changes chats.",
    101: "Good. Check the approval dialog on mobile next. Include keyboard focus and a long command in the evidence.",
    102: "I found a focus issue in the old dialog. The new Modal returns focus to the Answer button after it closes.\n\n```tsx\n<Modal opened={!!answer} onClose={close} title=\"Reply to the agent\">\n  <AnswerForm request={answer} />\n</Modal>\n```",
    103: "Fixed and checked at 390px. The dialog scrolls, the buttons stay reachable, and Escape closes it.",
    104: "Thanks. Add the screenshot and test command to your report. I will include it in the release evidence.",
}
for i in range(105):
    from_lead = i in {101, 104}
    c.runtime.chat_message(lead['id'] if from_lead else child['id'], child['id'] if from_lead else 'parent', updates.get(i, f'Earlier finding {i}'), f'fixture-earlier-{i}')
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
if os.environ.get('BACKGROUND_UI_FIXTURE'):
    c.runtime.monitor(lead['id'], {'command': 'watch-fixture --deploy production'}, approved=False)
server = make_server(c, port=int(sys.argv[2]) if len(sys.argv) > 2 else 0)
# Test-only notification input drives the real runtime and HTTP stream.
import json
import threading
def fixture_events():
    for line in sys.stdin:
        message = json.loads(line)
        if message.get('method') == 'fixture/agent-monitor':
            params = message['params']
            agent = c.runtime.agent(params['agent'])
            c.runtime.monitor(agent['id'], params, approved=params.get('approved', True),
                              key=params['id'], epoch=agent['epoch'])
        else:
            c.runtime.notification(message)
threading.Thread(target=fixture_events, daemon=True).start()
print(server.server_port, flush=True)
try:
    server.serve_forever()
finally:
    c.runtime.close()
