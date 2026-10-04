"""Restore lock owner diagnostics on the existing metrics wrapper."""
import hashlib
from pathlib import Path
import sys

from codex_source import signature, source_function

SOURCE_SHA256 = '1a2c606d1d906f26bf313d58d19541f53b8eb30d85e8c5b9a0f5336c54789f6a'
AFTER = '633f4fe29697d5f18b6280c5caebb47187f66b119e359256dd7b8e024066b693'


def apply(runtime):
    scripts = Path(__file__).resolve().parent
    module = sys.modules.get('codex_runtime')
    metrics = sys.modules.get('codex_lock_metrics')
    path = scripts / 'codex_lock_metrics.py'
    if (module is None or not isinstance(runtime, module.Runtime) or runtime.closed
            or Path(module.__file__).resolve().parent != scripts
            or metrics is None or Path(metrics.__file__).resolve() != path):
        raise RuntimeError('The running backend source identity differs')
    raw = path.read_bytes()
    if hashlib.sha256(raw).hexdigest() != SOURCE_SHA256:
        raise RuntimeError('The reviewed lock metrics source differs')
    desired, _ = source_function(raw, ['MeasuredRLock', '__repr__'], vars(metrics), str(path))
    if signature(desired) != AFTER:
        raise RuntimeError('The reviewed lock owner function differs')
    with runtime.lock:
        current = metrics.MeasuredRLock.__dict__.get('__repr__')
        if current is not None:
            if signature(current) != AFTER:
                raise RuntimeError('The running lock owner function differs')
            return {'status': 'already_applied'}
        metrics.MeasuredRLock.__repr__ = desired
    runtime.changed.set()
    return {'status': 'applied'}
