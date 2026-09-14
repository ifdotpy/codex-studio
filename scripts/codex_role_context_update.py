"""Remove proven duplicate role instructions and preserve prepared context versions."""
from pathlib import Path
import sys
from types import FunctionType

from codex_active_task_update import compile_function, signature

EXPECTED = {'codex_context_repair.verified_events': ('6d50b41b07c5f5d96c36547b31aa15f59a8ac1b41dd3c34f03e588a7381b90f1',
                                          '0e63dd2aaafb251219e402aec9e8f830a7fd3871e58a54f189a30c04571561b8'),
 'codex_context_repair.sanitized_rollout': ('8610798ee2991bb659f0e32e8fcdd8d73334ac97efb7fa7c6e9a808c3391fd79',
                                            '4296acce68e8899db5fe777768e1f94806fcac648255319121912cac2bfef76f'),
 'codex_efficiency.model_known_context': ('86c1e9a511a47a01f790eaa71e9955691d58097e4febd5ffb18e9cce2995a7fb',
                                          '91e0f9c4d1cd8dffe1e5e7d4cca12623f68c6b0d963d4b331a8de75228360bb5')}


def apply(runtime):
    import codex_runtime
    import codex_context_repair
    import codex_efficiency
    if sys.version_info[:2] != (3, 14) or type(runtime) is not codex_runtime.Runtime:
        raise RuntimeError('Unknown runtime for role context update')
    directory = Path(__file__).resolve().parent
    targets = (
        (codex_context_repair, None, 'verified_events'),
        (codex_context_repair, None, 'sanitized_rollout'),
        (codex_efficiency, 'EfficiencyMixin', 'model_known_context'),
    )
    staged = []
    for module, class_name, name in targets:
        path = directory / (module.__name__ + '.py')
        if Path(module.__file__).resolve() != path:
            raise RuntimeError('Unexpected role context module location')
        owner = getattr(module, class_name) if class_name else module
        desired = compile_function(path.read_text(), class_name, name, vars(module))
        allowed = EXPECTED[module.__name__ + '.' + name]
        if signature(desired) != allowed[1]:
            raise RuntimeError('Unreviewed role context replacement')
        staged.append((module, owner, name, desired, allowed))
    if not runtime.lock.acquire(timeout=10):
        raise RuntimeError('Runtime remains busy; no role context update applied')
    try:
        if runtime.closed:
            raise RuntimeError('Runtime is closed')
        owner = codex_efficiency.EfficiencyMixin
        if owner not in type(runtime).__mro__ or 'model_known_context' in vars(runtime):
            raise RuntimeError('Unknown context version owner')
        changes = []
        for module, owner, name, desired, allowed in staged:
            live = vars(owner).get(name)
            if (not isinstance(live, FunctionType) or live.__globals__ is not vars(module)
                    or live.__module__ != module.__name__ or signature(live) not in allowed
                    or live.__code__.co_freevars != desired.__code__.co_freevars):
                raise RuntimeError('Unknown live role context function: ' + name)
            if signature(live) != allowed[1]:
                changes.append((live, live.__code__, desired.__code__))
        try:
            for live, previous, desired in changes:
                live.__code__ = desired
        except BaseException:
            for live, previous, desired in reversed(changes):
                live.__code__ = previous
            raise
        return {'status': 'applied' if changes else 'already_applied',
                'functions': [name for _, _, name, _, _ in staged]}
    finally:
        runtime.lock.release()
