"""Install reviewed error handlers without stopping native turns or commands."""
from pathlib import Path
import types

import codex_native_errors as errors
from codex_efficiency_update import fingerprint

# Runtime at a2a2cce, CPython 3.14. Unknown versions require another source review.
BASE = {
    'notification': '422283711421152d2f0519b192aba41448c4bb2483ffc900e9b28a07969737ff',
    'request': 'aa7ddbedf7e087c9aa46c6d5032ecd07f7148f7be37a1814cb6e572cd998006f',
    'snapshot': '3da55c274917ed484b3232d6a36430b0c8b4c732764601a5599c81ea4045b7cd',
    'enqueue': 'a077bf20e0dcbc5da1f3a71876de22de36c690a3c514e8d53bd9bc2b6531133d',
    'send': '4008e181798dec99e04358dee571fc851edc259410468239676565216e5b106e',
    'dispatch': '3ced83fd48ff8da3d9c312fbd715977eb8c217a807b6a070ffa04a93f8ec1ed0',
}


def apply(runtime):
    source = Path(__file__).with_name('codex_runtime.py')
    module = compile(source.read_text(), str(source), 'exec', dont_inherit=True)
    cls = next(c for c in module.co_consts if isinstance(c, types.CodeType) and c.co_name == 'Runtime')
    replacements = {}
    for name in BASE:
        previous = getattr(runtime, name).__func__
        scope = previous.__globals__.copy()
        for symbol in ('NativeRpcError', 'SUPPORTED_REQUESTS', 'consume_native_notification',
                       'advance_native_status', 'notice', 'error_message', 'account_notices'):
            scope[symbol] = getattr(errors, symbol)
        code = next(c for c in cls.co_consts if isinstance(c, types.CodeType) and c.co_name == name)
        function = types.FunctionType(code, scope, name, previous.__defaults__)
        function.__kwdefaults__ = previous.__kwdefaults__
        replacements[name] = types.MethodType(function, runtime)
    if not runtime.lock.acquire(timeout=10):
        raise RuntimeError('Runtime busy; no update applied')
    try:
        if runtime.closed:
            raise RuntimeError('Runtime closed; no update applied')
        for name, value in replacements.items():
            if fingerprint(getattr(runtime, name)) not in {BASE[name], fingerprint(value)}:
                raise RuntimeError('Unknown runtime method ' + name + '; no update applied')
        if all(fingerprint(getattr(runtime, name)) == fingerprint(value) for name, value in replacements.items()):
            return {'status': 'already_applied'}
        before = {name: runtime.__dict__.get(name) for name in replacements}
        try:
            for name, value in replacements.items():
                setattr(runtime, name, value)
        except BaseException:
            for name, value in before.items():
                if value is None:
                    runtime.__dict__.pop(name, None)
                else:
                    setattr(runtime, name, value)
            raise
        return {'status': 'applied', 'methods': sorted(replacements),
                'reader': 'Existing pipe readers keep their original exception class. Error payloads remain intact.'}
    finally:
        runtime.lock.release()
