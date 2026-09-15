"""Apply compact review briefs and semantic change detection without interruption."""
from pathlib import Path
import sys
from types import FunctionType

from codex_active_task_update import compile_function, signature

EXPECTED = {'codex_chat_reviews.review_now': ('2231ffa8edddc141d9b6da6bd6337664565c23f76cc8db11ccbf74cd5d7a7a70',
                                   'e20b3be19d581c7b2ed8b50dd44f83cd95f5074530a15a165d18e5a73130e46f'),
 'codex_chat_reviews.review_tick': ('ae1ae39a20b0dcf139e3cb5981cfa5be7aa5e8c7c2a7e96a5713e4de7448dbed',
                                    'd1f10e9c37df03c3eca90a5a14d05efb6c2117d7fdd60c3248576be0b2facb71'),
 'codex_chat_reviews._outcome_text': ('74c7e27e91a313a4014f70ffe37b821bd4f244f4bb9e57669d6d7e6004dfb349',
                                      'e2ac4129fbc8ac2d910f10846047eae4829fbd03dbd1fec1fedcf22e87cb76db'),
 'codex_chat_reviews._snapshot': ('166190816468b9243ae2ba62d8433cc7b7899cc2b3aafc2bca3138027818f7c4',
                                  'e3c2e3457291cfc8934419cae5dfd0a542f4c3701faa81422a556199825c8046'),
 'codex_chat_reviews._prompt': ('368bb349c0d10bd0d0a92470dfbc28fdd6d6d619ac1ab6457a29161b43011f00',
                                '2f5569ce1c2018aff5045f975c7ce82d42151887f057205f15a92e33cb88aff7'),
 'codex_chat_reviews._review_baseline': (None,
                                         'cb7cfded4eaa922ad8f3859a3cd6314cdb56a5828768995d1484997de6d610a5')}


def apply(runtime):
    import codex_runtime
    import codex_chat_reviews
    if sys.version_info[:2] != (3, 14) or type(runtime) is not codex_runtime.Runtime:
        raise RuntimeError('Unknown runtime for review brief update')
    directory = Path(__file__).resolve().parent
    targets = (
        (codex_chat_reviews, None, 'review_now'),
        (codex_chat_reviews, None, 'review_tick'),
        (codex_chat_reviews, None, '_outcome_text'),
        (codex_chat_reviews, None, '_snapshot'),
        (codex_chat_reviews, None, '_prompt'),
        (codex_chat_reviews, None, '_review_baseline'),
    )
    staged = []
    for module, class_name, name in targets:
        path = directory / (module.__name__ + '.py')
        if Path(module.__file__).resolve() != path:
            raise RuntimeError('Unexpected review brief module location')
        owner = getattr(module, class_name) if class_name else module
        desired = compile_function(path.read_text(), class_name, name, vars(module))
        allowed = EXPECTED[module.__name__ + '.' + name]
        if signature(desired) != allowed[1]:
            raise RuntimeError('Unreviewed review brief replacement')
        staged.append((module, owner, name, desired, allowed))
    if not runtime.lock.acquire(timeout=10):
        raise RuntimeError('Runtime remains busy; no review brief update applied')
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
                raise RuntimeError('Unknown live review brief function: ' + name)
            if (signature(live) != allowed[1] or live.__defaults__ != desired.__defaults__
                    or live.__kwdefaults__ != desired.__kwdefaults__):
                changes.append((live, live.__code__, live.__defaults__, live.__kwdefaults__, desired))
        changed = bool(changes or additions)
        try:
            for owner, name, desired in additions:
                setattr(owner, name, desired)
            for live, previous, defaults, kwdefaults, desired in changes:
                live.__code__, live.__defaults__, live.__kwdefaults__ = desired.__code__, desired.__defaults__, desired.__kwdefaults__
        except BaseException:
            for live, previous, defaults, kwdefaults, desired in reversed(changes):
                live.__code__, live.__defaults__, live.__kwdefaults__ = previous, defaults, kwdefaults
            for owner, name, desired in additions:
                if getattr(owner, name, None) is desired:
                    delattr(owner, name)
            raise
        return {'status': 'applied' if changed else 'already_applied',
                'functions': [name for _, _, name, _, _ in staged]}
    finally:
        runtime.lock.release()
