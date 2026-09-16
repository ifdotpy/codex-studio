"""Apply exact turn pagination and native queue guards without interruption."""
from pathlib import Path
import sys
from types import FunctionType

from codex_active_task_update import compile_function, signature

EXPECTED = {'codex_turn_recovery.read_native_turn': (None,
                                          '8832228f92541cefb0ddc68f741c64f3b0155d345b9d8c8f66780d0a78920d86'),
 'codex_turn_recovery.reconcile_turn': ('7f8741054a055af1c963158e42ca2cc33bf7cb4e186d15708fd0f27a051cb025',
                                        'f6f586d74a5eb1ddc703c2f7bd0855fa7f0e5caca1cc7426b34e6943b880b02a'),
 'codex_connection_recovery.recover': ('795627e8bef6226991732909522e613f83737b50a08085390ca6bbaa2d32cf87',
                                       '6558f541e2db762638737115c9f3b4dd8599bb3331963e7cbb4a055cbb525d8d'),
 'codex_account_transfer.wait_for_native_queue': (None,
                                                  '569b145b440c5446919bec5784705fcb4acbd30a70a700c0124f51ddd31779d2'),
 'codex_account_transfer.run': ('cbfd1156b08a1946b4b149293fecdbd67f4b778e87a2d025b9403708286b06fb',
                                'c11ee8d3d6f7182b4c1b41d9c52881b1c0193a5e8d99ca463a518605a6e4f51e')}


def apply(runtime):
    import codex_runtime
    import codex_turn_recovery
    import codex_connection_recovery
    import codex_account_transfer
    if sys.version_info[:2] != (3, 14) or type(runtime) is not codex_runtime.Runtime:
        raise RuntimeError('Unknown runtime for recovery boundary update')
    directory = Path(__file__).resolve().parent
    targets = (
        (codex_turn_recovery, None, 'read_native_turn'),
        (codex_turn_recovery, 'TurnRecoveryMixin', 'reconcile_turn'),
        (codex_connection_recovery, None, 'recover'),
        (codex_account_transfer, 'AccountTransfers', 'wait_for_native_queue'),
        (codex_account_transfer, 'AccountTransfers', 'run'),
    )
    staged = []
    for module, class_name, name in targets:
        path = directory / (module.__name__ + '.py')
        if Path(module.__file__).resolve() != path:
            raise RuntimeError('Unexpected recovery boundary module location')
        owner = getattr(module, class_name) if class_name else module
        desired = compile_function(path.read_text(), class_name, name, vars(module))
        allowed = EXPECTED[module.__name__ + '.' + name]
        if signature(desired) != allowed[1]:
            raise RuntimeError('Unreviewed recovery boundary replacement')
        staged.append((module, owner, name, desired, allowed))
    if not runtime.lock.acquire(timeout=10):
        raise RuntimeError('Runtime remains busy; no recovery boundary update applied')
    try:
        if runtime.closed:
            raise RuntimeError('Runtime is closed')
        transfers = getattr(runtime, '_account_transfers', None)
        if transfers is not None and transfers.running:
            raise RuntimeError('An account transfer is active; retry the update after its current step')
        if runtime.reconcile_turn.__func__ is not codex_turn_recovery.TurnRecoveryMixin.reconcile_turn:
            raise RuntimeError('Unknown bound turn recovery method')
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
                raise RuntimeError('Unknown live recovery boundary function: ' + name)
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
