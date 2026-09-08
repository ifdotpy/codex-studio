"""Install inspected usage methods without replacing account connections or scans."""
import importlib.util
from pathlib import Path
import time
import types
import sys

from codex_efficiency_update import fingerprint

BASE_LIMITS = '9fdd8af0e2c03305636df32dd56bf329c20d5cb92e1e01780a5c3fca3ca8f54e'
BASE_COSTS = {
    'snapshot': 'd2027201e17863df16d5b713bbc668a6f510b01be50f6bf12fdadb67c18a5dd5',
    '_refresh': 'ab606076c11c020de42c581214119966a079b051ca629e662c55eaa3d083dba1',
}


def replacements(runtime, reader):
    path = Path(__file__).with_name('codex_runtime.py')
    module = compile(path.read_text(), str(path), 'exec', dont_inherit=True)
    cls = next(c for c in module.co_consts if isinstance(c, types.CodeType) and c.co_name == 'Runtime')
    methods = {}
    for name in ('limits', 'limit_refresh_lock'):
        code = next(c for c in cls.co_consts if isinstance(c, types.CodeType) and c.co_name == name)
        function = types.FunctionType(code, runtime.limits.__func__.__globals__.copy(), name,
                                      ('default', False) if name == 'limits' else None)
        methods[name] = types.MethodType(function, runtime)
    path = Path(__file__).with_name('codex_costs.py')
    spec = importlib.util.spec_from_file_location('studio_usage_costs', path)
    costs = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(costs)
    return methods, {name: types.MethodType(getattr(costs.CostReader, name), reader) for name in BASE_COSTS}


def apply(runtime, reader):
    if sys.version_info[:2] != (3, 14):
        raise RuntimeError('Usage update requires the inspected Python 3.14 runtime')
    methods, costs = replacements(runtime, reader)
    old_lock = getattr(runtime, 'limits_lock', None)
    locks = [lock for lock in (old_lock, runtime.lock, reader.lock) if lock is not None]
    acquired = []
    try:
        for lock in locks:
            if not lock.acquire(timeout=10):
                raise RuntimeError('Usage reader busy; no update applied')
            acquired.append(lock)
        if runtime.closed or reader.closed or Path(reader.path).parent != Path(runtime.root):
            raise RuntimeError('Usage state changed; no update applied')
        current = {'limits': fingerprint(runtime.limits), **{n: fingerprint(getattr(reader, n)) for n in costs}}
        target = {'limits': fingerprint(methods['limits']), **{n: fingerprint(f) for n, f in costs.items()}}
        helper = getattr(runtime, 'limit_refresh_lock', None)
        if helper is not None and fingerprint(helper) != fingerprint(methods['limit_refresh_lock']):
            raise RuntimeError('Unknown limit lock helper; no update applied')
        lock_map = getattr(runtime, 'limit_refresh_locks', None)
        if lock_map is not None and not isinstance(lock_map, dict):
            raise RuntimeError('Unknown account lock state; no update applied')
        if current == target and reader.interval == 120 and hasattr(reader, 'clock'):
            if helper is None or lock_map is None:
                raise RuntimeError('Incomplete usage update; no update applied')
            return {'status': 'already_applied', 'fingerprints': target}
        if current != {'limits': BASE_LIMITS, **BASE_COSTS} or reader.interval != 900:
            raise RuntimeError('Unknown usage methods; no update applied')
        changes = [(runtime, name, method) for name, method in methods.items()]
        if not hasattr(runtime, 'limit_refresh_locks'):
            changes.append((runtime, 'limit_refresh_locks', {}))
        changes += [(reader, name, method) for name, method in costs.items()]
        changes += [(reader, 'interval', 120), (reader, 'clock', time.time)]
        before = [(obj, name, name in obj.__dict__, obj.__dict__.get(name)) for obj, name, _ in changes]
        try:
            for obj, name, value in changes:
                setattr(obj, name, value)
        except BaseException:
            for obj, name, present, value in reversed(before):
                if present:
                    obj.__dict__[name] = value
                else:
                    obj.__dict__.pop(name, None)
            raise
        return {'status': 'applied', 'fingerprints': target, 'costIntervalSeconds': reader.interval}
    finally:
        for lock in reversed(acquired):
            lock.release()
