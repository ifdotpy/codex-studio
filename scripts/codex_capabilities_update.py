"""Apply the capabilities query fix without restarting or replaying work."""
from pathlib import Path
import sys
from types import FunctionType, MethodType

from codex_active_task_update import compile_function, signature

BASE_COMMIT = 'bb4b49e'
EXPECTED = ('ca2545ccf327eac08709b22dc1c9744f899195cd9bf6626ad7914da969bf8ec7', '44fa82dd9dba04e87c3d7ed2677ff0261c5a57cd479e30e781f799b5bc85c98c')


def apply(runtime):
    import codex_workspace
    import codex_runtime
    if sys.version_info[:2] != (3, 14) or type(runtime) is not codex_runtime.Runtime:
        raise RuntimeError('Unknown runtime for the capabilities query update')
    directory = Path(__file__).resolve().parent
    source = directory / 'codex_workspace.py'
    if (Path(codex_workspace.__file__).resolve() != source
            or Path(codex_runtime.__file__).resolve() != directory / 'codex_runtime.py'):
        raise RuntimeError('Unexpected live capabilities module location')
    owner = codex_workspace.WorkspaceMixin
    if (codex_runtime.WorkspaceMixin is not owner or owner not in type(runtime).__mro__
            or owner.__module__ != 'codex_workspace' or owner.__name__ != 'WorkspaceMixin'):
        raise RuntimeError('Unknown live capabilities class')
    desired = compile_function(source.read_text(), 'WorkspaceMixin', 'capabilities', vars(codex_workspace), str(source))
    if signature(desired) != EXPECTED[1]:
        raise RuntimeError('Unreviewed capabilities query replacement')
    if not runtime.lock.acquire(timeout=10):
        raise RuntimeError('Runtime remains busy; no capabilities query update applied')
    try:
        if runtime.closed:
            raise RuntimeError('Runtime is closed; no capabilities query update applied')
        live = vars(owner).get('capabilities')
        bound = getattr(runtime, 'capabilities', None)
        inherited_override = any('capabilities' in vars(cls)
                                 for cls in type(runtime).__mro__[:type(runtime).__mro__.index(owner)])
        if ('capabilities' in vars(runtime) or inherited_override or not isinstance(bound, MethodType)
                or bound.__self__ is not runtime or bound.__func__ is not live):
            raise RuntimeError('Unexpected capabilities method override')
        if (not isinstance(live, FunctionType) or signature(live) not in EXPECTED
                or live.__globals__ is not vars(codex_workspace) or live.__module__ != 'codex_workspace'
                or live.__code__.co_freevars != desired.__code__.co_freevars):
            raise RuntimeError('Unknown live capabilities method')
        if signature(live) == EXPECTED[1]:
            return {'status': 'already_applied', 'baseCommit': BASE_COMMIT, 'methods': ['capabilities']}
        previous = live.__code__
        try:
            live.__code__ = desired.__code__
        except BaseException:
            if live.__code__ is not previous:
                live.__code__ = previous
            raise
        return {'status': 'applied', 'baseCommit': BASE_COMMIT, 'methods': ['capabilities']}
    finally:
        runtime.lock.release()
