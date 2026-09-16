"""Apply the reviewed chat snapshot fix without restarting active agents."""
from pathlib import Path
import sys
from types import FunctionType

from codex_active_task_update import compile_function, signature

EXPECTED = {
    'codex_workspace.asset_record': (
        'fd118ddc8f6f6a0c29de891047e604ecf2a924a2c1b4eed2e81c0be2b904e1f3',
        '16b39663ed307219bf57055f2b15863f2faa7a19f936e110e84b191ddb598ec6'),
    'codex_runtime.transcript': (
        '68bb67f6eac8357e8d94466c820e3fc5a031bad543e3012545301ec033d2d8a2',
        '1d6054e88f5477b973b047e9f13e4b91d586a8a72fcfcf159096c1d1f158a288'),
}


def apply(runtime):
    import codex_runtime
    import codex_workspace
    if sys.version_info[:2] != (3, 14) or type(runtime) is not codex_runtime.Runtime:
        raise RuntimeError('Unknown runtime for chat read update')
    directory = Path(__file__).resolve().parent
    # Install the optional snapshot argument before its transcript caller.
    targets = ((codex_workspace, 'WorkspaceMixin', 'asset_record'),
               (codex_runtime, 'Runtime', 'transcript'))
    staged = []
    for module, class_name, name in targets:
        path = directory / (module.__name__ + '.py')
        if Path(module.__file__).resolve() != path:
            raise RuntimeError('Unexpected chat read module location')
        owner = getattr(module, class_name)
        desired = compile_function(path.read_text(), class_name, name, vars(module))
        allowed = EXPECTED[module.__name__ + '.' + name]
        if signature(desired) != allowed[1]:
            raise RuntimeError('Unreviewed chat read replacement')
        staged.append((module, owner, name, desired, allowed))
    if not runtime.lock.acquire(timeout=10):
        raise RuntimeError('Runtime remains busy; no chat read update applied')
    try:
        if runtime.closed:
            raise RuntimeError('Runtime is closed')
        changes = []
        for module, owner, name, desired, allowed in staged:
            live = vars(owner).get(name)
            if (not isinstance(live, FunctionType) or live.__globals__ is not vars(module)
                    or live.__module__ != module.__name__ or signature(live) not in allowed
                    or live.__code__.co_freevars != desired.__code__.co_freevars
                    or getattr(runtime, name).__func__ is not live):
                raise RuntimeError('Unknown live chat read function: ' + name)
            if signature(live) != allowed[1]:
                changes.append((live, live.__code__, live.__defaults__, live.__kwdefaults__, desired))
        try:
            for live, previous, defaults, kwdefaults, desired in changes:
                live.__code__, live.__defaults__, live.__kwdefaults__ = (
                    desired.__code__, desired.__defaults__, desired.__kwdefaults__)
        except BaseException:
            for live, previous, defaults, kwdefaults, desired in reversed(changes):
                live.__code__, live.__defaults__, live.__kwdefaults__ = previous, defaults, kwdefaults
            raise
        return {'status': 'applied' if changes else 'already_applied',
                'functions': [name for _, _, name, _, _ in staged]}
    finally:
        runtime.lock.release()
