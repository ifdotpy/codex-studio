"""Inspect Python source without executing its module or default expressions."""
import ast
import hashlib
import json
from types import CodeType, FunctionType


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
    raise RuntimeError('Unsupported code constant')

def _code(code):
    return (code.co_code.hex(), code.co_exceptiontable.hex(), code.co_argcount,
            code.co_posonlyargcount, code.co_kwonlyargcount, code.co_nlocals,
            code.co_stacksize, code.co_flags, code.co_names, code.co_varnames,
            code.co_freevars, code.co_cellvars, _constant(code.co_consts))

def signature(function):
    if not isinstance(function, FunctionType):
        raise RuntimeError('Unknown function type')
    value = (_code(function.__code__), _constant(function.__defaults__),
             sorted((key, _constant(value)) for key, value in (function.__kwdefaults__ or {}).items()))
    return hashlib.sha256(json.dumps(value, ensure_ascii=True, separators=(',', ':')).encode()).hexdigest()

def digest(value):
    return hashlib.sha256(json.dumps(value, ensure_ascii=True, sort_keys=True,
                                    separators=(',', ':')).encode()).hexdigest()

def source_function(source, path, namespace, filename='<source>', closure=None,
                    allow_contextmanager=False):
    """Extract code without executing imports, decorators, defaults, or classes."""
    tree = ast.parse(source)
    node = tree
    compiled = compile(tree, filename, 'exec', dont_inherit=True)
    for name in path:
        nodes = [child for child in node.body
                 if isinstance(child, (ast.ClassDef, ast.FunctionDef)) and child.name == name]
        codes = [child for child in compiled.co_consts
                 if isinstance(child, CodeType) and child.co_name == name]
        if len(nodes) != 1 or len(codes) != 1:
            raise RuntimeError('Unknown source structure')
        node, compiled = nodes[0], codes[0]
    if not isinstance(node, ast.FunctionDef):
        raise RuntimeError('Expected a source function')
    decorators = node.decorator_list
    static = len(decorators) == 1 and isinstance(decorators[0], ast.Name) and decorators[0].id == 'staticmethod'
    manager = (allow_contextmanager and len(decorators) == 1 and
               isinstance(decorators[0], ast.Name) and decorators[0].id == 'contextmanager')
    if decorators and not static and not manager:
        raise RuntimeError('Unknown source function decorator')
    defaults = tuple(ast.literal_eval(value) for value in node.args.defaults) or None
    if closure is None and compiled.co_freevars:
        # Only signature inspection uses empty cells; installed HTTP code keeps its live cells.
        closure = tuple((lambda item: lambda: item)(None).__closure__[0]
                        for _ in compiled.co_freevars)
    function = FunctionType(compiled, namespace, node.name, defaults, closure)
    function.__kwdefaults__ = {arg.arg: ast.literal_eval(value)
                              for arg, value in zip(node.args.kwonlyargs, node.args.kw_defaults)
                              if value is not None} or None
    return function, static

def source_instructions(source):
    nodes = [node for node in ast.parse(source).body if isinstance(node, ast.Assign)
             and any(isinstance(target, ast.Name) and target.id == 'INSTRUCTIONS'
                     for target in node.targets)]
    if len(nodes) != 1:
        raise RuntimeError('Unknown instructions source')
    value = ast.literal_eval(nodes[0].value)
    if type(value) is not str:
        raise RuntimeError('Unknown instructions value')
    return value
