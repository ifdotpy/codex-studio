"""Apply the reviewed active-task queries to the existing Studio process.

Only known function implementations can change. Existing bound callbacks keep
those function objects. The runtime lock protects the complete replacement.
"""
import ast
import hashlib
import json
from pathlib import Path
import sys
from types import CodeType, FunctionType, MethodType

_BASE_COMMIT = '5b90dba'
_TARGETS = (
    ('codex_runtime', 'Runtime', 'disconnected'),
    ('codex_runtime', 'Runtime', 'conversation_settings'),
    ('codex_runtime', 'Runtime', 'team'),
    ('codex_workspace', 'WorkspaceMixin', '_assert_workspace_idle'),
    ('codex_agent_management', None, '_blockers'),
)
# Canonical Python 3.14 signatures, baseline 5b90dba and the reviewed task-query patch.
_EXPECTED = {'disconnected': {'old': 'd9c33862b9c325782b1b1c7db23af1d60b53f2a38596b888eb304f675eeea9bd',
                  'new': 'cb159c3b932a00531639c621182e8e24e140c028dcd6c4a7a44592d1867cab28'},
 'conversation_settings': {'old': '0831590f9e8bcc5f0ab89c719b831fa7a9ed06f42255c0702c4b8d5805fee3e3',
                           'new': 'fe42afee750a93612e74d72c16a373d523e9df8e088fe6fe75adc6df0360fc27'},
 'team': {'old': '21f40972450b366c0aadb934ac79a7f3d692137be70a848a9f2c9a9dfaf71d76',
          'new': '16d1fcea2a2ede24f9040aae8730f7e22736a1f3a60ee448f57b2b735703037d'},
 '_assert_workspace_idle': {'old': 'dba85bf55c08ded3922ee31896473d0b6113a5f01829b81eb278d9de5ea12be9',
                            'new': '8532d6b20e1438585ccd0681f5dc86ce3f94663f999ddae7de19c535b639718b'},
 '_blockers': {'old': 'a1174d84589d819881e322d1b4f08c2cb893db3c1b6232af6e31570353bf50b1',
               'new': '0b725c447f7e3d974a46108e55ea23b55c4e7da180fd3777e2759dcc762d05ac'}}
_HELPER_SIGNATURE = '59f241d023b5eacc07a2343b6d261e5b9c4ba96f0769e4d2a4745a8cde44b07d'


def _constant(value):
    if isinstance(value, CodeType):
        return ('code', _code(value))
    if isinstance(value, tuple):
        return ('tuple', tuple(_constant(item) for item in value))
    if isinstance(value, frozenset):
        return ('frozenset', sorted((_constant(item) for item in value), key=repr))
    if isinstance(value, slice):
        return ('slice', _constant(value.start), _constant(value.stop), _constant(value.step))
    if isinstance(value, bytes):
        return ('bytes', value.hex())
    if isinstance(value, float):
        return ('float', value.hex())
    if value is None or type(value) in (bool, int, str):
        return (type(value).__name__, value)
    if value is Ellipsis:
        return ('ellipsis',)
    raise RuntimeError('Unsupported code constant; no active-task update applied')


def _code(code):
    return (code.co_code.hex(), code.co_exceptiontable.hex(), code.co_argcount,
            code.co_posonlyargcount, code.co_kwonlyargcount, code.co_nlocals,
            code.co_stacksize, code.co_flags, code.co_names, code.co_varnames,
            code.co_freevars, code.co_cellvars, _constant(code.co_consts))


def signature(function):
    if not isinstance(function, FunctionType):
        raise RuntimeError('Unknown function type; no active-task update applied')
    value = (_code(function.__code__), _constant(function.__defaults__),
             sorted((key, _constant(value)) for key, value in (function.__kwdefaults__ or {}).items()))
    return hashlib.sha256(json.dumps(value, ensure_ascii=True, separators=(',', ':')).encode()).hexdigest()


def compile_function(source, class_name, name, namespace, filename='<active-task-update>'):
    tree = ast.parse(source)
    owner = tree.body
    if class_name:
        classes = [node for node in tree.body if isinstance(node, ast.ClassDef) and node.name == class_name]
        if len(classes) != 1:
            raise RuntimeError('Unknown source class; no active-task update applied')
        owner = classes[0].body
    functions = [node for node in owner if isinstance(node, ast.FunctionDef) and node.name == name]
    if len(functions) != 1 or functions[0].decorator_list:
        raise RuntimeError('Unknown source function; no active-task update applied')
    # Compile in the original module context, but execute none of its code.
    # Python 3.14 uses module import metadata when it compiles attribute calls.
    compiled = compile(tree, filename, 'exec', dont_inherit=True)
    if class_name:
        compiled = next(code for code in compiled.co_consts
                        if isinstance(code, CodeType) and code.co_name == class_name)
    code = next(code for code in compiled.co_consts
                if isinstance(code, CodeType) and code.co_name == name)
    arguments = functions[0].args
    defaults = tuple(ast.literal_eval(value) for value in arguments.defaults) or None
    function = FunctionType(code, namespace, name, defaults)
    function.__kwdefaults__ = {
        arg.arg: ast.literal_eval(value)
        for arg, value in zip(arguments.kwonlyargs, arguments.kw_defaults) if value is not None
    } or None
    return function


def apply(runtime):
    if sys.version_info[:2] != (3, 14):
        raise RuntimeError('This update requires the reviewed Python 3.14 runtime')
    source_dir = Path(__file__).parent
    desired = {}
    modules = {}
    for module_name, class_name, name in _TARGETS:
        module = sys.modules.get(module_name)
        if module is None:
            raise RuntimeError('Expected runtime module is not loaded; no active-task update applied')
        modules[module_name] = module
        path = source_dir / (module_name + '.py')
        function = compile_function(path.read_text(), class_name, name, vars(module), str(path))
        if signature(function) != _EXPECTED[name]['new']:
            raise RuntimeError('Unreviewed replacement source; no active-task update applied')
        desired[name] = function
    workspace = modules['codex_workspace']
    helper = compile_function((source_dir / 'codex_workspace.py').read_text(), None,
                              'active_task_records', vars(workspace))
    if signature(helper) != _HELPER_SIGNATURE:
        raise RuntimeError('Unreviewed task helper; no active-task update applied')
    helper_defaults = helper.__kwdefaults__
    helper = FunctionType(helper.__code__, vars(workspace), helper.__name__, helper.__defaults__)
    helper.__kwdefaults__ = helper_defaults
    if signature(helper) != _HELPER_SIGNATURE:
        raise RuntimeError('Task helper defaults changed; no active-task update applied')
    if not runtime.lock.acquire(timeout=10):
        raise RuntimeError('Runtime remains busy; no active-task update applied')
    missing = object()
    originals = []
    namespaces = []
    try:
        for module_name, class_name, name in _TARGETS:
            if class_name:
                method = getattr(runtime, name)
                if not isinstance(method, MethodType) or method.__self__ is not runtime:
                    raise RuntimeError('Unknown live method binding; no active-task update applied')
                current = method.__func__
            else:
                current = getattr(modules[module_name], name)
            if not isinstance(current, FunctionType) or current.__globals__ is not vars(modules[module_name]):
                raise RuntimeError('Unknown live function namespace; no active-task update applied')
            if signature(current) not in _EXPECTED[name].values():
                raise RuntimeError('Unknown live ' + name + '; no active-task update applied')
            originals.append((current, current.__code__, desired[name].__code__))
        for module_name in ('codex_runtime', 'codex_workspace'):
            namespace = vars(modules[module_name])
            previous = namespace.get('active_task_records', missing)
            if previous is not missing and (signature(previous) != _HELPER_SIGNATURE
                                            or previous.__globals__ is not vars(workspace)):
                raise RuntimeError('Unknown live task helper; no active-task update applied')
            namespaces.append((namespace, previous))
        if (all(signature(current) == _EXPECTED[name]['new']
                for (current, _, _), (_, _, name) in zip(originals, _TARGETS))
                and all(previous is not missing for _, previous in namespaces)):
            return {'status': 'already_applied', 'baseCommit': _BASE_COMMIT}
        # Validation completes before any namespace or function changes.
        try:
            for namespace, _ in namespaces:
                namespace['active_task_records'] = helper
            for current, _, replacement in originals:
                current.__code__ = replacement
        except BaseException:
            for current, previous, _ in originals:
                current.__code__ = previous
            for namespace, previous in namespaces:
                if previous is missing:
                    namespace.pop('active_task_records', None)
                else:
                    namespace['active_task_records'] = previous
            raise
        return {'status': 'applied', 'baseCommit': _BASE_COMMIT,
                'methods': [name for _, _, name in _TARGETS]}
    finally:
        runtime.lock.release()
