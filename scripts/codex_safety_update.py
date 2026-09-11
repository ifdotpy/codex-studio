"""Install reviewed safety handlers without restarting user agents or transports."""
from pathlib import Path
import types

from codex_efficiency_update import fingerprint, load_source
import codex_safety_buffering as safety

BASE = {
    'notification': '7bd01bb86e3ed1603afb25d661803bd5035ff704f6d91c332c1c721a67cbb2e7',
    'native_action': 'c7ab5edb92aff4661b736cb0feb66566855cd6e84a99ff96496f0e4dcfe48af0',
    'dispatch': '8b68d1357e8481846c7b84c78f4555f2aa3161870dbfd0c70484ac0a27fafd7b',
    'send': 'a5b3af04cb412abe0f2eaf471e8abd0722fb6ff46c4f968b49509078a14b632a',
    'assert_workspace_available': '768d6f9b7145c7c8b3e880eaf4c280709c9452fc2e264451eda257d20d2e54be',
}
ERROR_BASE = {
    'consume_native_notification': 'efcd632ac8350ef75ca8a6043d3e0218313efc0468bf416e9c25c6646897f408',
    'advance_native_status': '48a8e42c52941860f012fb1e88dfb8b41bea694d54ad89a963933ff39aa22eba',
}


def apply(runtime):
    errors = load_source('codex_native_errors')
    # Existing pipe readers and exception guards retain their original class.
    previous_scope = runtime.notification.__func__.__globals__
    errors.NativeRpcError = previous_scope['NativeRpcError']
    additions = {name: getattr(errors, name) for name in ERROR_BASE}
    additions['safety_retry_active'] = safety.active
    replacements = {}
    for filename, cls in [('codex_runtime.py', 'Runtime'), ('codex_workspace.py', 'WorkspaceMixin')]:
        source = Path(__file__).with_name(filename)
        code = compile(source.read_text(), str(source), 'exec', dont_inherit=True)
        code = next(c for c in code.co_consts if isinstance(c, types.CodeType) and c.co_name == cls)
        for c in code.co_consts:
            if not isinstance(c, types.CodeType) or c.co_name not in BASE:
                continue
            previous = getattr(runtime, c.co_name).__func__
            scope = {**previous.__globals__, **additions}
            fn = types.FunctionType(c, scope, c.co_name, previous.__defaults__)
            fn.__kwdefaults__ = previous.__kwdefaults__
            replacements[c.co_name] = types.MethodType(fn, runtime)
    if not runtime.lock.acquire(timeout=10):
        raise RuntimeError('Runtime is busy. No handlers changed.')
    try:
        if runtime.closed:
            raise RuntimeError('Runtime is closed. No handlers changed.')
        for name, fn in replacements.items():
            if fingerprint(getattr(runtime, name)) not in {BASE[name], fingerprint(fn)}:
                raise RuntimeError('Unreviewed live handler: ' + name + '. No handlers changed.')
        for name, baseline in ERROR_BASE.items():
            if fingerprint(previous_scope[name]) not in {baseline, fingerprint(additions[name])}:
                raise RuntimeError('Unreviewed native error handler: ' + name + '. No handlers changed.')
        if all(fingerprint(getattr(runtime, name)) == fingerprint(fn) for name, fn in replacements.items()) and all(
                fingerprint(previous_scope[name]) == fingerprint(additions[name]) for name in ERROR_BASE):
            return {'status': 'already_applied'}
        previous = {name: runtime.__dict__.get(name) for name in replacements}
        try:
            for name, fn in replacements.items():
                setattr(runtime, name, fn)
        except BaseException:
            for name, fn in previous.items():
                if fn is None:
                    runtime.__dict__.pop(name, None)
                else:
                    setattr(runtime, name, fn)
            raise
        return {'status': 'applied', 'methods': sorted(replacements)}
    finally:
        runtime.lock.release()
