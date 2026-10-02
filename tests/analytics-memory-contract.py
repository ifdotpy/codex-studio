#!/usr/bin/env python3
"""Keep the analytics overview bounded when item history grows."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import importlib.util
import json
from pathlib import Path
import resource
import sys
import tempfile

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'scripts'))
spec = importlib.util.spec_from_file_location('runtime_contract', ROOT / 'tests/runtime-contract.py')
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
from codex_runtime import Runtime


COUNT = 150_000
LIMIT_BYTES = 200 * 1024 * 1024

with tempfile.TemporaryDirectory() as folder:
    runtime = Runtime(Path(folder), fixture.FakeServer)
    try:
        agent = runtime.create({'name': 'Analytics memory', 'cwd': folder, 'prompt': 'Wait'})
        fixture.eventually(lambda: runtime.agent(agent['id'])['status'] == 'running')
        agent = runtime.agent(agent['id'])
        with runtime.db() as db:
            for index in range(COUNT):
                record = {'id': 'item:' + str(index), 'agentId': agent['id'],
                          'threadId': agent['threadId'], 'turnId': 'turn:' + str(index // 100),
                          'type': 'commandExecution', 'name': 'exec', 'isTool': True,
                          'payloadBoundary': 'protocol', 'status': 'completed',
                          'at': index + 1, 'durationMs': 1.0,
                          'output': {'bytes': 900}, 'details': 'x' * 900}
                db.execute('INSERT INTO analytics_items VALUES (?,?,?,?,?,?,?,?,?,?)',
                           (record['id'], agent['id'], agent['rootId'], agent['threadId'],
                            record['turnId'], record['at'], record['type'], record['name'],
                            1, json.dumps(record)))
        result = runtime.analytics(agent['id'])
        assert result['summary']['protocolToolCalls'] == COUNT
        assert result['pagination']['total'] == COUNT
        assert len(result['calls']) == 50
        peak = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
        if sys.platform != 'darwin':
            peak *= 1024
        print(f'analytics item rows={COUNT} peak RSS={peak} bytes')
        assert peak < LIMIT_BYTES, f'analytics peak exceeded {LIMIT_BYTES} bytes'
    finally:
        runtime.close()
