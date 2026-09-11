"""Install the reviewed model-view methods without restarting active work.

Run inside the existing runtime. Exact bytecode guards reject unknown versions.
Existing Python frames finish their old method; later calls use the new method.
"""
import ast
import hashlib
import importlib.util
import json
from pathlib import Path
import types

BASE = {
    'prepare_locked': 'bce41c745f752855721db22e456d8c05301965cbba44bd84052b01a6dcf82aa9',
    'dynamic': 'b03edceae30ecc8a86cbc2cbda43f6681511c1c364ea7f3e0005da4152842644',
    'dispatch': 'c9b5e18fcef07a0fb9505fc555731412f6f31bf43d4d969941cffc587c95b977',
    'start': '222605eb84a7525d008fa14470736f52eb9ce1a6cf16b5b36698639dc1ac1dc5',
    'new_thread_params': '5c02d942dcf0f749f2782f9a648a0f4367fe408526a67ee605d5462839472e1d',
    'chat_message': '4420bd84a3a933a9f20fd551235d93adc0f04d4b09a1e2ab951d3b72b5795ca2',
    'monitor': '6b9d98692eb8e59fbe7549475dc40019cbc1f926ba7104ef117296017c68fb10',
    '_finish_monitor': 'dfa8b9f12c525efe41b6ba51cd97c7b06e6ccff1b8c2d0ed4779f8f36702012c',
    'transcript': '387e43ea53ad7d4bed471ee18d0b490d6908b96948cd0cac0505a62064204f29',
    'transcript_tool_result': None,
}


def code_data(code):
    if isinstance(code, types.CodeType):
        return [code.co_code.hex(), code.co_names, code.co_varnames, code.co_argcount,
                code.co_posonlyargcount, code.co_kwonlyargcount, code.co_freevars,
                code.co_cellvars, code.co_flags, [code_data(c) for c in code.co_consts]]
    if isinstance(code, tuple):
        return ['tuple', [code_data(c) for c in code]]
    if isinstance(code, frozenset):
        return ['frozenset', sorted(repr(c) for c in code)]
    return [type(code).__name__, repr(code)]


def fingerprint(function):
    if function is None:
        return None
    function = getattr(function, '__func__', function)
    return hashlib.sha256(json.dumps(code_data(function.__code__), sort_keys=True).encode()).hexdigest()


def load_source(name):
    path = Path(__file__).with_name(name + '.py')
    spec = importlib.util.spec_from_file_location('studio_update_' + name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def replacements(runtime):
    source = Path(__file__).with_name('codex_runtime.py')
    text = source.read_text()
    tree = ast.parse(text)
    module_code = compile(text, str(source), 'exec', dont_inherit=True)
    cls_code = next(c for c in module_code.co_consts if isinstance(c, types.CodeType) and c.co_name == 'Runtime')
    cls_node = next(n for n in tree.body if isinstance(n, ast.ClassDef) and n.name == 'Runtime')
    # Copy globals, preserving live exception types and account connections.
    # Compiling the full module preserves Python 3.14 import-aware bytecode.
    scope = runtime.dynamic.__func__.__globals__.copy()
    work, efficiency, requests = (load_source(n) for n in ('codex_work', 'codex_efficiency', 'codex_tool_requests'))
    scope.update(work_tools=work.work_tools, efficiency_tools=efficiency.efficiency_tools)
    definitions = [n for n in tree.body if
        (isinstance(n, ast.Assign) and any(isinstance(t, ast.Name) and t.id in {'TOOLS', 'INSTRUCTIONS'} for t in n.targets))
        or (isinstance(n, ast.AugAssign) and isinstance(n.target, ast.Name) and n.target.id == 'TOOLS')
        or (isinstance(n, ast.For) and isinstance(n.iter, ast.Name) and n.iter.id == 'TOOLS')]
    exec(compile(ast.Module(body=definitions, type_ignores=[]), str(source), 'exec'), scope)
    methods = {}
    names = set(BASE) - {'transcript_tool_result'} | {'tool_definitions', 'role_guidance'}
    for node in cls_node.body:
        if not isinstance(node, ast.FunctionDef) or node.name not in names:
            continue
        code = next(c for c in cls_code.co_consts if isinstance(c, types.CodeType) and c.co_name == node.name)
        if code.co_freevars:
            raise RuntimeError('A replacement requires an unsupported closure')
        function = types.FunctionType(code, scope, node.name, tuple(ast.literal_eval(v) for v in node.args.defaults) or None)
        function.__kwdefaults__ = {arg.arg: ast.literal_eval(value) for arg, value in zip(node.args.kwonlyargs, node.args.kw_defaults) if value is not None}
        static = any(isinstance(d, ast.Name) and d.id == 'staticmethod' for d in node.decorator_list)
        methods[node.name] = function if static else types.MethodType(function, runtime)
    for name, value in efficiency.EfficiencyMixin.__dict__.items():
        if isinstance(value, staticmethod):
            methods[name] = value.__func__
        elif isinstance(value, types.FunctionType):
            methods[name] = types.MethodType(value, runtime)
    methods['transcript_tool_result'] = types.MethodType(requests.RequestMixin.transcript_tool_result, runtime)
    return methods


def apply(runtime):
    methods = replacements(runtime)
    if not runtime.lock.acquire(timeout=10):
        raise RuntimeError('Runtime is busy; no update applied')
    try:
        if runtime.closed:
            raise RuntimeError('Runtime is closed; no update applied')
        current = {name: fingerprint(getattr(runtime, name, None)) for name in BASE}
        updated = {name: fingerprint(methods[name]) for name in BASE}
        if (current == updated
                and all(fingerprint(getattr(runtime, name, None)) == fingerprint(method) for name, method in methods.items())
                and runtime.tool_definitions() == methods['tool_definitions']()):
            return {'status': 'already_applied', 'methods': sorted(methods)}
        if current != BASE:
            raise RuntimeError('Unknown or mixed runtime methods; no update applied')
        # All helpers are installed before any entry point. No database mutation,
        # server replacement, queue reset, model call, or command cancellation.
        before = {name: runtime.__dict__.get(name) for name in methods}
        absent = {name for name in methods if name not in runtime.__dict__}
        try:
            for name, method in methods.items():
                if name not in BASE:
                    setattr(runtime, name, method)
            for name in BASE:
                setattr(runtime, name, methods[name])
        except BaseException:
            for name in absent:
                runtime.__dict__.pop(name, None)
            for name in before.keys() - absent:
                setattr(runtime, name, before[name])
            raise
        return {'status': 'applied', 'methods': sorted(methods),
                'inFlight': 'Existing frames finish without replay; subsequent calls use the update.'}
    finally:
        runtime.lock.release()
