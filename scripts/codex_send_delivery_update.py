"""Apply send dispatch fixes without restarting native agents."""
import hashlib
from pathlib import Path
import sys

from codex_source import signature, source_function

SOURCE_SHA = '9a3869adee93640cb432ec7f75c61ddca9f2e353d9e133ba17ceaaa7566a77fb'
SIGNATURES = {
    'enqueue': ({'bc8bb86dd49d3a4c7948a07aba3e4e2ebf577ae7ba95f201cd48e62a94a1545c'},
                '650d8ad98af9a0fd25d7ed891e9087e1b9f535d0c55f092d8ac9b883c86c9e10'),
    'dispatch_candidates': ({'37f04914035daf6611f9d27322853bd0bfbf4c885743bf33eaafa703256cad42'}, '175a60de70408b7d02337c035df70d480d0690ff8e514fd1336aa259064fdc5c'),
}


def apply(runtime):
    module = sys.modules.get('codex_runtime')
    scripts = Path(__file__).resolve().parent
    if (module is None or Path(module.__file__).resolve().parent != scripts
            or not isinstance(runtime, module.Runtime)):
        raise RuntimeError('The running backend source identity differs')
    source = scripts / 'codex_runtime.py'
    raw = source.read_bytes()
    if hashlib.sha256(raw).hexdigest() != SOURCE_SHA:
        raise RuntimeError('The send dispatch source was not reviewed')
    replacements = []
    for name, (baselines, expected) in SIGNATURES.items():
        desired, _ = source_function(raw, ('Runtime', name), vars(module), str(source))
        if signature(desired) != expected:
            raise RuntimeError('The send dispatch function differs from the reviewed fix: ' + name)
        replacements.append((name, baselines, expected, desired))
    with runtime.lock:
        for name, baselines, expected, _ in replacements:
            if signature(getattr(module.Runtime, name)) not in baselines | {expected}:
                raise RuntimeError('The running send dispatch function differs: ' + name)
        if all(signature(getattr(module.Runtime, name)) == expected
               for name, _, expected, _ in replacements):
            return {'status': 'already_applied'}
        for name, _, _, desired in replacements:
            live = getattr(module.Runtime, name)
            live.__code__ = desired.__code__
            live.__defaults__ = desired.__defaults__
            live.__kwdefaults__ = desired.__kwdefaults__
    runtime.changed.set()
    return {'status': 'applied'}
