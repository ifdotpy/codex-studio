"""Apply the analytics transaction fix without restarting or replaying work."""
from pathlib import Path
import sys
from types import FunctionType, MethodType

from codex_active_task_update import compile_function, signature

BASE_COMMIT = 'bb4b49e'
EXPECTED = ('800b2b45be4a124850ac9d1adb708d341a67c7976d332b227109673a2cfbe2c6', 'a1616ddf456664073125fb8fe84ee9a53060c5aa5411e816ac9e8fb1c89506cb')


def apply(runtime):
    import codex_analytics
    import codex_runtime
    if sys.version_info[:2] != (3, 14) or type(runtime) is not codex_runtime.Runtime:
        raise RuntimeError('Unknown runtime for the analytics transaction update')
    directory = Path(__file__).resolve().parent
    source = directory / 'codex_analytics.py'
    if (Path(codex_analytics.__file__).resolve() != source
            or Path(codex_runtime.__file__).resolve() != directory / 'codex_runtime.py'):
        raise RuntimeError('Unexpected live analytics module location')
    owner = codex_analytics.AnalyticsMixin
    if (codex_runtime.AnalyticsMixin is not owner or owner not in type(runtime).__mro__
            or owner.__module__ != 'codex_analytics' or owner.__name__ != 'AnalyticsMixin'):
        raise RuntimeError('Unknown live analytics class')
    desired = compile_function(source.read_text(), 'AnalyticsMixin', 'analytics_safe', vars(codex_analytics), str(source))
    if signature(desired) != EXPECTED[1]:
        raise RuntimeError('Unreviewed analytics transaction replacement')
    if not runtime.lock.acquire(timeout=10):
        raise RuntimeError('Runtime remains busy; no analytics transaction update applied')
    try:
        if runtime.closed:
            raise RuntimeError('Runtime is closed; no analytics transaction update applied')
        live = vars(owner).get('analytics_safe')
        bound = getattr(runtime, 'analytics_safe', None)
        inherited_override = any('analytics_safe' in vars(cls)
                                 for cls in type(runtime).__mro__[:type(runtime).__mro__.index(owner)])
        if ('analytics_safe' in vars(runtime) or inherited_override or not isinstance(bound, MethodType)
                or bound.__self__ is not runtime or bound.__func__ is not live):
            raise RuntimeError('Unexpected analytics method override')
        if (not isinstance(live, FunctionType) or signature(live) not in EXPECTED
                or live.__globals__ is not vars(codex_analytics) or live.__module__ != 'codex_analytics'
                or live.__code__.co_freevars != desired.__code__.co_freevars):
            raise RuntimeError('Unknown live analytics method')
        if signature(live) == EXPECTED[1]:
            return {'status': 'already_applied', 'baseCommit': BASE_COMMIT, 'methods': ['analytics_safe']}
        previous = live.__code__
        try:
            live.__code__ = desired.__code__
        except BaseException:
            if live.__code__ is not previous:
                live.__code__ = previous
            raise
        return {'status': 'applied', 'baseCommit': BASE_COMMIT, 'methods': ['analytics_safe']}
    finally:
        runtime.lock.release()
