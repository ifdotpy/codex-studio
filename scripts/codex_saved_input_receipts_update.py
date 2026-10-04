"""Apply exact saved input receipts without a server or native restart."""
import ast
import hashlib
import importlib
from pathlib import Path
import sys
from codex_source import signature, source_function

HASHES = {'codex_context_repair': 'fe42cbfbe5df4059f23947f4b73a224d636cf28aaf572cc3a9d6f9b1bc4bd784', 'codex_native_input_projection': '845f65c087ddb50e66461dbc725ac1872d2d0f2553c1605a302ab060559a51e2', 'codex_historical_input_receipts': 'a9efd7b7c6c59bd3cc793e3a3b9089e4c1010f7d869ed587de009d5022e986e5'}
BEFORE = {'761236ec797e70c42714512333c43a9ae2daadd41ef2a1ff6d898fc610140840',
          'f0d29697c0fba3ac2dd6feef9f54971c25427dda3dee06199dbfc2cab81fbfca'}
AFTER = '6418a8b0d2584623f4c8fd25ee6fa4321cc0be8dc300a0535caa10bd9a15e1be'
DEPENDENCIES = {'_identity': '80fa0af556ae6361072cae6d76af1ed7deedcbda905380d3639ec84e370f2da4', '_unsubmitted': 'cc91cb6045b9e3351683147bee5be3307cd4d755f045402a9d1d99e7ea506749', '_held_restart_marker': '5a0f047e71f9cec27e95da3a3abd046e60afbd43b6b7a78c24122d1fa290ea9a', '_historical_input_wait': '74f6cc9c8e86caaf802a454605d95f475ac1565a5e9167b4df381b31abf8cae3', '_historical_input_rows': '44d7ec4f288bda864bb5f5fae1ba69214ee7afc6cf0c92bbfec8452834cb19ef', '_accepted_input_turn': '742145d694a26e9819f112dcac600d1e94f01e8f1aefe48b44258baf268c5fe8'}


def apply(runtime):
    scripts = Path(__file__).resolve().parent
    module = sys.modules.get('codex_runtime')
    context = sys.modules.get('codex_context_repair')
    if (module is None or not isinstance(runtime, module.Runtime) or runtime.closed
            or Path(module.__file__).resolve().parent != scripts
            or context is None or Path(context.__file__).resolve().parent != scripts):
        raise RuntimeError('The running backend source identity differs')
    for name, expected in HASHES.items():
        path = scripts / (name + '.py')
        raw = path.read_bytes()
        if hashlib.sha256(raw).hexdigest() != expected:
            raise RuntimeError('The reviewed input receipt source differs: ' + name)
        if name != 'codex_context_repair':
            loaded = importlib.import_module(name)
            if Path(loaded.__file__).resolve() != path:
                raise RuntimeError('The saved input receipt module identity differs: ' + name)
            for node in ast.parse(raw).body:
                if isinstance(node, ast.FunctionDef):
                    desired_helper, _ = source_function(raw, [node.name], vars(loaded), str(path))
                    if signature(getattr(loaded, node.name)) != signature(desired_helper):
                        raise RuntimeError('The running saved receipt helper differs: ' + node.name)
            limits = {'READ_LIMIT': 2097152, 'SCAN_LIMIT': 33554432} if name == 'codex_native_input_projection' \
                else {'TEXT_LIMIT': 65536, 'CAPTURE_LIMIT': 1048576}
            if any(getattr(loaded, key) != value for key, value in limits.items()):
                raise RuntimeError('The running saved receipt limits differ: ' + name)
    path = scripts / 'codex_context_repair.py'
    desired, _ = source_function(path.read_bytes(), ['recover_unconfirmed_inputs'],
                                 vars(context), str(path))
    if signature(desired) != AFTER:
        raise RuntimeError('The reviewed input receipt function differs')
    with runtime.lock:
        current = context.recover_unconfirmed_inputs
        actual = signature(current)
        if actual not in BEFORE | {AFTER}:
            raise RuntimeError('The running input receipt function differs')
        for name, expected in DEPENDENCIES.items():
            if signature(getattr(context, name)) != expected:
                raise RuntimeError('The running input receipt guard differs: ' + name)
        if actual == AFTER:
            return {'status': 'already_applied'}
        current.__code__ = desired.__code__
        current.__defaults__ = desired.__defaults__
        current.__kwdefaults__ = desired.__kwdefaults__
    runtime.changed.set()
    return {'status': 'applied'}
