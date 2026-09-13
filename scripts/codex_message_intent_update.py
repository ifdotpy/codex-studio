"""Apply the reviewed message intent projection without stopping Studio."""
from pathlib import Path
import sys
from types import MethodType

from codex_active_task_update import compile_function, signature

BASE_COMMIT = '92a8c7d'
EXPECTED = (
    'a339b1ea20aa4d8f20e4526bb037b0f3b46237e42c1a670ebd100ac0226ee8f1',
    '68bb67f6eac8357e8d94466c820e3fc5a031bad543e3012545301ec033d2d8a2',
)


def apply(runtime):
    import codex_runtime
    owner = codex_runtime.Runtime
    if sys.version_info[:2] != (3, 14) or type(runtime) is not owner:
        raise RuntimeError('Unknown runtime for the message intent update')
    source = Path(__file__).resolve().with_name('codex_runtime.py')
    if Path(codex_runtime.__file__).resolve() != source:
        raise RuntimeError('Unexpected live message intent module location')
    if owner.__module__ != 'codex_runtime' or owner.__name__ != 'Runtime':
        raise RuntimeError('Unknown live message intent class')
    desired = compile_function(source.read_text(), 'Runtime', 'transcript', vars(codex_runtime), str(source))
    if signature(desired) != EXPECTED[1]:
        raise RuntimeError('Unreviewed message intent replacement')
    if not runtime.lock.acquire(timeout=10):
        raise RuntimeError('Runtime remains busy; no message intent update applied')
    try:
        if runtime.closed:
            raise RuntimeError('Runtime is closed; no message intent update applied')
        live = vars(owner).get('transcript')
        bound = getattr(runtime, 'transcript', None)
        if ('transcript' in vars(runtime) or not isinstance(bound, MethodType)
                or bound.__self__ is not runtime or bound.__func__ is not live):
            raise RuntimeError('Unexpected transcript method override')
        if (signature(live) not in EXPECTED or live.__globals__ is not vars(codex_runtime)
                or live.__module__ != 'codex_runtime'
                or live.__code__.co_freevars != desired.__code__.co_freevars):
            raise RuntimeError('Unknown live transcript method')
        if signature(live) == EXPECTED[1]:
            return {'status': 'already_applied', 'baseCommit': BASE_COMMIT, 'methods': ['transcript']}
        previous = live.__code__
        try:
            live.__code__ = desired.__code__
        except BaseException:
            if live.__code__ is not previous:
                live.__code__ = previous
            raise
        return {'status': 'applied', 'baseCommit': BASE_COMMIT, 'methods': ['transcript']}
    finally:
        runtime.lock.release()
