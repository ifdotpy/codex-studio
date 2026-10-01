#!/usr/bin/env python3
"""Keep the analytics overview bounded when usage history grows."""
import importlib.util
import json
from pathlib import Path
import resource
import sys
import tempfile
import time

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'scripts'))
spec = importlib.util.spec_from_file_location('runtime_contract', ROOT / 'tests/runtime-contract.py')
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
from codex_runtime import Runtime


COUNT = 150_000
LIMIT_BYTES = 250 * 1024 * 1024

with tempfile.TemporaryDirectory() as folder:
    runtime = Runtime(Path(folder), fixture.FakeServer)
    try:
        agent = runtime.create({'name': 'Usage memory', 'cwd': folder, 'prompt': 'Wait'})
        fixture.eventually(lambda: runtime.agent(agent['id'])['status'] == 'running')
        agent = runtime.agent(agent['id'])
        with runtime.db() as db:
            for index in range(COUNT):
                turn = 'turn:' + str(index // 100)
                record = {'agentId': agent['id'], 'threadId': agent['threadId'],
                          'turnId': turn, 'accountKey': agent.get('accountKey', 'default'),
                          'model': 'test-model', 'responseId': 'response:' + str(index),
                          'baselineMissing': False, 'modelContextWindow': 10000,
                          'last': {'totalTokens': index % 10000},
                          'delta': {'inputTokens': 100, 'cachedInputTokens': 50,
                                    'cacheWriteInputTokens': 0, 'outputTokens': 10,
                                    'reasoningOutputTokens': 1, 'totalTokens': 110},
                          'at': index + 1}
                db.execute('INSERT INTO analytics_usage(id,agent,root,thread,turn,at,record) VALUES (?,?,?,?,?,?,?)',
                           ('usage:' + str(index), agent['id'], agent['rootId'],
                            agent['threadId'], turn, index + 1, json.dumps(record)))
        started = time.perf_counter()
        result = runtime.analytics(agent['id'])
        elapsed = time.perf_counter() - started
        assert result['summary']['usageSamples'] == COUNT
        assert result['timelineTotal'] == COUNT
        assert len(result['timeline']) == 500
        peak = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
        if sys.platform != 'darwin':
            peak *= 1024
        print(f'analytics usage rows={COUNT} peak RSS={peak} bytes query={elapsed:.3f} s')
        assert peak < LIMIT_BYTES, f'analytics peak exceeded {LIMIT_BYTES} bytes'
    finally:
        runtime.close()
