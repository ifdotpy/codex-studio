#!/usr/bin/env python3
"""Opt-in live check. Uses the configured Codex account and model."""
import argparse
import json
from pathlib import Path
import sys
import tempfile
import time
sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
from codex_runtime import Runtime
p = argparse.ArgumentParser(description=__doc__)
p.add_argument('--run', action='store_true', required=True)
a = p.parse_args()
root = Path(tempfile.mkdtemp(prefix='codex-runtime-live-'))
r = Runtime(root)
print('Evidence:', root, flush=True)
try:
    lead = r.new_lead({})
    r.conversation_settings(lead['id'], {'cwd': str(root)})
    r.configure(lead['id'], {'concurrency': 3, 'maxAgents': 4, 'tokenBudget': 300000})
    r.send(lead['id'], """Test this orchestration runtime. Do not inspect files or use native shell tools.
First, set the conversation title with orchestration_title.
Then use functions.exec to print ALL_TOOLS.map(t => t.name) once, without executing any nested tool.
Call orchestration_spawn once with exactly two reviewer agents named Probe A and Probe B.
Their prompts must ask for a single reply CHILD_A_OK and CHILD_B_OK respectively, without tools.
Call orchestration_monitor once with command "sleep 2; printf MONITOR_OK" and timeout_ms 10000.
Then finish this first turn with WAITING_FOR_EVENTS. Do not poll or launch more work.
Completion events will resume you automatically. After both child results and the command's
exit event have arrived, reply LIVE_ORCHESTRATION_OK. If some are still pending, finish
with WAITING_FOR_EVENTS and wait for automatic continuation.""")
    end = time.monotonic() + 180
    while time.monotonic() < end:
        state = r.snapshot()
        current = r.agent(lead['id'])
        if state['requests']:
            # Honor an inherited policy that requires command approval in this
            # explicitly authorized, fixed smoke scenario only.
            for q in state['requests']:
                if q['method'] != 'monitor/approve' or q['params']['command'] != 'sleep 2; printf MONITOR_OK':
                    raise AssertionError('Unexpected request: ' + q['method'])
                r.answer(q['id'], {'decision': 'accept'})
        if current['status'] in ('failed', 'paused', 'interrupted'):
            raise AssertionError(json.dumps(current.get('error')))
        if (len(state['agents']) == 3 and len(state['monitors']) == 1
            and state['monitors'][0]['status'] == 'completed'
            and current['status'] == 'completed'
            and 'LIVE_ORCHESTRATION_OK' in current.get('lastAnswer', '')):
            events = [e for e in state['events'] if e['agent'] == lead['id'] and e['kind'] in ('child_result', 'monitor_exit')]
            assert len(events) == 3 and all(e['status'] == 'delivered' for e in events)
            assert state['monitors'][0]['exitCode'] == 0
            assert state['monitors'][0]['tail'] == 'MONITOR_OK'
            assert current['name'] != 'New chat' and not current['needsTitle'], 'No model-generated title'
            evidence = {'result': 'PASS', 'title': current['name'], 'agents': [{k: x.get(k) for k in ('id', 'name', 'model', 'status', 'tokensUsed')} for x in state['agents']], 'events': events, 'monitor': state['monitors'][0]}
            (root / 'result.json').write_text(json.dumps(evidence, indent=2))
            print(json.dumps(evidence, indent=2), flush=True)
            break
        time.sleep(.5)
    else:
        raise AssertionError('Timed out before all completion events reached the lead')
finally:
    for node in r.snapshot()['agents']:
        if not node.get('parentId'): r.stop(node['id'])
    r.close()
