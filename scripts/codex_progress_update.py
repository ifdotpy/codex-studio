"""Apply the reviewed progress-file cutover without restarting Studio.

Compile source in its full module context, but execute no module or function.
Existing callback functions and HTTP closure cells retain their identities.
"""
import ast
import contextlib
import hashlib
import inspect
import json
from pathlib import Path
import sys
from types import CodeType, FunctionType

from codex_active_task_update import signature

_BASE_COMMIT = '0c80d8e'
_TARGETS = (
    ('codex_runtime', ('Runtime', 'turn_permissions')),
    ('codex_runtime', ('Runtime', 'progress_file')),
    ('codex_runtime', ('Runtime', 'new_thread_params')),
    ('codex_runtime', ('Runtime', 'dynamic')),
    ('codex_efficiency', ('EfficiencyMixin', 'model_context')),
    ('codex_efficiency', ('EfficiencyMixin', 'model_turn_context')),
    ('codex_panel', ('PanelMixin', 'get_panel')),
    ('codex_panel', ('panel_tools',)),
    ('codex_canvas', ('make_server', 'Handler', 'do_GET')),
)
_NEW_METHODS = {'progress_file', 'get_panel'}
_RETIRED_TOOLS = {'orchestration_panel', 'orchestration_panel_feed'}
_PROGRESS_SHA256 = '69e8d3c264a0e41887b76d56d254921b3e05cf82d1aef614658acdfcfd3471f3'
_EXPECTED = {'turn_permissions': {'old': 'c8b6d19daeae71ecab7bd80f14fd9d8a22076772159653e8bfd30af189559cda',
                      'new': '32cd55c245cfa739202faa637064df44f87d678211a7983841e0a5fa9b50248e'},
 'progress_file': {'old': None, 'new': 'a18a6126cf4438f462aa8677747312070e8555108a7559e8eb79a89c4ef26693'},
 'new_thread_params': {'old': '7acb95273c124724700bb7b6b40db6a0ecc353304f76d8990aad297e898d7c29',
                       'new': '657c5c181919e3cfeacb2667169d5019a2417092d0691cf12ffc4a2842a53ef3'},
 'dynamic': {'old': '0a7af805ebeceeb5680884a877f3c65d0fbdc6a048c4013d0e993d28152506e5',
             'new': '617f63299f75e37e282c1495d7cfbf11508d440abea748e852b27d937ec34e0c'},
 'model_context': {'old': 'daa0880c59f8dc3ee0f3e43fbc8b17aa08015327baad5284fb868d831c81f858',
                   'new': '2857e1eb092ff24d3ab2b7f57eb4e07618eaddd92a23a38250986e6972c9385f'},
 'model_turn_context': {'old': '416de31b0a6b8f83eca9618481985ce1f29be8ca2b4b4544c84fd76d27e23610',
                        'new': '51781d0f57f57541123efb55c26b3c0f81d85292ef2d144455a6a9bde9c5ecbb'},
 'get_panel': {'old': None, 'new': '827daf0db0690dd83110812cbba6e793ece1cbe852e867a81122bb35d7a3b7be'},
 'panel_tools': {'old': 'ca1f1fc5e625a4e54ce9b797c9515e587eaf382801c999a1af9641fdb32c5a88',
                 'new': 'e54e1b5cee135807415694aac5fdf61bf498e6d126167259acf76cc3def6be74'},
 'do_GET': {'old': '7ba8af0a97f318ce76423143ac75b7dc60f621401dc9868a7d2ab0444df6661e',
            'new': 'efcb921bfccd63c4bdfee7b71c77395a2d5b46adace2959fa4cf61f25b0351ce'}}
_INSTRUCTIONS = {'old': 'f81bf02ce2c0256dfabdabad2771953fd9497e9ff6f7e4756dc07238000a7667', 'new': 'b88c6bcfa95abbccd2691de695afd81564735d861bee62d2cb12878facb80097'}
_TOOLS = {'old': 'fd6c6cf0a8c309c3ddd05f3f7646e53d6a796e0c9c4b4b84548eba41bac9c0e8', 'new': 'e2d99901f1817b946ef24238fa1fc31c464b60a9888094aeea4bf97f9df4a75b'}


def digest(value):
    return hashlib.sha256(json.dumps(value, ensure_ascii=True, sort_keys=True,
                                    separators=(',', ':')).encode()).hexdigest()


def source_function(source, path, namespace, filename='<progress-update>', closure=None,
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
            raise RuntimeError('Unknown progress source structure')
        node, compiled = nodes[0], codes[0]
    if not isinstance(node, ast.FunctionDef):
        raise RuntimeError('Expected a progress source function')
    decorators = node.decorator_list
    static = len(decorators) == 1 and isinstance(decorators[0], ast.Name) and decorators[0].id == 'staticmethod'
    manager = (allow_contextmanager and len(decorators) == 1 and
               isinstance(decorators[0], ast.Name) and decorators[0].id == 'contextmanager')
    if decorators and not static and not manager:
        raise RuntimeError('Unknown progress function decorator')
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
        raise RuntimeError('Unknown progress instructions source')
    value = ast.literal_eval(nodes[0].value)
    if type(value) is not str:
        raise RuntimeError('Unknown progress instructions value')
    return value


def _function(value, namespace, label):
    if not isinstance(value, FunctionType) or value.__globals__ is not namespace:
        raise RuntimeError('Unknown live progress function namespace: ' + label)
    return value


def _validate_progress_source(source_dir):
    path = source_dir / 'codex_progress.py'
    raw = path.read_bytes()
    if hashlib.sha256(raw).hexdigest() != _PROGRESS_SHA256:
        raise RuntimeError('Unreviewed progress file helper source')
    module = sys.modules.get('codex_progress')
    if module is None:
        return
    if Path(getattr(module, '__file__', '')).resolve() != path.resolve():
        raise RuntimeError('Unknown loaded progress file helper origin')
    namespace = vars(module)
    tree = ast.parse(raw)
    for node in tree.body:
        if not isinstance(node, ast.FunctionDef):
            continue
        function = namespace.get(node.name)
        if node.decorator_list:
            original = getattr(function, '__wrapped__', None)
            _function(original, namespace, node.name)
            wrapper = contextlib.contextmanager(original)
            if (not isinstance(function, FunctionType) or function.__globals__ is not vars(contextlib)
                    or signature(function) != signature(wrapper)
                    or len(function.__closure__ or ()) != 1
                    or function.__closure__[0].cell_contents is not original):
                raise RuntimeError('Unknown loaded progress context manager')
            function = original
        _function(function, namespace, node.name)
        expected, _ = source_function(raw, (node.name,), namespace,
                                      allow_contextmanager=True)
        if signature(function) != signature(expected):
            raise RuntimeError('Unknown loaded progress file helper: ' + node.name)
    pattern = namespace.get('_AGENT_ID')
    if (namespace.get('MAX_PROGRESS_BYTES') != 128 * 1024
            or getattr(pattern, 'pattern', None) != r'[A-Za-z0-9_-]{1,200}\Z'
            or getattr(pattern, 'flags', None) != 32):
        raise RuntimeError('Unknown loaded progress file helper limits')


def _http_closure(function, runtime, handler_class):
    cells = dict(zip(function.__code__.co_freevars, function.__closure__ or ()))
    if set(cells) != {'canvas', 'cost_reader', 'remote', 'snapshot', 'sync',
                      'terminal_lock', 'terminals', 'token'}:
        raise RuntimeError('Unknown HTTP progress closure')
    try:
        canvas = cells['canvas'].cell_contents
        if canvas.runtime is not runtime:
            raise RuntimeError('HTTP progress handler belongs to another runtime')
        # Sibling handlers from the same make_server invocation share these exact cells.
        siblings = ('do_POST', 'trusted', 'stream_sync')
        observed = set()
        for name in siblings:
            sibling = inspect.getattr_static(handler_class, name)
            _function(sibling, function.__globals__, 'HTTP ' + name)
            for key, cell in zip(sibling.__code__.co_freevars, sibling.__closure__ or ()):
                if key in cells:
                    if cell is not cells[key]:
                        raise RuntimeError('HTTP progress closure cells do not match')
                    observed.add(key)
        for name in ('snapshot', 'sync', 'terminals'):
            nested = _function(cells[name].cell_contents, function.__globals__, name)
            if nested.__qualname__ != 'make_server.<locals>.' + name:
                raise RuntimeError('Unknown HTTP progress closure function')
            for key, cell in zip(nested.__code__.co_freevars, nested.__closure__ or ()):
                if key in cells and cell is not cells[key]:
                    raise RuntimeError('HTTP progress helper closure does not match')
                if key in cells:
                    observed.add(key)
        if not {'canvas', 'remote', 'token', 'terminal_lock', 'terminals', 'sync'} <= observed:
            raise RuntimeError('HTTP progress closure provenance is incomplete')
    except (AttributeError, ValueError) as error:
        raise RuntimeError('Unknown HTTP progress closure state') from error
    return function.__closure__


def apply(runtime, handler_class):
    if sys.version_info[:2] != (3, 14):
        raise RuntimeError('Progress update requires the reviewed Python 3.14 runtime')
    source_dir = Path(__file__).parent
    modules, desired = {}, {}
    for module_name, path in _TARGETS:
        module = sys.modules.get(module_name)
        if module is None:
            raise RuntimeError('Expected progress runtime module is not loaded')
        modules[module_name] = module
        source = (source_dir / (module_name + '.py')).read_text()
        replacement, static = source_function(source, path, vars(module))
        name = path[-1]
        if static or signature(replacement) != _EXPECTED[name]['new']:
            raise RuntimeError('Unreviewed progress replacement: ' + name)
        desired[name] = replacement
    runtime_module = modules['codex_runtime']
    instructions = source_instructions((source_dir / 'codex_runtime.py').read_text())
    if digest(instructions) != _INSTRUCTIONS['new']:
        raise RuntimeError('Unreviewed progress instructions')
    if not runtime.lock.acquire(timeout=10):
        raise RuntimeError('Runtime remains busy; no progress update applied')
    missing = object()
    changes, codes = [], []
    try:
        if type(handler_class) is not type or handler_class.__module__ != 'codex_canvas' or handler_class.__qualname__ != 'make_server.<locals>.Handler':
            raise RuntimeError('Unknown HTTP progress handler class')
        if runtime.closed:
            raise RuntimeError('Runtime is closed; no progress update applied')
        _validate_progress_source(source_dir)
        for module_name, path in _TARGETS:
            namespace = vars(modules[module_name])
            name = path[-1]
            owner = (handler_class if name == 'do_GET' else
                     getattr(modules[module_name], path[0]) if len(path) > 1 else modules[module_name])
            current = vars(owner).get(name, missing)
            if name != 'do_GET' and len(path) > 1:
                if not isinstance(runtime, owner) or inspect.getattr_static(runtime, name, missing) is not current:
                    raise RuntimeError('Unknown live progress method binding: ' + name)
            if current is missing:
                if name not in _NEW_METHODS:
                    raise RuntimeError('Missing live progress method: ' + name)
                changes.append((owner, name, missing, desired[name]))
                continue
            static = isinstance(current, staticmethod)
            function = _function(current.__func__ if static else current, namespace, name)
            live_signature = signature(function)
            if live_signature not in _EXPECTED[name].values():
                raise RuntimeError('Unknown live progress implementation: ' + name)
            if static != (name == 'turn_permissions' and live_signature == _EXPECTED[name]['old']):
                raise RuntimeError('Unknown live progress descriptor: ' + name)
            replacement = desired[name]
            if name == 'do_GET':
                _http_closure(function, runtime, handler_class)
            if function.__code__.co_freevars != replacement.__code__.co_freevars:
                raise RuntimeError('Progress closure names changed: ' + name)
            if static:
                # This method has inline callers only; a static descriptor cannot bind self.
                changes.append((owner, name, current, replacement))
            elif live_signature != _EXPECTED[name]['new']:
                if function.__defaults__ != replacement.__defaults__ or function.__kwdefaults__ != replacement.__kwdefaults__:
                    raise RuntimeError('Progress callback defaults changed: ' + name)
                codes.append((function, function.__code__, replacement.__code__))
        panel_function = vars(modules['codex_panel'])['panel_tools']
        if runtime_module.panel_tools is not panel_function:
            raise RuntimeError('Unknown imported panel tools function')
        old_instructions = runtime_module.INSTRUCTIONS
        tools = runtime_module.TOOLS
        if type(old_instructions) is not str or digest(old_instructions) not in _INSTRUCTIONS.values():
            raise RuntimeError('Unknown live progress instructions')
        if type(tools) is not list or digest(tools) not in _TOOLS.values():
            raise RuntimeError('Unknown live progress tool definitions')
        new_tools = [item for item in tools if item.get('name') not in _RETIRED_TOOLS]
        if digest(new_tools) != _TOOLS['new']:
            raise RuntimeError('Progress tools would change beyond the retired pair')
        if not changes and not codes and old_instructions == instructions and len(new_tools) == len(tools):
            return {'status': 'already_applied', 'baseCommit': _BASE_COMMIT}
        old_tools = tools[:]
        try:
            for owner, name, _, replacement in changes:
                setattr(owner, name, replacement)
            runtime_module.INSTRUCTIONS = instructions
            tools[:] = new_tools
            for function, _, replacement in codes:
                function.__code__ = replacement
        except BaseException:
            for function, previous, _ in codes:
                function.__code__ = previous
            tools[:] = old_tools
            runtime_module.INSTRUCTIONS = old_instructions
            for owner, name, previous, _ in reversed(changes):
                if previous is missing:
                    if name in vars(owner):
                        delattr(owner, name)
                else:
                    setattr(owner, name, previous)
            raise
        return {'status': 'applied', 'baseCommit': _BASE_COMMIT,
                'methods': [path[-1] for _, path in _TARGETS],
                'retiredTools': sorted(_RETIRED_TOOLS)}
    finally:
        runtime.lock.release()
