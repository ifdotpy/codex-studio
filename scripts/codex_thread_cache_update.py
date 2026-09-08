"""Apply the thread-cache fix to the inspected live runtime without a restart.

The notification source comes from the exact reviewed Git revision. Existing
notification behavior stays intact. Only three bound methods are replaced.
"""
import ast
import copy
import json
from pathlib import Path
import types

from codex_efficiency_update import fingerprint
from codex_native_errors import NativeRpcError

NOTIFICATION_REVISION = '4bcf0b03d2d57ebf2d009de4db6910fbcd051377'
BASE = {
    'notification': '956d07ca388aa0887fb4b0a96d8342f1edbbe65b976698b7818e9212cbef2048',
    'start_error': 'ebb4e0d5ea06e8043298dd614ecd26d9c25b83c1debb125328e439c8bc8c9572',
    'prepared_result': 'e8b23327406c268f761f4ee2ce7a6272915a251e1d2658fc072656a3c264d022',
}
PREPARE = 'af1b717ed8d13ede31c3fb82f54a73817506099d999dfdfb28e8cbec8267a677'


def normalize_legacy_missing_thread(error):
    # Existing reader frames produce RuntimeError(JSON), not NativeRpcError.
    # Accept only the exact structured native rejection. Never parse a timeout.
    if type(error) is RuntimeError and len(str(error)) < 2048:
        try:
            value = json.loads(str(error))
        except (ValueError, TypeError):
            return error
        if (isinstance(value, dict) and value.get('code') == -32600
                and isinstance(value.get('message'), str)
                and value['message'].startswith('thread not found: ')):
            return NativeRpcError(value)
    return error


def method_node(tree, name):
    cls = next(n for n in tree.body if isinstance(n, ast.ClassDef) and n.name == 'Runtime')
    return next(n for n in cls.body if isinstance(n, ast.FunctionDef) and n.name == name)


def method_code(tree, name):
    module = compile(ast.fix_missing_locations(tree), '<studio-thread-cache-update>', 'exec', dont_inherit=True)
    cls = next(c for c in module.co_consts if isinstance(c, types.CodeType) and c.co_name == 'Runtime')
    return next(c for c in cls.co_consts if isinstance(c, types.CodeType) and c.co_name == name)


def replacements(runtime, notification_source):
    baseline = ast.parse(notification_source)
    code = method_code(baseline, 'notification')
    if fingerprint(types.FunctionType(code, {})) != BASE['notification']:
        raise RuntimeError('Unknown notification source; no update applied')
    current = ast.parse(Path(__file__).with_name('codex_runtime.py').read_text())
    notification = method_node(baseline, 'notification')
    source = method_node(current, 'notification')
    unload = next(n for n in ast.walk(source) if isinstance(n, ast.If)
                  and isinstance(n.test, ast.BoolOp)
                  and any(isinstance(v, ast.Constant) and v.value == 'thread/closed' for v in ast.walk(n.test)))
    anchors = []
    for node in ast.walk(notification):
        body = getattr(node, 'body', None)
        if not isinstance(body, list):
            continue
        for index, child in enumerate(body):
            if isinstance(child, ast.If) and ast.unparse(child.test) == "a.get('deletedAt')":
                anchors.append((body, index))
    if len(anchors) != 1:
        raise RuntimeError('Notification anchor changed; no update applied')
    body, index = anchors[0]
    body.insert(index + 1, copy.deepcopy(unload))
    method_node(current, 'start_error').body.insert(0, ast.parse('error = normalize_legacy_missing_thread(error)').body[0])
    methods = {}
    for name in BASE:
        previous = getattr(runtime, name).__func__
        scope = previous.__globals__.copy()
        scope.update(NativeRpcError=NativeRpcError, normalize_legacy_missing_thread=normalize_legacy_missing_thread)
        function = types.FunctionType(method_code(baseline if name == 'notification' else current, name),
                                      scope, name, previous.__defaults__)
        function.__kwdefaults__ = previous.__kwdefaults__
        methods[name] = types.MethodType(function, runtime)
    return methods


def apply(runtime, notification_source):
    methods = replacements(runtime, notification_source)
    if not runtime.lock.acquire(timeout=10):
        raise RuntimeError('Runtime busy; no update applied')
    try:
        if runtime.closed or fingerprint(runtime.prepare_locked) != PREPARE:
            raise RuntimeError('Unknown preparation version; no update applied')
        current = {name: fingerprint(getattr(runtime, name)) for name in BASE}
        target = {name: fingerprint(method) for name, method in methods.items()}
        if current == target:
            return {'status': 'already_applied', 'fingerprints': target}
        if current != BASE:
            raise RuntimeError('Unknown or mixed runtime methods; no update applied')
        previous = {name: runtime.__dict__.get(name) for name in methods}
        try:
            for name, method in methods.items():
                setattr(runtime, name, method)
        except BaseException:
            for name, method in previous.items():
                if method is None:
                    runtime.__dict__.pop(name, None)
                else:
                    setattr(runtime, name, method)
            raise
        return {'status': 'applied', 'fingerprints': target}
    finally:
        runtime.lock.release()
