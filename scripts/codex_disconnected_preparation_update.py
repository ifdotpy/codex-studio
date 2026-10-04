"""Restore exact unsubmitted input after a native connection failure."""
import hashlib
from pathlib import Path
import sys
from codex_source import signature, source_function

SOURCE_SHA256 = '437a0b789edf832aa126cd317830892a920f1c5a6884b3a52abf80ce99311448'
FUNCTIONS = (('disconnected_preparation_eligible', None, '605f4523fbb7263f43ff771f615124d391263e85ba74d7056c551350f75a64eb'), ('restore_disconnected_preparation', None, 'c7d2b4ca00e2e015cf4c174b709e3dba0e2a9dbe06660b8f9c2591465ba82d99'), ('eligible', 'dbfc3ac6ddec1cf281e7ca55efd2f3250a8a0e59bfc7f8fff5b32ecf3df1b22d', '9874fb1beb0754deae4cc66c057934f157841f2b1d43d718966fec491e548ef5'), ('recover', '2059d56877b01c087d0f49c848c4f741cffb3ef15cd621ba3e4e9d22dcdc3f60', '61f78fb547f3b1b60b7422f54e9817cf93d2cf2aa39f7d918e1417f8e16bf74f'))


def apply(runtime):
    scripts = Path(__file__).resolve().parent
    module = sys.modules.get('codex_runtime')
    context = sys.modules.get('codex_connection_recovery')
    path = scripts / 'codex_connection_recovery.py'
    if (module is None or not isinstance(runtime, module.Runtime) or runtime.closed
            or Path(module.__file__).resolve().parent != scripts
            or context is None or Path(context.__file__).resolve() != path):
        raise RuntimeError('The running backend source identity differs')
    raw = path.read_bytes()
    if hashlib.sha256(raw).hexdigest() != SOURCE_SHA256:
        raise RuntimeError('The reviewed connection recovery source differs')
    plans = []
    for name, before, after in FUNCTIONS:
        desired, _ = source_function(raw, [name], vars(context), str(path))
        if signature(desired) != after:
            raise RuntimeError('The reviewed recovery function differs: ' + name)
        plans.append((name, before, after, desired))
    with runtime.lock:
        already = True
        for name, before, after, desired in plans:
            current = vars(context).get(name)
            actual = None if current is None else signature(current)
            if actual not in {before, after}:
                raise RuntimeError('The running recovery function differs: ' + name)
            already = already and actual == after
        if already:
            return {'status': 'already_applied'}
        for name, before, after, desired in plans:
            current = vars(context).get(name)
            if current is None:
                vars(context)[name] = desired
            else:
                current.__code__ = desired.__code__
                current.__defaults__ = desired.__defaults__
                current.__kwdefaults__ = desired.__kwdefaults__
    runtime.changed.set()
    return {'status': 'applied'}
