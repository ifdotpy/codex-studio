"""Install reviewed account selection and transfer fixes without restarting agents."""
from pathlib import Path
import sys
from types import FunctionType

from codex_active_task_update import signature
from codex_progress_update import source_function, _http_closure
from codex_resource_removal_update import _find_handler

BASE_COMMIT = '610c3eb'
EXPECTED = {'codex_account_transfer.AccountTransfers.__init__': ['863c1efa0f6f262c30d8937b1bcc7e1f9b2d8b2bdef69e430699f834d3e599ef',
                                                      '3c2977ae2359dc64a390426b854a58c3d05b41c77ef8b6447395eb5a296fe0f5'],
 'codex_account_transfer.AccountTransfers.commit': ['8fec45515ce9e171dbcb716d9d60e7365c60eaa24c57159a3897fddcc1e4ad23',
                                                    'b3c37d2e738498ea5a98316b423495f533eb40a78a192dcf2d3e9c4706b7c8af'],
 'codex_account_transfer.AccountTransfers.received': ['7a1369afa24951d22bd1ce775c3e091cc205c22ba7202b91261b5df71cc70133',
                                                      '7ffb7fc3e7a262f9889def34d134983464384418b0dc87aba51135abc2fa3c33'],
 'codex_account_transfer.AccountTransfers.run': ['c11ee8d3d6f7182b4c1b41d9c52881b1c0193a5e8d99ca463a518605a6e4f51e',
                                                 '73591107d65ee32cfdeb01bbed48f3af87da21da1005f195a0a78da63b9c0e41'],
 'codex_account_transfer.AccountTransfers.save': ['743a404c8319d1ca957c28eb1a7bb2f6cb7532bf06ba6c5f4e913bdb772d433c',
                                                  'befb38c397ff4ec01c5fed1c2ba2fe21718bc5b0809daba03908014749ce682d'],
 'codex_account_transfer.AccountTransfers.tick': ['87d0b6776108cfda85f055aeb310e9d984ec3b8eb6daa386eff1f4e921981d3a',
                                                  'e686e2a4d5aa4bb2f73d64796434b691299150857343b78498f36c638f43f0f5'],
 'codex_canvas.make_server.Handler.do_POST': ['f24b8e4bf756ce7967c0333bb2025d4985c19d61a614313548c40d6a3d0a07d6',
                                              'f702559c0583a80fd06fef2b27d5396d685bbd7c009f89dd0a18abde1a41415a']}


def _install(live, desired):
    live.__code__ = desired.__code__
    live.__defaults__ = desired.__defaults__
    live.__kwdefaults__ = desired.__kwdefaults__


def apply(runtime):
    import codex_runtime
    import codex_account_transfer
    if sys.version_info[:2] != (3, 14) or type(runtime) is not codex_runtime.Runtime:
        raise RuntimeError('Unknown transfer speed runtime')
    if not EXPECTED:
        raise RuntimeError('Transfer speed source review is incomplete')
    directory = Path(__file__).resolve().parent
    handler = _find_handler(runtime)
    _http_closure(handler.do_GET, runtime, handler)
    replacements = []
    for target, allowed in EXPECTED.items():
        module_name, *path = target.split('.')
        module = sys.modules.get(module_name)
        if module is None or Path(module.__file__).resolve() != directory / (module_name + '.py'):
            raise RuntimeError('Unexpected transfer speed module: ' + module_name)
        desired, static = source_function((directory / (module_name + '.py')).read_text(), path, vars(module))
        if static or signature(desired) != allowed[1]:
            raise RuntimeError('Unreviewed transfer speed source: ' + target)
        owner = handler if path[0] == 'make_server' else getattr(module, path[0])
        replacements.append((owner, path[-1], desired, allowed, target))
    if not runtime.lock.acquire(timeout=10):
        raise RuntimeError('Runtime busy; transfer speed update waits')
    try:
        if runtime.closed:
            raise RuntimeError('Runtime closed')
        transfers = vars(runtime).get('_account_transfers')
        if transfers is not None:
            names = {name for name, value in vars(codex_account_transfer.AccountTransfers).items()
                     if isinstance(value, (FunctionType, staticmethod))}
            if (type(transfers) is not codex_account_transfer.AccountTransfers or transfers.rt is not runtime
                    or names.intersection(vars(transfers))):
                raise RuntimeError('Unknown account transfer instance')
            if transfers.running or transfers.workers or transfers.futures:
                raise RuntimeError('An account transfer is active; retry after its receipt')
        _http_closure(handler.do_GET, runtime, handler)
        originals = []
        for owner, name, desired, allowed, target in replacements:
            live = vars(owner).get(name)
            if (not isinstance(live, FunctionType) or signature(live) not in allowed
                    or live.__globals__ is not desired.__globals__
                    or live.__module__ != desired.__module__
                    or live.__code__.co_freevars != desired.__code__.co_freevars):
                raise RuntimeError('Unknown live transfer speed method: ' + target)
            if signature(live) != allowed[1]:
                originals.append((live, desired, (live.__code__, live.__defaults__, live.__kwdefaults__)))
        try:
            for live, desired, _ in originals:
                _install(live, desired)
        except BaseException:
            for live, _, previous in reversed(originals):
                live.__code__, live.__defaults__, live.__kwdefaults__ = previous
            raise
        return {'status': 'applied' if originals else 'already_applied',
                'baseCommit': BASE_COMMIT, 'methods': list(EXPECTED)}
    finally:
        runtime.lock.release()
