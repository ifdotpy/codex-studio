"""Apply the reviewed chat read-state change without stopping live work."""
import hashlib
import importlib.util
from pathlib import Path
import sys
from types import MethodType

from codex_active_task_update import compile_function, signature

BASE_COMMIT = '48274b7531a47d20c85c5fd937449d0649ac3821'
EXPECTED = {
    'codex_runtime.Runtime.snapshot': (
        'e1061238eb00d0382a671659e733e66b6c6bc089e2bae4437a71653eb6418d1e',
        '97a2ce8650011ef46a5e53e29e6bdb49905ebde923cfc653fd16f80b43c44516'),
    'codex_work.WorkMixin.chat_organization': (
        '5794b30380f1bba2d0a3558d654d46dd80df1f074c50b36a77a868fda3d48f3e',
        'e966214616ffb37fa70added9a579071b30d93fa8ecdcf0c0cb4914b00ef8501'),
}
HELPER_SHA = 'b90e556be099468ca305cb98de0e08b200de4e4f6698313883e50dccb19b1675'
HELPER_SIGNATURE = 'cf1112fec7234af03f1d61211e191c6a5e59061ff007141ce1220a05e82bef4f'


def apply(runtime):
    import codex_runtime
    if sys.version_info[:2] != (3, 14) or type(runtime) is not codex_runtime.Runtime:
        raise RuntimeError('Unknown runtime for the chat status update')
    directory = Path(__file__).resolve().parent
    helper_name = 'codex_chat_read_state'
    helper_path = directory / (helper_name + '.py')
    helper_source = helper_path.read_bytes()
    if hashlib.sha256(helper_source).hexdigest() != HELPER_SHA:
        raise RuntimeError('Unreviewed chat read-state source')
    replacements = []
    for target, allowed in EXPECTED.items():
        module_name, owner_name, name = target.split('.')
        module = sys.modules.get(module_name)
        path = directory / (module_name + '.py')
        if module is None or Path(module.__file__).resolve() != path:
            raise RuntimeError('Unexpected chat status module location')
        desired = compile_function(path.read_text(), owner_name, name, vars(module), str(path))
        if signature(desired) != allowed[1]:
            raise RuntimeError('Unreviewed chat status replacement: ' + target)
        replacements.append((getattr(module, owner_name), name, desired, module, allowed))
    if not runtime.lock.acquire(timeout=10):
        raise RuntimeError('Runtime remains busy; no chat status update applied')
    originals = []
    missing = object()
    previous_helper = sys.modules.get(helper_name, missing)
    try:
        if runtime.closed:
            raise RuntimeError('Runtime is closed; no chat status update applied')
        for owner, name, desired, module, allowed in replacements:
            live = vars(owner).get(name)
            bound = getattr(runtime, name, None)
            if (name in vars(runtime) or not isinstance(bound, MethodType)
                    or bound.__self__ is not runtime or bound.__func__ is not live):
                raise RuntimeError('Unexpected chat status method override: ' + name)
            if (signature(live) not in allowed or live.__globals__ is not vars(module)
                    or live.__code__.co_freevars != desired.__code__.co_freevars):
                raise RuntimeError('Unknown live chat status method: ' + name)
            originals.append((live, live.__code__))
        if previous_helper is missing:
            spec = importlib.util.spec_from_file_location(helper_name, helper_path)
            helper = importlib.util.module_from_spec(spec)
            exec(compile(helper_source, str(helper_path), 'exec', dont_inherit=True), vars(helper))
        else:
            helper = previous_helper
        if (Path(getattr(helper, '__file__', '')).resolve() != helper_path
                or signature(getattr(helper, 'read_state', None)) != HELPER_SIGNATURE
                or helper.read_state.__globals__ is not vars(helper)):
            raise RuntimeError('Unknown live chat read-state helper')
        already = all(signature(live) == row[4][1] for (live, _), row in zip(originals, replacements))
        if already and previous_helper is not missing:
            return {'status': 'already_applied', 'baseCommit': BASE_COMMIT}
        try:
            sys.modules[helper_name] = helper
            for (live, _), (_, _, desired, _, _) in zip(originals, replacements):
                live.__code__ = desired.__code__
        except BaseException:
            for live, code in originals:
                live.__code__ = code
            if previous_helper is missing:
                sys.modules.pop(helper_name, None)
            else:
                sys.modules[helper_name] = previous_helper
            raise
        return {'status': 'applied', 'baseCommit': BASE_COMMIT, 'methods': list(EXPECTED)}
    finally:
        runtime.lock.release()
