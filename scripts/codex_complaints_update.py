"""Apply reviewed complaint fixes without replacing native connections.

The legacy notification source must match the inspected runtime bytecode.
Only named methods change. Existing calls finish without command replay.
"""
import ast
import copy
from pathlib import Path
import types

from codex_efficiency_update import fingerprint
from codex_thread_cache_update import method_node, method_code

BASE = {
    'notification': '36ade05c0707f4731bf0f7d56b93b6aaebb828453a80af7354bbd8e02dac057d',
    'chat_message': '08849a21f93e502b0e926718ec741cd2d92e8df36be23a9c88fb37f802d6ef33',
    'store_completed_broadcasts': None,
    '_monitor_exit_event': None,
    '_finish_monitor': '80ed6e9e915cda12cd8ed4fb31214799ccdad29a424d7fffaaf3760a82bc214a',
}


def notification_tree(source, current):
    tree = ast.parse(source)
    old = method_node(tree, 'notification')
    fresh = method_node(current, 'notification')
    # Reconstruct the previously installed thread-unload fix exactly.
    unload = next(n for n in ast.walk(fresh) if isinstance(n, ast.If)
                  and isinstance(n.test, ast.BoolOp)
                  and any(isinstance(v, ast.Constant) and v.value == 'thread/closed'
                          for v in ast.walk(n.test)))
    anchors = []
    for node in ast.walk(old):
        body = getattr(node, 'body', None)
        if isinstance(body, list):
            for index, child in enumerate(body):
                if isinstance(child, ast.If) and ast.unparse(child.test) == "a.get('deletedAt')":
                    anchors.append((body, index))
    if len(anchors) != 1:
        raise RuntimeError('Unknown notification source; no update applied')
    body, index = anchors[0]
    body.insert(index + 1, copy.deepcopy(unload))
    if fingerprint(types.FunctionType(method_code(tree, 'notification'), {})) != BASE['notification']:
        raise RuntimeError('Unknown notification source; no update applied')
    additions = [n for n in ast.walk(fresh) if isinstance(n, ast.If)
                 and ast.unparse(n.test) == 'not watches and (not children)'
                 and any(isinstance(v, ast.Attribute) and v.attr == 'store_completed_broadcasts'
                         for v in ast.walk(n))]
    if len(additions) != 1:
        raise RuntimeError('Unknown broadcast completion patch; no update applied')
    anchors = [n for n in ast.walk(old) if isinstance(n, ast.Expr)
               and isinstance(n.value, ast.Call) and isinstance(n.value.func, ast.Attribute)
               and n.value.func.attr == 'enforce_complaints']
    if len(anchors) != 1:
        raise RuntimeError('Unknown completion boundary; no update applied')
    for node in ast.walk(old):
        body = getattr(node, 'body', None)
        if isinstance(body, list) and anchors[0] in body:
            body.insert(body.index(anchors[0]) + 1, copy.deepcopy(additions[0]))
            break
    return tree


def replacements(runtime, legacy_notification):
    current = ast.parse(Path(__file__).with_name('codex_runtime.py').read_text())
    notification = notification_tree(legacy_notification, current)
    methods = {}
    for name in BASE:
        previous = getattr(runtime, name, None)
        node = method_node(current, name)
        scope = (getattr(previous, '__func__', previous).__globals__ if previous
                 else runtime.chat_message.__func__.__globals__).copy()
        function = types.FunctionType(method_code(notification if name == 'notification' else current, name),
                                      scope, name, tuple(ast.literal_eval(v) for v in node.args.defaults) or None)
        function.__kwdefaults__ = {arg.arg: ast.literal_eval(value) for arg, value in
                                  zip(node.args.kwonlyargs, node.args.kw_defaults) if value is not None}
        methods[name] = types.MethodType(function, runtime)
    return methods


def apply(runtime, legacy_notification):
    methods = replacements(runtime, legacy_notification)
    if not runtime.lock.acquire(timeout=10):
        raise RuntimeError('Runtime busy; no update applied')
    try:
        if runtime.closed:
            raise RuntimeError('Runtime closed; no update applied')
        current = {name: fingerprint(getattr(runtime, name, None)) for name in BASE}
        target = {name: fingerprint(method) for name, method in methods.items()}
        if current == target:
            return {'status': 'already_applied', 'methods': sorted(methods)}
        if current != BASE:
            raise RuntimeError('Unknown runtime methods; no update applied')
        # Validate every method before the first assignment. Keep all active
        # frames, queues, HTTP handlers, accounts, and native servers intact.
        for name in sorted(methods, key=lambda name: BASE[name] is not None):
            setattr(runtime, name, methods[name])
        return {'status': 'applied', 'methods': sorted(methods)}
    finally:
        runtime.lock.release()
