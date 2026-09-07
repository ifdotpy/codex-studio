"""Managed chat reads avoid global discovery and full task-history scans."""
import importlib.util
import json
from pathlib import Path
import sys
import tempfile
from unittest.mock import patch

sys.dont_write_bytecode = True
root = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(root / 'scripts'))
from codex_canvas import Canvas

spec = importlib.util.spec_from_file_location('runtime_fixture', root / 'tests/runtime-contract.py')
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)

with tempfile.TemporaryDirectory(prefix='studio-transcript-latency-') as directory:
    canvas = Canvas(Path(directory))
    runtime = canvas.runtime = fixture.Runtime(canvas.root, fixture.FakeServer)
    try:
        lead = runtime.create({'name': 'History', 'cwd': directory, 'prompt': 'Fixture'}, defer=True)
        with runtime.lock, runtime.db() as db:
            runtime.item(db, lead['id'], 'answer', 'assistant', 'Latest answer', 'Agent')
        with patch.object(canvas, 'threads', side_effect=AssertionError('Global discovery on chat read')):
            assert canvas.transcript(lead['id'])['items'][-1]['text'] == 'Latest answer'
            with runtime.lock, runtime.db() as db:
                deleted = runtime.agent(lead['id'], db)
                deleted['deletedAt'] = 1
                runtime.put(db, 'agents', deleted)
            try:
                canvas.transcript(lead['id'])
                raise AssertionError('Deleted chat accepted')
            except ValueError as error:
                assert 'deleted' in str(error)
        # Unknown managed identities still use the legacy discovery path.
        with patch.object(canvas, 'thread', return_value={'id': 'legacy'}) as lookup:
            assert not canvas.transcript('legacy')['items']
            lookup.assert_called_once_with('legacy')
        with runtime.lock, runtime.db() as db:
            lead.pop('deletedAt', None)
            runtime.put(db, 'agents', lead)
            db.executemany('INSERT INTO runtime_tasks VALUES (?, ?)', (
                (str(i), json.dumps({'id': str(i), 'agent': lead['id'], 'created': i,
                                    'status': 'running' if i == 0 else 'completed', 'tail': 'x' * 1024}))
                for i in range(5000)
            ))
            def measured():
                steps = [0]
                def progress():
                    steps[0] += 100
                    return 0
                db.set_progress_handler(progress, 100)
                try:
                    return runtime.recent_tasks(db), steps
                finally:
                    db.set_progress_handler(None, 0)
            indexed, fast_steps = measured()
            db.execute('DROP INDEX runtime_task_history')
            scanned, slow_steps = measured()
            assert indexed == scanned
            assert len(indexed) == 101 and indexed[0]['status'] == 'running'
            assert fast_steps[0] * 5 < slow_steps[0], (fast_steps, slow_steps)
            print({'indexedSteps': fast_steps[0], 'scanSteps': slow_steps[0], 'result': 'PASS'})
    finally:
        runtime.close()
