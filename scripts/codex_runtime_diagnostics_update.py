"""Capture bounded thread stacks without stopping active Studio work."""
import json
import os
from pathlib import Path
import sys
import threading
import time


def apply(runtime):
    frames = sys._current_frames()
    threads = []
    for thread in threading.enumerate():
        frame = frames.get(thread.ident)
        stack = []
        while frame is not None and len(stack) < 40:
            stack.append({'file': frame.f_code.co_filename, 'line': frame.f_lineno,
                          'function': frame.f_code.co_name})
            frame = frame.f_back
        threads.append({'name': thread.name, 'ident': thread.ident,
                        'nativeId': thread.native_id, 'stack': stack})
    report = {'at': time.time(), 'pid': os.getpid(), 'threads': threads,
              'runtimeLock': repr(runtime.lock), 'schedulerError': runtime.scheduler_error}
    directory = Path(runtime.root) / 'evidence' / 'runtime-diagnostics'
    directory.mkdir(parents=True, exist_ok=True)
    path = directory / (str(time.time_ns()) + '.json')
    with path.open('x') as output:
        json.dump(report, output, indent=2)
    return {'status': 'applied', 'report': str(path)}
