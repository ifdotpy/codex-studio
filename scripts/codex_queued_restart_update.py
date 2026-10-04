"""Restore the exact native turn behind an unsent restart input wait."""
import hashlib
from pathlib import Path
import sys
from codex_source import signature, source_function

SOURCE_SHA256 = 'aa56bc309155c0c6878cf00a0eaa9a3419ab63816e772cc4a72ba8b8748f33a4'
FUNCTIONS = (('queued_active_wait_eligible', '377c1bdc1f2287909532e45156fc020d8666638ad94016fb7732fe567adb718f', 'a976627c039f61cc19cadd08a4571ff7890a744ec3d25c41e58255d4321f2573'), ('recover_queued_active_wait', 'f1d7ab5c66fbb0e7eb97a38f2107019b84c66fdef9e29343e30dfe47fd38fdf0', 'f65599dcb9cf91918b909fe97f83484f8bca98f1731495a7683d27be02628ca6'))


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
