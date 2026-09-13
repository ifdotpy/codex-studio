"""Apply reviewed chat review timers without stopping active Studio work."""
import hashlib
import importlib.util
from pathlib import Path
import sys
from types import FunctionType, MethodType, ModuleType

from codex_active_task_update import compile_function, signature

BASE_COMMIT = '1acb7723c309b02051fdc9c175406a5777a2bfb0'
EXPECTED = {
    'codex_work.WorkMixin.chat_organization': (
        'e966214616ffb37fa70added9a579071b30d93fa8ecdcf0c0cb4914b00ef8501',
        '83fbe2fce26de374212d90e474aa7945378a598d0138cd97f9d374d45cdea1dc'),
    'codex_rules.RulesMixin.rules_tick': (
        '837622d1b3d5bf3fd7cdcb6a67815accf459f2f8545724ff39c1fd3e5ca3e085',
        '580666112d3276dc200aaee4f930cf741e0dc32e1fbadeefc3ca32ff9fbb24cb'),
}
HELPER_SHA = '85fd7de32557117b8cd6085ea8ac103d0761e626777de99cbed75d6795878065'
HELPER_NAME = 'codex_chat_reviews'


def _validate_helper(helper, reviewed, path):
    if (type(helper) is not ModuleType
            or Path(getattr(helper, '__file__', '')).resolve() != path
            or helper.__name__ != HELPER_NAME):
        raise RuntimeError('Unexpected chat review helper location or identity')
    metadata = {'__name__', '__doc__', '__package__', '__loader__', '__spec__', '__file__', '__cached__'}
    actual = {key: value for key, value in vars(helper).items() if key not in metadata}
    expected = {key: value for key, value in vars(reviewed).items() if key not in metadata}
    if actual.keys() != expected.keys():
        raise RuntimeError('Unknown live chat review helper globals')
    for name, desired in expected.items():
        live = actual[name]
        if isinstance(desired, FunctionType) and desired.__module__ == HELPER_NAME:
            if (not isinstance(live, FunctionType) or live.__module__ != HELPER_NAME
                    or live.__globals__ is not vars(helper) or live.__closure__ is not None
                    or signature(live) != signature(desired)):
                raise RuntimeError('Unknown live chat review helper function: ' + name)
        elif type(desired) in (str, int, float, bool, type(None), tuple, frozenset):
            if type(live) is not type(desired) or live != desired:
                raise RuntimeError('Unknown live chat review helper constant: ' + name)
        elif live is not desired:
            raise RuntimeError('Unknown live chat review helper dependency: ' + name)


def apply(runtime):
    import codex_runtime
    if sys.version_info[:2] != (3, 14) or type(runtime) is not codex_runtime.Runtime:
        raise RuntimeError('Unknown runtime for the chat review update')
    directory = Path(__file__).resolve().parent
    if Path(codex_runtime.__file__).resolve() != directory / 'codex_runtime.py':
        raise RuntimeError('Unexpected chat review runtime module location')
    helper_path = directory / (HELPER_NAME + '.py')
    helper_source = helper_path.read_bytes()
    if hashlib.sha256(helper_source).hexdigest() != HELPER_SHA:
        raise RuntimeError('Unreviewed chat review helper source')
    replacements = []
    for target, allowed in EXPECTED.items():
        module_name, owner_name, name = target.split('.')
        module = sys.modules.get(module_name)
        path = directory / (module_name + '.py')
        if module is None or Path(module.__file__).resolve() != path:
            raise RuntimeError('Unexpected chat review module location: ' + target)
        owner = getattr(module, owner_name)
        if (getattr(codex_runtime, owner_name, None) is not owner
                or owner not in type(runtime).__mro__ or owner.__module__ != module_name):
            raise RuntimeError('Unknown live chat review class: ' + target)
        desired = compile_function(path.read_text(), owner_name, name, vars(module), str(path))
        if signature(desired) != allowed[1]:
            raise RuntimeError('Unreviewed chat review replacement: ' + target)
        replacements.append((owner, name, desired, module, allowed))
    if not runtime.lock.acquire(timeout=10):
        raise RuntimeError('Runtime remains busy; no chat review update applied')
    originals = []
    missing = object()
    previous_helper = sys.modules.get(HELPER_NAME, missing)
    try:
        if runtime.closed:
            raise RuntimeError('Runtime is closed; no chat review update applied')
        for owner, name, desired, module, allowed in replacements:
            live = vars(owner).get(name)
            bound = getattr(runtime, name, None)
            inherited_override = any(name in vars(cls)
                                    for cls in type(runtime).__mro__[:type(runtime).__mro__.index(owner)])
            if (name in vars(runtime) or inherited_override or not isinstance(bound, MethodType)
                    or bound.__self__ is not runtime or bound.__func__ is not live):
                raise RuntimeError('Unexpected chat review method override: ' + name)
            if (signature(live) not in allowed or live.__globals__ is not vars(module)
                    or live.__code__.co_freevars != desired.__code__.co_freevars):
                raise RuntimeError('Unknown live chat review method: ' + name)
            originals.append((live, live.__code__))
        spec = importlib.util.spec_from_file_location(HELPER_NAME, helper_path)
        reviewed = importlib.util.module_from_spec(spec)
        exec(compile(helper_source, str(helper_path), 'exec', dont_inherit=True), vars(reviewed))
        helper = reviewed if previous_helper is missing else previous_helper
        _validate_helper(helper, reviewed, helper_path)
        if (previous_helper is not missing
                and all(signature(live) == row[4][1] for (live, _), row in zip(originals, replacements))):
            return {'status': 'already_applied', 'baseCommit': BASE_COMMIT, 'methods': list(EXPECTED)}
        try:
            sys.modules[HELPER_NAME] = helper
            for (live, _), (_, _, desired, _, _) in zip(originals, replacements):
                live.__code__ = desired.__code__
        except BaseException:
            for live, code in originals:
                if live.__code__ is not code:
                    live.__code__ = code
            if previous_helper is missing:
                sys.modules.pop(HELPER_NAME, None)
            else:
                sys.modules[HELPER_NAME] = previous_helper
            raise
        return {'status': 'applied', 'baseCommit': BASE_COMMIT, 'methods': list(EXPECTED)}
    finally:
        runtime.lock.release()
