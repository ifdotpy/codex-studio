"""Apply reviewed provider transfers without restarting native sessions."""
import ast
import hashlib
import importlib
from pathlib import Path
import sys
from types import FunctionType

from codex_active_task_update import signature
from codex_progress_update import source_function

BASE_COMMIT = '016e38d'
EXPECTED = {'codex_runtime.Runtime.tool_definitions': ['61c6477e3e232adfc145eabc8a4893f69ab608330cc80d02e00915146bec294e',
                                            '13fe92bce87c320a02c6549abb73ce5d6da12227bf82410f778fa19d076d1a6b'],
 'codex_runtime.Runtime.new_thread_params': ['edb3cc2c27e066a16eb891107563c9896f3eea3543c591adeeb87102ebfb6876',
                                             'e2e34a2a76cb8224e75ca2d29e149ccebf81c4252dc234608b3d2ae8d5a6eacb'],
 'codex_runtime.Runtime.prepare_locked': ['aab8c519064c7136449f0351ce4682ef0161d3f3468be55ba8a9e5ecf1a05d7c',
                                          'b8d52cf8883f8cf91422b1d4fab53e088607ce0503540cf025d641a06fab12b2'],
 'codex_claude_controls.retire_idle_bridge': ['4d9b7c7f29231ab868d4ff90dc587eb653789ffce3bc465f63db2bf922fdbc6f',
                                              'd4abaf1e028d005f5f5aaf99d15fe5cd958d153f6a0996634bcf8f6c4b108d16'],
 'codex_account_transfer.AccountTransfers.save': ['befb38c397ff4ec01c5fed1c2ba2fe21718bc5b0809daba03908014749ce682d',
                                                  'c0d2f701d4e154fbb6f3e6704450eee80f3c69b26a8000e516da4dd8c1a352be'],
 'codex_account_transfer.AccountTransfers.check_destination': ['935e54abb3004ab11ee3ddc43a48f01cbae5a8f85bfde6d4340c7c0352bd2ca4',
                                                               '6dce47803852b1ee2f7eefc678121e99cc222d6545d4ce7de22df96fc80fee18'],
 'codex_account_transfer.AccountTransfers.settings_snapshot': [None,
                                                               'f7e6a7bbdfb7b276f7ec3cde00e6d7e43a67d272299f06341abb840af1cec17d'],
 'codex_account_transfer.AccountTransfers.destination_settings': [None,
                                                                  '0b3b97e6f9201982197da27ec30bbf47b2310f752be4a3e17ff8e1c41b972639'],
 'codex_account_transfer.AccountTransfers.action': ['2aefcfecd6f3e122a04c7b8e148dacf4a0824dd4c3a065fe87c2f552d13cfe22',
                                                    '068f3b041a4a10494705d8c812910d57747790e1dd4b451a16b22dadb3d63177'],
 'codex_account_transfer.AccountTransfers.wait_for_native_queue': ['569b145b440c5446919bec5784705fcb4acbd30a70a700c0124f51ddd31779d2',
                                                                   'e8bf5e3fb2c663b774f75cfe4d6b15e2a2825be05aac779e7cc5dc98452df657'],
 'codex_account_transfer.AccountTransfers.invalidate_archive': [None,
                                                                'f7cb5254c133a9a694ab8a5a5c9e694db32bbcc1cb557970bbcc922cc6e80d70'],
 'codex_account_transfer.AccountTransfers.same_native_history': [None,
                                                                 'a9d25fce1c54bf80a607f9732ff7176a38a538cf3eea7844a02486b1bd1084a6'],
 'codex_account_transfer.AccountTransfers.archive_source_current': [None,
                                                                    '2e4eb2156ac74c5f149154b6d8321bd36f31750b2dc1561872cb77a27bc28bcb'],
 'codex_account_transfer.AccountTransfers.run': ['73591107d65ee32cfdeb01bbed48f3af87da21da1005f195a0a78da63b9c0e41',
                                                 '47816f219667aff31f9d92102c8f8b6031a6aebeb3b58097d4ac7c001138016d'],
 'codex_account_transfer.AccountTransfers.assert_settings': ['29addd9da8af12a9505c9e42ce2863d367271a02dd1be4f5de48403fa56be2c0',
                                                             'cc7e11a160692a5e06aef14fafc23ac2c9a982da1a42ede17d194e86fae62f59'],
 'codex_account_transfer.AccountTransfers.received': ['7ffb7fc3e7a262f9889def34d134983464384418b0dc87aba51135abc2fa3c33',
                                                      'd0a3b58e988e18251b1967308ed2e8d56dd464175bd63271d081cafa067303ce'],
 'codex_account_transfer.AccountTransfers.commit': ['b3c37d2e738498ea5a98316b423495f533eb40a78a192dcf2d3e9c4706b7c8af',
                                                    '65775c6308507104e795d32e0a5f6c273bde9f7e66b3886875ce40dc94acbec1']}
PORTABLE_SHA256 = 'bd4d3600ab27509af96eb1c37e459355669fccd4072f3f7393f8c755f4609119'


def _install(live, desired):
    live.__code__, live.__defaults__, live.__kwdefaults__ = desired.__code__, desired.__defaults__, desired.__kwdefaults__


def _portable(directory):
    path = directory / 'codex_portable_history.py'
    raw = path.read_bytes()
    if hashlib.sha256(raw).hexdigest() != PORTABLE_SHA256:
        raise RuntimeError('Unreviewed portable history source')
    module = sys.modules.get('codex_portable_history')
    if module is None:
        return
    if Path(getattr(module, '__file__', '')).resolve() != path:
        raise RuntimeError('Unknown portable history module origin')
    namespace = vars(module)
    for node in ast.parse(raw).body:
        if isinstance(node, ast.FunctionDef):
            live = namespace.get(node.name)
            desired, static = source_function(raw, (node.name,), namespace)
            if (static or not isinstance(live, FunctionType) or live.__globals__ is not namespace
                    or live.__module__ != module.__name__ or signature(live) != signature(desired)):
                raise RuntimeError('Unknown loaded portable history function: ' + node.name)
        elif isinstance(node, ast.ClassDef):
            owner = namespace.get(node.name)
            if (not isinstance(owner, type) or type(owner) is not type or owner.__bases__ != (object,)
                    or owner.__module__ != module.__name__ or node.bases or node.keywords or node.decorator_list):
                raise RuntimeError('Unknown portable history class')
            names = {child.name for child in node.body if isinstance(child, ast.FunctionDef)}
            if set(vars(owner)) - names - {'__module__', '__doc__', '__dict__', '__weakref__', '__firstlineno__', '__static_attributes__'}:
                raise RuntimeError('Unknown portable history class members')
            for child in node.body:
                if not isinstance(child, ast.FunctionDef):
                    raise RuntimeError('Unknown portable history class structure')
                desired, static = source_function(raw, (node.name, child.name), namespace)
                live = vars(owner).get(child.name)
                if (static or not isinstance(live, FunctionType) or live.__globals__ is not namespace
                        or live.__module__ != module.__name__ or signature(live) != signature(desired)):
                    raise RuntimeError('Unknown loaded portable history method: ' + child.name)
        elif isinstance(node, ast.Assign):
            value = ast.literal_eval(node.value)
            if any(not isinstance(target, ast.Name) or namespace.get(target.id) != value for target in node.targets):
                raise RuntimeError('Unknown portable history constant')
        elif isinstance(node, ast.Import):
            for alias in node.names:
                if namespace.get(alias.asname or alias.name) is not importlib.import_module(alias.name):
                    raise RuntimeError('Unknown portable history import')
        elif isinstance(node, ast.ImportFrom):
            imported = importlib.import_module(node.module)
            for alias in node.names:
                if namespace.get(alias.asname or alias.name) is not getattr(imported, alias.name):
                    raise RuntimeError('Unknown portable history import')
        elif not (isinstance(node, ast.Expr) and isinstance(node.value, ast.Constant) and isinstance(node.value.value, str)):
            raise RuntimeError('Unknown portable history module structure')


def apply(runtime):
    import copy
    import codex_runtime
    import codex_account_transfer
    import codex_claude_controls
    if sys.version_info[:2] != (3, 14) or type(runtime) is not codex_runtime.Runtime:
        raise RuntimeError('Unknown provider transfer runtime')
    if not EXPECTED or not PORTABLE_SHA256:
        raise RuntimeError('Provider transfer source review is incomplete')
    directory = Path(__file__).resolve().parent
    _portable(directory)
    replacements = []
    for target, allowed in EXPECTED.items():
        module_name, *path = target.split('.')
        module = sys.modules.get(module_name)
        if module is None or Path(getattr(module, '__file__', '')).resolve() != directory / (module_name + '.py'):
            raise RuntimeError('Unexpected provider transfer module: ' + module_name)
        desired, static = source_function((directory / (module_name + '.py')).read_text(), path, vars(module))
        if signature(desired) != allowed[1]:
            raise RuntimeError('Unreviewed provider transfer source: ' + target)
        owner = getattr(module, path[0]) if len(path) > 1 else module
        replacements.append((owner, path[-1], desired, static, allowed, target))
    if not runtime.start_lock.acquire(timeout=10):
        raise RuntimeError('Native connection is busy; provider transfer update waits')
    try:
        if not runtime.lock.acquire(timeout=10):
            raise RuntimeError('Runtime is busy; provider transfer update waits')
        try:
            if runtime.closed:
                raise RuntimeError('Runtime closed')
            _portable(directory)
            if any(target.split('.')[-1] in vars(runtime) for target in EXPECTED if target.startswith('codex_runtime.Runtime.')):
                raise RuntimeError('Unknown provider transfer runtime override')
            transfers = vars(runtime).get('_account_transfers')
            if transfers is not None:
                names = {name for name, value in vars(codex_account_transfer.AccountTransfers).items() if isinstance(value, (FunctionType, staticmethod))}
                names.update(target.split('.')[-1] for target in EXPECTED if target.startswith('codex_account_transfer.AccountTransfers.'))
                if type(transfers) is not codex_account_transfer.AccountTransfers or transfers.rt is not runtime or names.intersection(vars(transfers)):
                    raise RuntimeError('Unknown account transfer instance')
                if transfers.running or transfers.workers or transfers.futures:
                    raise RuntimeError('An account transfer is active; wait for its receipt')
                with runtime.db() as db:
                    if any(op.get('status') not in {'completed', 'cancelled'} and any(m.get('phase') in {'reading', 'submitted'} for m in op.get('members', {}).values()) for op in runtime.records(db, 'account_transfers')):
                        raise RuntimeError('An account transfer is active; wait for its receipt')
            globals_before = vars(codex_account_transfer).get('copy')
            if globals_before is not None and globals_before is not copy:
                raise RuntimeError('Unknown provider transfer copy import')
            originals = []
            for owner, name, desired, static, allowed, target in replacements:
                descriptor = vars(owner).get(name)
                live = descriptor.__func__ if isinstance(descriptor, staticmethod) else descriptor
                if live is None and allowed[0] is None:
                    originals.append((owner, name, None, desired, static, None))
                    continue
                if (not isinstance(live, FunctionType) or isinstance(descriptor, staticmethod) != static
                        or signature(live) not in allowed or live.__globals__ is not desired.__globals__
                        or live.__module__ != desired.__module__ or live.__code__.co_freevars != desired.__code__.co_freevars):
                    raise RuntimeError('Unknown live provider transfer method: ' + target)
                if signature(live) != allowed[1]:
                    originals.append((owner, name, live, desired, static, (live.__code__, live.__defaults__, live.__kwdefaults__)))
            try:
                codex_account_transfer.copy = copy
                for owner, name, live, desired, static, _ in originals:
                    if live is None:
                        setattr(owner, name, staticmethod(desired) if static else desired)
                    else:
                        _install(live, desired)
            except BaseException:
                for owner, name, live, _, _, previous in reversed(originals):
                    if live is None:
                        if name in vars(owner):
                            delattr(owner, name)
                    else:
                        live.__code__, live.__defaults__, live.__kwdefaults__ = previous
                if globals_before is None:
                    vars(codex_account_transfer).pop('copy', None)
                else:
                    codex_account_transfer.copy = globals_before
                raise
            return {'status': 'applied' if originals else 'already_applied', 'baseCommit': BASE_COMMIT,
                    'methods': list(EXPECTED), 'portableHistorySha256': PORTABLE_SHA256}
        finally:
            runtime.lock.release()
    finally:
        runtime.start_lock.release()
