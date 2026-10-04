"""Update exact-turn reads while preserving imported callback objects."""
import hashlib
from pathlib import Path
import sys

from codex_source import signature, source_function

SOURCE_HASH = 'f363cf1406e4b0686c365b1e2e43ebdf29f857f848724812d7c9b69e4ad493cc'
RUNTIME_HASHES = frozenset({
    '7e2f7a03acdf9c6a1c8876f022b70c14821ef37a7119f33ed910845e18ba7450',
    '9b7fcf91543d2e469bc8581ec8d4c1cd868afe64a4b035a15fe0f5fd1beac7e7',
})
CONNECTION_HASH = 'aa56bc309155c0c6878cf00a0eaa9a3419ab63816e772cc4a72ba8b8748f33a4'
BEFORE = '8832228f92541cefb0ddc68f741c64f3b0155d345b9d8c8f66780d0a78920d86'
AFTER = 'eeb6dae4e3db92946416dd5846b4862c8b8917ca4eb609c1e2c91f7bcc43b899'
TURN_CALLERS = {
    'reconcile_turn': '3920daad9ebf2151ec730dedadb8478092de85cb1e2417e1f031f8dd05256569',
    'apply_turn_recovery': 'cd44b8af432c9fc760d3ac94659c9a5ad0988f8083846923f28f4c92c0d30946',
}
CONNECTION_CALLERS = {
    'recover': '61f78fb547f3b1b60b7422f54e9817cf93d2cf2aa39f7d918e1417f8e16bf74f',
    'recover_queued_active_wait': 'f65599dcb9cf91918b909fe97f83484f8bca98f1731495a7683d27be02628ca6',
}
CONNECTION_CURRENT = 'f8ea88adeb09f9e5b2fe314c212843d8f3f109838b6741c0c5006533e2015174'


def apply(runtime):
    scripts = Path(__file__).resolve().parent
    modules = {name: sys.modules.get(name) for name in
               ('codex_runtime', 'codex_turn_recovery', 'codex_connection_recovery')}
    module, turns, connection = (modules[name] for name in modules)
    if (module is None or not isinstance(runtime, module.Runtime) or runtime.closed
            or any(loaded is None or Path(loaded.__file__).resolve() != scripts / (name + '.py')
                   for name, loaded in modules.items())):
        raise RuntimeError('The running backend source identity differs')
    sources = {}
    accepted = {'codex_runtime': RUNTIME_HASHES, 'codex_turn_recovery': {SOURCE_HASH},
                'codex_connection_recovery': {CONNECTION_HASH}}
    for name, hashes in accepted.items():
        raw = (scripts / (name + '.py')).read_bytes()
        if hashlib.sha256(raw).hexdigest() not in hashes:
            raise RuntimeError('The reviewed turn history source differs: ' + name)
        sources[name] = raw
    desired, static = source_function(sources['codex_turn_recovery'], ['read_native_turn'],
                                      vars(turns), str(scripts / 'codex_turn_recovery.py'))
    if static or signature(desired) != AFTER:
        raise RuntimeError('The reviewed turn history function differs')
    with runtime.lock:
        if runtime.closed:
            raise RuntimeError('The running backend is closed')
        for name, loaded in modules.items():
            if (sys.modules.get(name) is not loaded
                    or Path(loaded.__file__).resolve() != scripts / (name + '.py')
                    or (scripts / (name + '.py')).read_bytes() != sources[name]):
                raise RuntimeError('The running turn history source changed before the update')
        owner = turns.TurnRecoveryMixin
        if (module.TurnRecoveryMixin is not owner or owner not in module.Runtime.__mro__
                or owner.__module__ != 'codex_turn_recovery'):
            raise RuntimeError('The running turn history owner differs')
        for name, expected in TURN_CALLERS.items():
            method = getattr(owner, name, None)
            bound = getattr(runtime, name, None)
            if (name in vars(runtime) or signature(method) != expected
                    or method.__globals__ is not vars(turns)
                    or getattr(module.Runtime, name, None) is not method
                    or getattr(bound, '__func__', None) is not method
                    or getattr(bound, '__self__', None) is not runtime):
                raise RuntimeError('The running turn history caller differs: ' + name)
        method = module.Runtime.connection_current
        if ('connection_current' in vars(runtime) or signature(method) != CONNECTION_CURRENT
                or method.__globals__ is not vars(module)
                or getattr(runtime.connection_current, '__func__', None) is not method):
            raise RuntimeError('The running turn history guard differs: connection_current')
        for name, expected in CONNECTION_CALLERS.items():
            method = getattr(connection, name, None)
            if signature(method) != expected or method.__globals__ is not vars(connection):
                raise RuntimeError('The running connection recovery caller differs: ' + name)
        current = turns.read_native_turn
        if current.__globals__ is not vars(turns):
            raise RuntimeError('The running turn history callback globals differ')
        actual = signature(current)
        if actual not in {BEFORE, AFTER}:
            raise RuntimeError('The running turn history callback differs')
        if actual == AFTER:
            return {'status': 'already_applied'}
        # Both signatures have the same defaults and no closure. One assignment
        # preserves every cached function reference and leaves active reads alone.
        current.__code__ = desired.__code__
    return {'status': 'applied'}
