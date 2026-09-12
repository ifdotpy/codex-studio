"""Apply the reviewed queue controls without stopping active Studio work."""
from pathlib import Path
import sys
from types import MethodType

from codex_active_task_update import compile_function, signature

BASE_COMMIT = 'f58c0db'
EXPECTED = (
    '735cad72c1c9177a25c9756f2e0eb0091c4e582fa7bb392f6fbba9e736490884',
    '4e263ff8b08c41c23b54cfa5f108ab60cebcf107c45bcb8c3faad1cfb9c34762',
)


def apply(runtime):
    import codex_runtime
    import codex_work
    if sys.version_info[:2] != (3, 14) or type(runtime) is not codex_runtime.Runtime:
        raise RuntimeError('Unknown runtime for the queue update')
    source = Path(__file__).resolve().with_name('codex_work.py')
    if (Path(codex_work.__file__).resolve() != source
            or Path(codex_runtime.__file__).resolve() != source.with_name('codex_runtime.py')):
        raise RuntimeError('Unexpected live queue module location')
    owner = codex_work.WorkMixin
    if (codex_runtime.WorkMixin is not owner or owner not in type(runtime).__mro__
            or owner.__module__ != 'codex_work'):
        raise RuntimeError('Unknown live queue class')
    desired = compile_function(source.read_text(), 'WorkMixin', 'queue_action', vars(codex_work), str(source))
    if signature(desired) != EXPECTED[1]:
        raise RuntimeError('Unreviewed queue replacement')
    if not runtime.lock.acquire(timeout=10):
        raise RuntimeError('Runtime remains busy; no queue update applied')
    try:
        if runtime.closed:
            raise RuntimeError('Runtime is closed; no queue update applied')
        live = vars(owner).get('queue_action')
        bound = getattr(runtime, 'queue_action', None)
        inherited_override = any('queue_action' in vars(cls)
                                 for cls in type(runtime).__mro__[:type(runtime).__mro__.index(owner)])
        if ('queue_action' in vars(runtime) or inherited_override or not isinstance(bound, MethodType)
                or bound.__self__ is not runtime or bound.__func__ is not live):
            raise RuntimeError('Unexpected queue method override')
        if (signature(live) not in EXPECTED or live.__globals__ is not vars(codex_work)
                or live.__code__.co_freevars != desired.__code__.co_freevars):
            raise RuntimeError('Unknown live queue method')
        if signature(live) == EXPECTED[1]:
            return {'status': 'already_applied', 'baseCommit': BASE_COMMIT, 'methods': ['queue_action']}
        previous = live.__code__
        try:
            live.__code__ = desired.__code__
        except BaseException:
            if live.__code__ is not previous:
                live.__code__ = previous
            raise
        return {'status': 'applied', 'baseCommit': BASE_COMMIT, 'methods': ['queue_action']}
    finally:
        runtime.lock.release()
