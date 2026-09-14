"""Defer optional context changes while a native background command runs."""
from pathlib import Path
import sys
from types import FunctionType

from codex_active_task_update import compile_function, signature

EXPECTED = {'_local_idle': ('db33f9769072f8c583a63546088943a02b817fa86a3b97727dacc94b326b1b98',
                 'c5bed0241908919b136da69e5124505f57f8901f156f75b0390177bec87432ab'),
 '_optional_monitor_repair': ('8e399bd5fdd73ecb1076d6a2a08bfdf35bf9b05da6725835239c603fbf365962',
                              'eeecd47c259d9a55b2f5db3aca9d08f80f2ce1cbe9decaff94f66a3f60f1ddc8'),
 'claim_context_wait': ('105692a9eda133506c5242640c75cd85deef0957d698284868bf10b634a26e46',
                        '48ca279f0dd421a28af3afc8f0b9844e9ae403971a71078d0707c4fdfe21b931')}


def apply(runtime):
    import codex_runtime
    import codex_context_repair as repair
    if sys.version_info[:2] != (3, 14) or type(runtime) is not codex_runtime.Runtime:
        raise RuntimeError('Unknown runtime for background context update')
    source = Path(__file__).resolve().parent / 'codex_context_repair.py'
    if Path(repair.__file__).resolve() != source:
        raise RuntimeError('Unexpected context repair module location')
    raw = source.read_text()
    staged = []
    for name, allowed in EXPECTED.items():
        desired = compile_function(raw, None, name, vars(repair))
        if signature(desired) != allowed[1]:
            raise RuntimeError('Unreviewed background context replacement')
        staged.append((name, desired, allowed))
    if not runtime.lock.acquire(timeout=10):
        raise RuntimeError('Runtime remains busy; no background context update applied')
    try:
        if runtime.closed:
            raise RuntimeError('Runtime is closed')
        changes = []
        for name, desired, allowed in staged:
            live = vars(repair).get(name)
            if (not isinstance(live, FunctionType) or live.__globals__ is not vars(repair)
                    or live.__module__ != repair.__name__ or signature(live) not in allowed
                    or live.__code__.co_freevars != desired.__code__.co_freevars):
                raise RuntimeError('Unknown live background context function: ' + name)
            if signature(live) != allowed[1]:
                changes.append((live, live.__code__, live.__kwdefaults__, desired))
        try:
            for live, previous, defaults, desired in changes:
                live.__code__ = desired.__code__
                live.__kwdefaults__ = desired.__kwdefaults__
        except BaseException:
            for live, previous, defaults, desired in reversed(changes):
                live.__code__ = previous
                live.__kwdefaults__ = defaults
            raise
        return {'status': 'applied' if changes else 'already_applied', 'functions': list(EXPECTED)}
    finally:
        runtime.lock.release()
