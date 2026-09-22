"""Apply exact rejection classification and receipt reconciliation during transfers."""
from pathlib import Path
import sys
from types import FunctionType

from codex_active_task_update import compile_function, signature

EXPECTED = {'codex_tool_requests.request_result_outcome': ('bf75265bf28d747a8815e1021b60a5c14cbdc6a84b56d50f697b0334bece3adc', '3899bc2fa6b9fe7697dfc5201864000aae409d5c1dfd298a11a3eadaddd849f4')}


def apply(runtime):
    import codex_runtime
    import codex_tool_requests
    import codex_account_transfer
    if sys.version_info[:2] != (3, 14) or type(runtime) is not codex_runtime.Runtime:
        raise RuntimeError('Unknown runtime for rejection transfer update')
    directory = Path(__file__).resolve().parent
    targets = (
        (codex_tool_requests, None, 'request_result_outcome'),
    )
    staged = []
    for module, class_name, name in targets:
        path = directory / (module.__name__ + '.py')
        if Path(module.__file__).resolve() != path:
            raise RuntimeError('Unexpected rejection transfer module location')
        owner = getattr(module, class_name) if class_name else module
        desired = compile_function(path.read_text(), class_name, name, vars(module))
        allowed = EXPECTED[module.__name__ + '.' + name]
        if signature(desired) != allowed[1]:
            raise RuntimeError('Unreviewed rejection transfer replacement')
        staged.append((module, owner, name, desired, allowed))
    if not runtime.lock.acquire(timeout=10):
        raise RuntimeError('Runtime remains busy; no rejection transfer update applied')
    try:
        if runtime.closed:
            raise RuntimeError('Runtime is closed')
        transfers = vars(runtime).get('_account_transfers')
        if transfers is not None and (type(transfers) is not codex_account_transfer.AccountTransfers
                or transfers.rt is not runtime or 'local_blocker' in vars(transfers)):
            raise RuntimeError('Unknown account transfer instance')
        changes = []
        for module, owner, name, desired, allowed in staged:
            live = vars(owner).get(name)
            if (not isinstance(live, FunctionType) or live.__globals__ is not vars(module)
                    or live.__module__ != module.__name__ or signature(live) not in allowed
                    or live.__code__.co_freevars != desired.__code__.co_freevars):
                raise RuntimeError('Unknown live rejection transfer function: ' + name)
            if signature(live) != allowed[1]:
                changes.append((live, live.__code__, desired.__code__))
        try:
            for live, previous, desired in changes:
                live.__code__ = desired
        except BaseException:
            for live, previous, desired in reversed(changes):
                live.__code__ = previous
            raise
        return {'status': 'applied' if changes else 'already_applied',
                'functions': [name for _, _, name, _, _ in staged]}
    finally:
        runtime.lock.release()
