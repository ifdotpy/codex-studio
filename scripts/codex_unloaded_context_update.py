"""Apply the unloaded-context check without replacing connections or callbacks."""
from pathlib import Path
import sys
from types import FunctionType

from codex_active_task_update import compile_function, signature

BASE = 'f9b1f7dfa04d8acd05151c50d97a02b0391dc4e943147510cb081b48a4785e41'
TARGET = '14e08b9744aae38cabb3c38f284b9fda87e91f61a55e367a3c0ddd72ee896040'


def apply(runtime):
    import codex_runtime
    import codex_context_repair as repair
    if sys.version_info[:2] != (3, 14) or type(runtime) is not codex_runtime.Runtime:
        raise RuntimeError('Unknown runtime for unloaded context update')
    path = Path(__file__).resolve().with_name('codex_context_repair.py')
    if Path(repair.__file__).resolve() != path:
        raise RuntimeError('Unexpected context repair module location')
    desired = compile_function(path.read_text(), None, '_native_idle', vars(repair))
    if signature(desired) != TARGET:
        raise RuntimeError('Unreviewed context repair replacement')
    if not runtime.lock.acquire(timeout=10):
        raise RuntimeError('Runtime remains busy; no context update applied')
    try:
        if runtime.closed:
            raise RuntimeError('Runtime is closed')
        live = repair._native_idle
        if (not isinstance(live, FunctionType) or live.__globals__ is not vars(repair)
                or live.__module__ != repair.__name__ or signature(live) not in {BASE, TARGET}
                or live.__code__.co_freevars != desired.__code__.co_freevars):
            raise RuntimeError('Unknown live context repair function')
        if signature(live) == TARGET:
            return {'status': 'already_applied', 'functions': ['_native_idle']}
        # Existing frames can finish their read and enter the normal retry wait.
        # The next retry uses this function. No input or receipt is changed here.
        live.__code__ = desired.__code__
        return {'status': 'applied', 'functions': ['_native_idle']}
    finally:
        runtime.lock.release()
