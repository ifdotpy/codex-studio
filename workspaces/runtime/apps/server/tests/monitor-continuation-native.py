#!/usr/bin/env python3
"""Real local monitor output and exit wake a fixture lead, without model calls."""
import importlib.util
import json
from pathlib import Path
import shlex
import sys
import tempfile
import unittest

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location('native_fixture', Path(__file__).with_name('monitor-completion-native.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)


class NativeContinuation(unittest.TestCase):
    def test_output_and_cancel_continue_owner_once(self):
        with tempfile.TemporaryDirectory(prefix='studio-monitor-continuation-') as directory:
            runtime = f.Runtime(Path(directory), f.NativeCommands)
            try:
                lead = runtime.create({'name': 'Local fixture', 'prompt': 'Wait for command',
                                       'cwd': directory, 'model': 'gpt-5.6-sol'})
                f.f.eventually(lambda: runtime.agent(lead['id'])['status'] == 'running')
                server = runtime.server
                def record(key):
                    with runtime.db() as db:
                        return json.loads(db.execute('SELECT record FROM runtime_monitors WHERE id=?', (key,)).fetchone()[0])
                def event(key):
                    with runtime.db() as db:
                        return db.execute('SELECT * FROM runtime_events WHERE id=?', ('monitor:' + key,)).fetchone()
                def finish_turn():
                    current = runtime.agent(lead['id'])
                    server.complete(current['threadId'], current['turnId'])
                def starts():
                    return [p for method, p in server.calls if method == 'turn/start']
                finish_turn()
                code = 'import sys; [print("OUTPUT-%04d" % i, flush=True) for i in range(1600)]; print("FINAL_SENTINEL", flush=True)'
                before = len(starts())
                result = runtime.monitor(lead['id'], {'command': shlex.quote(sys.executable) + ' -c ' + shlex.quote(code),
                                                     'timeout_ms': 10000}, approved=True)
                key = result['id']
                f.f.eventually(lambda: event(key) is not None and event(key)['status'] == 'delivered', timeout=15)
                self.assertEqual(len(starts()), before + 1)
                self.assertIn(key, starts()[-1]['input'][0]['text'])
                terminal = record(key)
                self.assertEqual(terminal['exitCode'], 0)
                output = Path(terminal['log']).read_bytes()
                self.assertEqual(output, (''.join('OUTPUT-%04d\n' % i for i in range(1600)) + 'FINAL_SENTINEL\n').encode())
                self.assertEqual(terminal['bytes'], len(output))
                self.assertIn('FINAL_SENTINEL', json.loads(event(key)['text'])['tail'])
                finish_turn()
                before = len(starts())
                result = runtime.monitor(lead['id'], {'command': 'printf CANCEL_READY; exec sleep 30',
                                                     'timeout_ms': 40000}, approved=True)
                key = result['id']
                f.f.eventually(lambda: 'CANCEL_READY' in record(key)['tail'], timeout=15)
                runtime.cancel_monitor(key)
                f.f.eventually(lambda: event(key) is not None and event(key)['status'] == 'delivered', timeout=15)
                self.assertEqual(len(starts()), before + 1)
                terminal = record(key)
                self.assertEqual(terminal['status'], 'cancelled')
                self.assertNotEqual(terminal['exitCode'], 0)
                self.assertIn(key, starts()[-1]['input'][0]['text'])
                print(json.dumps({'normalBytes': len(output), 'normalExit': 0,
                                  'cancelExit': terminal['exitCode'], 'ownerContinuations': 2}))
            finally:
                runtime.close()


if __name__ == '__main__':
    unittest.main(verbosity=2)
