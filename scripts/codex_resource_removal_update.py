"""Retire resource reservations without restarting Studio or changing agent state."""
import gc
from http.server import ThreadingHTTPServer
from pathlib import Path
import socket
import sys
from types import FunctionType, MethodType

from codex_active_task_update import signature
from codex_progress_update import source_function, source_instructions, digest, _http_closure

BASE_COMMIT = '1043d8f683bbed59170a64119e6ac51dd6c182e8'
EXPECTED = {'codex_runtime.Runtime.dynamic': ('617f63299f75e37e282c1495d7cfbf11508d440abea748e852b27d937ec34e0c',
                                   'a343c4822dc25a246fe4ad0c2bffb287b3941b11b84cd18361367a558968e3b1'),
 'codex_rules.rule_tools': ('b547e0c88f14d5826ad019303c38bc1a50f97ab45aa95cbd51ff26b3c6ab2893',
                            'b88e9fd2f1242f089ea9327361929d0aee7f4dc9941547edf773a7b0a64a069d'),
 'codex_canvas.Canvas.snapshot': ('cbfe32ce03d99f62f1682045ad388b9c3a3adc682cf711b781c54a138175956d',
                                  'd5aeba8fd245bf2818fc9da4e99e1ce4b3f38202eeaa7a983c97de4cd47a60ab'),
 'codex_canvas.Canvas.post': ('149607b0c09427105206b061c048f5cf21be694c17c0d692ac188dd13d4bd019',
                              '79c593293f496a1019111051f928a02dddb8aebe3998f083bb9373cf25879a8d'),
 'codex_canvas.make_server.Handler.do_GET': ('de37295ae35715b5fbdd7eba8a1b17e7d2c4283bb8dd59c1fcb8c4ac5cd63ef9',
                                             '0967520427aa11defca65644d9f19d30ef5f87669a8f7baad04e88b65036b86f'),
 'codex_canvas.make_server.Handler.do_POST': ('495849636d0d7042ea094f0428610b65462e076bb72f30e894ae7cc394e7177d',
                                              'b1ca1845e6a542e241321f2910da4f3edc388711c4268ebed326f7e8a113a7db'),
 'codex_agent_management._blockers': ('0b725c447f7e3d974a46108e55ea23b55c4e7da180fd3777e2759dcc762d05ac',
                                      'c122c8c6e53926d79fa4a090fb903e1f6f99ad3994181272e359fc9dd9d5d26b'),
 'codex_tool_requests.request_result_outcome': ('43d0e38988434c238d1d464e005451d4ed0c3f76ff487e3b64010b8d36bc9979',
                                                'd31f0064d50c17cfabb2f67ffb36af2e959c44b71f7c5861c47a634a2f93da54')}
INSTRUCTIONS = (
    'b88c6bcfa95abbccd2691de695afd81564735d861bee62d2cb12878facb80097',
    'ebb35d368395c55366d82c17f193ad3fa8795e3221ea919a1362fa4895c95530',
)
TOOLS = (
    'e2d99901f1817b946ef24238fa1fc31c464b60a9888094aeea4bf97f9df4a75b',
    '7037067a4a5b050878f37f8047ca86de4cb30be04b28e01e7288fd83b91e6736',
)
RETIRED = {
    'resource_action': '34dc963afbbfd4b4760d1807d310683df9dc64d10c5626c31e7d8cd45bea5d71',
    'resource_locked': '6adecbed137f8cac84fdccfb4883da4980a4e8a3b0cd800f39ca016e128761b2',
}


def resource_action(self, data=None, actor=None, epoch=None):
    return {'ok': False, 'outcome': 'not_applied',
            'message': 'Resource reservations were removed. Continue without a board claim.'}


def resource_locked(self, data=None, actor=None):
    return {'ok': False, 'outcome': 'not_applied',
            'message': 'Resource reservations were removed. Continue without a board claim.'}


def _bound(owner, name, instance):
    live = vars(owner).get(name)
    bound = getattr(instance, name, None)
    prefix = type(instance).__mro__[:type(instance).__mro__.index(owner)]
    if (name in vars(instance) or any(name in vars(cls) for cls in prefix)
            or not isinstance(bound, MethodType) or bound.__self__ is not instance
            or bound.__func__ is not live):
        raise RuntimeError('Unexpected reservation method override: ' + name)
    return live


def _find_handler(runtime):
    matches = []
    for candidate in gc.get_objects():
        owner = type(candidate)
        if (owner.__module__ != 'codex_canvas'
                or owner.__qualname__ != 'make_server.<locals>.LocalServer'
                or not isinstance(candidate, ThreadingHTTPServer)):
            continue
        handler = vars(candidate).get('RequestHandlerClass')
        listener = vars(candidate).get('socket')
        if (type(handler) is not type or handler.__module__ != 'codex_canvas'
                or handler.__qualname__ != 'make_server.<locals>.Handler'
                or not isinstance(listener, socket.socket) or listener.fileno() < 0):
            continue
        function = vars(handler).get('do_GET')
        if not isinstance(function, FunctionType):
            continue
        cells = dict(zip(function.__code__.co_freevars, function.__closure__ or ()))
        try:
            canvas = cells['canvas'].cell_contents
            if vars(canvas).get('runtime') is runtime:
                matches.append(handler)
        except (KeyError, ValueError, TypeError):
            continue
    if len(matches) != 1:
        raise RuntimeError('Expected one live HTTP server for reservation removal')
    return matches[0]


def apply(runtime, handler_class=None):
    import codex_runtime
    if sys.version_info[:2] != (3, 14) or type(runtime) is not codex_runtime.Runtime:
        raise RuntimeError('Unknown runtime for the reservation removal')
    if handler_class is None:
        handler_class = _find_handler(runtime)
    if (type(handler_class) is not type or handler_class.__module__ != 'codex_canvas'
            or handler_class.__qualname__ != 'make_server.<locals>.Handler'):
        raise RuntimeError('Unknown reservation HTTP handler')
    directory = Path(__file__).resolve().parent
    modules = {}
    sources = {}
    for module_name in {key.split('.')[0] for key in EXPECTED} | {'codex_rules'}:
        module = sys.modules.get(module_name)
        path = directory / (module_name + '.py')
        if module is None or Path(module.__file__).resolve() != path:
            raise RuntimeError('Unexpected reservation module location: ' + module_name)
        modules[module_name] = module
        sources[module_name] = path.read_text()
    replacements = []
    for target, allowed in EXPECTED.items():
        module_name, *path = target.split('.')
        desired, static = source_function(sources[module_name], tuple(path), vars(modules[module_name]))
        if static or signature(desired) != allowed[1]:
            raise RuntimeError('Unreviewed reservation replacement: ' + target)
        replacements.append((target, path, modules[module_name], desired, allowed))
    instructions = source_instructions(sources['codex_runtime'])
    if digest(instructions) != INSTRUCTIONS[1]:
        raise RuntimeError('Unreviewed reservation instructions')
    if not runtime.lock.acquire(timeout=10):
        raise RuntimeError('Runtime remains busy; no reservation update applied')
    originals = []
    try:
        if runtime.closed:
            raise RuntimeError('Runtime is closed; no reservation update applied')
        _http_closure(handler_class.do_GET, runtime, handler_class)
        cells = dict(zip(handler_class.do_GET.__code__.co_freevars, handler_class.do_GET.__closure__))
        canvas = cells['canvas'].cell_contents
        if type(canvas) is not modules['codex_canvas'].Canvas:
            raise RuntimeError('Unknown reservation canvas')
        for target, path, module, desired, allowed in replacements:
            if len(path) == 1:
                live = vars(module).get(path[0])
            elif path[0] == 'make_server':
                live = vars(handler_class).get(path[-1])
            else:
                owner = getattr(module, path[0])
                if owner.__module__ != module.__name__ or owner.__name__ != path[0]:
                    raise RuntimeError('Unknown reservation class: ' + target)
                live = _bound(owner, path[-1], canvas if path[0] == 'Canvas' else runtime)
            if (not isinstance(live, FunctionType) or live.__globals__ is not vars(module)
                    or live.__module__ != module.__name__ or signature(live) not in allowed
                    or live.__code__.co_freevars != desired.__code__.co_freevars):
                raise RuntimeError('Unknown live reservation function: ' + target)
            originals.append((live, live.__code__, desired.__code__))
        rules = modules['codex_rules']
        if codex_runtime.rule_tools is not rules.rule_tools:
            raise RuntimeError('Unknown reservation tool factory alias')
        owner = rules.RulesMixin
        if (codex_runtime.RulesMixin is not owner or owner not in type(runtime).__mro__
                or owner.__module__ != 'codex_rules' or owner.__name__ != 'RulesMixin'):
            raise RuntimeError('Unknown reservation mixin')
        # Old dispatch frames and captured callbacks must receive a definite failure.
        # The lock lets an already executing board command finish before this cutover.
        # Fresh processes have neither method. Only old processes retain these stubs.
        for name, allowed in RETIRED.items():
            if name not in vars(owner):
                if hasattr(runtime, name):
                    raise RuntimeError('Unexpected retired reservation override: ' + name)
                continue
            live = _bound(owner, name, runtime)
            desired = globals()[name]
            if (not isinstance(live, FunctionType) or live.__globals__ is not vars(rules)
                    or live.__module__ != 'codex_rules' or live.__code__.co_freevars
                    or signature(live) not in {allowed, signature(desired)}):
                raise RuntimeError('Unknown retired reservation function: ' + name)
            originals.append((live, live.__code__, desired.__code__))
        previous_instructions = codex_runtime.INSTRUCTIONS
        tools = codex_runtime.TOOLS
        if type(previous_instructions) is not str or digest(previous_instructions) not in INSTRUCTIONS:
            raise RuntimeError('Unknown live reservation instructions')
        if type(tools) is not list or digest(tools) not in TOOLS:
            raise RuntimeError('Unknown live reservation tool definitions')
        desired_tools = [item for item in tools if item.get('name') != 'orchestration_resource']
        if digest(desired_tools) != TOOLS[1]:
            raise RuntimeError('Unreviewed reservation tool removal')
        unchanged = (all(live.__code__ == new for live, _, new in originals)
                     and previous_instructions == instructions and tools == desired_tools)
        if unchanged:
            return {'status': 'already_applied', 'baseCommit': BASE_COMMIT}
        previous_tools = list(tools)
        # Every source, live function, alias and value is checked before mutation.
        try:
            for live, _, desired in originals:
                live.__code__ = desired
            tools[:] = desired_tools
            codex_runtime.INSTRUCTIONS = instructions
        except BaseException:
            for live, old, _ in originals:
                if live.__code__ is not old:
                    live.__code__ = old
            tools[:] = previous_tools
            codex_runtime.INSTRUCTIONS = previous_instructions
            raise
        return {'status': 'applied', 'baseCommit': BASE_COMMIT,
                'methods': list(EXPECTED), 'retiredTools': ['orchestration_resource']}
    finally:
        runtime.lock.release()
