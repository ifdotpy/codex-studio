"""Install the turn-state watchdog without replacing connections or active frames."""
from pathlib import Path
import types

from codex_efficiency_update import fingerprint
from codex_turn_recovery import TurnRecoveryMixin

BASE_DISPATCH = '9650dc7d75d928e17c494265a9feed79e82af9efd3cca8b41eece7cfc1afa815'
BASE_PREPARE = '395b8ec1d54a285b8cb739acf833d9a7d190649afd77a9c6da70a1d81e00dfbd'


def apply(runtime):
    source = Path(__file__).with_name('codex_runtime.py')
    module = compile(source.read_text(), str(source), 'exec', dont_inherit=True)
    cls = next(c for c in module.co_consts if isinstance(c, types.CodeType) and c.co_name == 'Runtime')
    methods = {name: types.MethodType(value, runtime) for name, value in TurnRecoveryMixin.__dict__.items()
               if isinstance(value, types.FunctionType)}
    for name in ('prepare_locked', 'dispatch'):
        code = next(c for c in cls.co_consts if isinstance(c, types.CodeType) and c.co_name == name)
        function = types.FunctionType(code, getattr(runtime, name).__func__.__globals__.copy(), name)
        methods[name] = types.MethodType(function, runtime)
    if not runtime.lock.acquire(timeout=10):
        raise RuntimeError('Runtime busy; no update applied')
    try:
        if runtime.closed:
            raise RuntimeError('Runtime closed; no update applied')
        if all(fingerprint(getattr(runtime, name, None)) == fingerprint(value) for name, value in methods.items()):
            return {'status': 'already_applied'}
        if (fingerprint(runtime.dispatch) != BASE_DISPATCH
                or fingerprint(runtime.prepare_locked) not in {BASE_PREPARE, fingerprint(methods['prepare_locked'])}):
            raise RuntimeError('Unknown scheduler or preparation version; no update applied')
        for name, value in methods.items():
            if name not in {'dispatch', 'prepare_locked'} and hasattr(runtime, name) and fingerprint(getattr(runtime, name)) != fingerprint(value):
                raise RuntimeError('Unknown recovery method; no update applied')
        before = {name: runtime.__dict__.get(name) for name in methods}
        absent = {name for name in methods if name not in runtime.__dict__}
        try:
            for name, value in methods.items():
                setattr(runtime, name, value)
        except BaseException:
            for name in methods:
                if name in absent:
                    runtime.__dict__.pop(name, None)
                else:
                    setattr(runtime, name, before[name])
            raise
        return {'status': 'applied', 'methods': sorted(methods)}
    finally:
        runtime.lock.release()
