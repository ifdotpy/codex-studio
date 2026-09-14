"""Apply manual review requests without interrupting active work."""
from pathlib import Path
import sys
from types import FunctionType

from codex_active_task_update import compile_function, signature

EXPECTED = {'codex_chat_reviews.review_schedule': ('9589b0d087ca7b72f5d3f8c84a59dfc3a5f71d7bdd6c2766acf1e824ca259b78',
                                        'f31d4724cd8b38c478a73e070df5851af43521625b8fe89b2f6828db5aadc875'),
 'codex_chat_reviews.review_now': (None,
                                   '2231ffa8edddc141d9b6da6bd6337664565c23f76cc8db11ccbf74cd5d7a7a70'),
 'codex_chat_reviews._prompt': ('3bb746fd3fb4b7bda9143e1c97d294633445305fab1f71b45b1b2df5bafec04f',
                                '368bb349c0d10bd0d0a92470dfbc28fdd6d6d619ac1ab6457a29161b43011f00')}


def apply(runtime):
    import codex_runtime
    import codex_chat_reviews
    if sys.version_info[:2] != (3, 14) or type(runtime) is not codex_runtime.Runtime:
        raise RuntimeError('Unknown runtime for manual review update')
    directory = Path(__file__).resolve().parent
    targets = (
        (codex_chat_reviews, None, 'review_schedule'),
        (codex_chat_reviews, None, 'review_now'),
        (codex_chat_reviews, None, '_prompt'),
    )
    staged = []
    for module, class_name, name in targets:
        path = directory / (module.__name__ + '.py')
        if Path(module.__file__).resolve() != path:
            raise RuntimeError('Unexpected manual review module location')
        owner = getattr(module, class_name) if class_name else module
        desired = compile_function(path.read_text(), class_name, name, vars(module))
        allowed = EXPECTED[module.__name__ + '.' + name]
        if signature(desired) != allowed[1]:
            raise RuntimeError('Unreviewed manual review replacement')
        staged.append((module, owner, name, desired, allowed))
    if not runtime.lock.acquire(timeout=10):
        raise RuntimeError('Runtime remains busy; no manual review update applied')
    try:
        if runtime.closed:
            raise RuntimeError('Runtime is closed')
        additions = []
        changes = []
        for module, owner, name, desired, allowed in staged:
            live = vars(owner).get(name)
            if live is None and allowed[0] is None:
                additions.append((owner, name, desired))
                continue
            if (not isinstance(live, FunctionType) or live.__globals__ is not vars(module)
                    or live.__module__ != module.__name__ or signature(live) not in allowed
                    or live.__code__.co_freevars != desired.__code__.co_freevars):
                raise RuntimeError('Unknown live manual review function: ' + name)
            if signature(live) != allowed[1]:
                changes.append((live, live.__code__, desired.__code__))
        changed = bool(changes or additions)
        try:
            for owner, name, desired in additions:
                setattr(owner, name, desired)
            for live, previous, desired in changes:
                live.__code__ = desired
        except BaseException:
            for live, previous, desired in reversed(changes):
                live.__code__ = previous
            for owner, name, desired in additions:
                if getattr(owner, name, None) is desired:
                    delattr(owner, name)
            raise
        return {'status': 'applied' if changed else 'already_applied',
                'functions': [name for _, _, name, _, _ in staged]}
    finally:
        runtime.lock.release()
