"""Apply the idle-send fix without restarting native agents."""
import hashlib
from pathlib import Path
import sys

from codex_source import signature, source_function

SOURCE_SHA = '4dad6145eacaebf24441d6767c22a5c6c1e84e1465cf5cc0f86ee2030dce4bbc'
BASE_SIGNATURE = 'bc8bb86dd49d3a4c7948a07aba3e4e2ebf577ae7ba95f201cd48e62a94a1545c'
NEW_SIGNATURE = '650d8ad98af9a0fd25d7ed891e9087e1b9f535d0c55f092d8ac9b883c86c9e10'


def apply(runtime):
    module = sys.modules.get('codex_runtime')
    scripts = Path(__file__).resolve().parent
    if (module is None or Path(module.__file__).resolve().parent != scripts
            or not isinstance(runtime, module.Runtime)):
        raise RuntimeError('The running backend source identity differs')
    source = scripts / 'codex_runtime.py'
    raw = source.read_bytes()
    if hashlib.sha256(raw).hexdigest() != SOURCE_SHA:
        raise RuntimeError('The idle-send source was not reviewed')
    live = module.Runtime.enqueue
    desired, _ = source_function(raw, ('Runtime', 'enqueue'), vars(module), str(source))
    if signature(desired) != NEW_SIGNATURE:
        raise RuntimeError('The idle-send function differs from the reviewed fix')
    with runtime.lock:
        current = signature(live)
        if current == NEW_SIGNATURE:
            return {'status': 'already_applied'}
        if current != BASE_SIGNATURE:
            raise RuntimeError('The running enqueue function differs')
        live.__code__ = desired.__code__
        live.__defaults__ = desired.__defaults__
        live.__kwdefaults__ = desired.__kwdefaults__
    runtime.changed.set()
    return {'status': 'applied'}
