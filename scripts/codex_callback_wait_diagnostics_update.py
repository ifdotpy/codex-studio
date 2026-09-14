"""Observe callback waits for one bounded window without changing callbacks."""
import json
from itertools import islice
import os
from pathlib import Path
import sys
import threading
import time

WINDOW_SECONDS = 60
MAX_CAPTURES = 10
CAPTURE_INTERVAL_SECONDS = 2


def queued_waits(runtime):
    waits = []
    now = time.time()
    for account, server in list(runtime.servers.items()):
        callbacks = getattr(server, 'callbacks', None)
        if callbacks is None or not callbacks.mutex.acquire(timeout=.01):
            continue
        try:
            received = [message.get('_studioReceivedAt') for _, message in islice(callbacks.queue, 256)
                        if isinstance(message, dict) and type(message.get('_studioReceivedAt')) in (int, float)]
            oldest = min(received) if received else now
            if now - oldest > 1:
                waits.append({'account': account, 'queued': len(callbacks.queue),
                              'oldestWaitMs': round((now - oldest) * 1000, 3)})
        finally:
            callbacks.mutex.release()
    return waits


def capture(runtime, waits):
    frames = sys._current_frames()
    threads = []
    for thread in threading.enumerate():
        frame, stack = frames.get(thread.ident), []
        while frame is not None and len(stack) < 40:
            stack.append({'file': frame.f_code.co_filename, 'line': frame.f_lineno,
                          'function': frame.f_code.co_name})
            frame = frame.f_back
        threads.append({'name': thread.name, 'ident': thread.ident,
                        'nativeId': thread.native_id, 'stack': stack})
    return {'at': time.time(), 'pid': os.getpid(), 'waits': waits, 'threads': threads,
            'runtimeLock': repr(getattr(runtime, 'lock', None))}


def apply(runtime):
    existing = getattr(runtime, '_callback_wait_diagnostic', None)
    if existing is not None:
        return {'status': 'already_applied', 'observing': existing.is_alive()}
    directory = Path(runtime.root) / 'evidence' / 'runtime-diagnostics'
    directory.mkdir(parents=True, exist_ok=True)
    path = directory / ('callback-waits-' + str(time.time_ns()) + '.jsonl')
    started = time.monotonic()

    def observe():
        count, last = 0, None
        terminal = {'status': 'completed'}
        with path.open('x') as output:
            try:
                while not runtime.closed and time.monotonic() - started < WINDOW_SECONDS and count < MAX_CAPTURES:
                    now = time.monotonic()
                    waits = queued_waits(runtime)
                    if waits and (last is None or now - last >= CAPTURE_INTERVAL_SECONDS):
                        output.write(json.dumps(capture(runtime, waits)) + '\n')
                        output.flush()
                        count, last = count + 1, now
                    time.sleep(min(.1, max(0, WINDOW_SECONDS - (time.monotonic() - started))))
            except Exception as error:
                terminal = {'status': 'failed', 'errorType': type(error).__name__}
            finally:
                output.write(json.dumps({**terminal, 'captures': count,
                                         'elapsedSeconds': time.monotonic() - started}) + '\n')

    worker = threading.Thread(target=observe, daemon=True, name='bounded-callback-diagnostic')
    runtime._callback_wait_diagnostic = worker
    worker.start()
    return {'status': 'applied', 'report': str(path), 'windowSeconds': WINDOW_SECONDS,
            'maxCaptures': MAX_CAPTURES}
