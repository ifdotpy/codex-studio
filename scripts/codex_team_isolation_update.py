"""Apply team communication boundaries without restarting or replaying work."""
import ast
import copy
import hashlib
import importlib.util
from pathlib import Path
import sys
from types import FunctionType, MethodType, ModuleType

from codex_active_task_update import signature
from codex_progress_update import source_function, source_instructions, digest, _http_closure
from codex_resource_removal_update import _find_handler, _canvas_runtime

BASE_COMMIT = '1156784'
EXPECTED = {'codex_canvas.Canvas.connect_chat': ('361d8382b8b11f21957b1ddb3f8679f21b0619c9315e4ffae9eb8e405e208af9',
                                      'e875aff15e452eef0a52a4fcb6146cdbd4ae34e0754f280da031fea366004589'),
 'codex_canvas.Canvas.create_chat': ('68276ae3c0f41142a6377d4f7690669abc73fa35300a4764eeeca28fd88f0375',
                                     '4d5c64638d9bdaeb718f29cbaae4f11d53b8cdd48eda0fcd0850a629e4130df4'),
 'codex_canvas.Canvas.post': ('79c593293f496a1019111051f928a02dddb8aebe3998f083bb9373cf25879a8d',
                              '3c4c6a3342e5d95e6e6bae5274eb3effa913dcce253cf42bb4a1e2d0ece05fcd'),
 'codex_chat_reviews.review_schedule': ('b6f5b6977f5e43627325af55f61a386606e5c162b05b2b4b2bc3fd6fb39f52cc',
                                        '4c44491c0fae5f08d608f785250cb5c966cf317552317c646740cb2b53cde671'),
 'codex_chat_reviews.review_tick': ('8161b71027336043b49f38032c32ac34db396d5e030c0ef2fe5334f401162237',
                                    'e2094cbd0396ad019db3c298072109fff2708602dd3616020647172a53614124'),
 'codex_efficiency.EfficiencyMixin.model_directory': ('850792484a5680baaf626d8763dd6b8a7d0175ecc79b74e74cb8227c323d6f55',
                                                      'c331c087dbb76f0172b7b917aba4b0f7dee65907eb966f6e15531207137690e3'),
 'codex_runtime.Runtime.chat_message': ('d10d140b068a4bda2c4735eccbff03acd3c2113c0becf1bbdbb600d0857c5fc3',
                                        'd9b157c0d6c051a82b9ed74dd8919393097df1657ecdf9f3b4ccec1ef105df9d'),
 'codex_runtime.Runtime.chat_rooms': ('2985543f141edbb15702dfb01b2ac9d7651aa4e6607b88c9c25d47d66bff4953',
                                      '58359b4cd138fbcd4500277d96ea4115cd24e3e21a352b42fbf5c201ba7db450'),
 'codex_runtime.Runtime.dispatch': ('73a8e8cb68b310553f764ffdcdf283edadcbb850de964b90ac87ef1c8c6526c2',
                                    'd7bb2dfa7d7597681ca39a591b4dd02120db63dabb4ab674f04a3834be798347'),
 'codex_runtime.Runtime.peers': ('eb6fda807a910305cbd186997b43a529572db0c2bacc8d7443a9d64bcb695a05',
                                 '8827bbe90605d20d30ef79a68008e66563d504ff86c1c6a863ff9ec03d392db3'),
 'codex_runtime.Runtime.send': ('a6436bf19a3ef9007e4ed02397d6f5cc34fc62aa3f49f1b3e7a8b8bf63b56096',
                                '1f54db3e0594af33f273df6a235cf02bb6ae12a187f83d819d9079d926f7d1da'),
 'codex_runtime.Runtime.start': ('7f4fae6c692137efaee1bfa5ec93dbe832cea00526c235ebcaf84ab426671552',
                                 '362ec61e1d70ea3d127f59627c7d7111bc6e4e26b860b7ea1d3dc023e4fbd72c'),
 'codex_work.WorkMixin.search_work': ('c3223ec4a77e6060b91b26596a1da4e26ec77c7518c484f5ee897121061ee096',
                                      '3aa77d86a18686b92ca9c9e82872e2d09371c25b9369115cbf29c52302d1ebd7')}
NEW_METHODS = {'codex_canvas.Canvas._check_chat_team': (None,
                                          '7e0b2c92eb3db528e1ae3c8eb03b609b92c1000b0c308838dfdcf2b6272917cc'),
 'codex_canvas.Canvas._team_id': (None, '1888f85fcacf40b08487507351f1e4602747f3b95cc4034f76b2ecd79197cb35'),
 'codex_canvas.Canvas.agent_chats': (None, '8e0ab7ff1cabae668b3d58a3cb594078653c4f66a8ded62323d81371d2802eb3'),
 'codex_canvas.Canvas.agent_messages': (None, '424f751556a9922cf8de7921dca30b19a5e7a4b213fd419beed1fd9381437239')}
INSTRUCTIONS = ('ebb35d368395c55366d82c17f193ad3fa8795e3221ea919a1362fa4895c95530',
 'eb63c2ee7e9e7b0e3af0a6c0ff36018f28a45cb657218d02e65a0663a2b6bcf5')
TOOLS = ('7037067a4a5b050878f37f8047ca86de4cb30be04b28e01e7288fd83b91e6736',
 '7abaeeff65bd2ceb4c5b0b786b8ce2bd47267e7e4242cdbc64113d488371277a')
HELPER_SHA = '411f71c5c1916c66f7aea52561d5c3fc417024a2b6b65b1fd667b69f950a04ae'
HELPER_NAME = 'codex_team_isolation'


def _bound(owner, name, instance):
    live = vars(owner).get(name)
    bound = getattr(instance, name, None)
    if owner not in type(instance).__mro__:
        raise RuntimeError('Unknown team isolation class: ' + name)
    prefix = type(instance).__mro__[:type(instance).__mro__.index(owner)]
    if (name in vars(instance) or any(name in vars(cls) for cls in prefix)
            or not isinstance(bound, MethodType) or bound.__self__ is not instance
            or bound.__func__ is not live):
        raise RuntimeError('Unexpected team isolation method override: ' + name)
    return live


def _tools(source, current):
    assignment = next(node for node in ast.parse(source).body if isinstance(node, ast.Assign)
                      and any(isinstance(t, ast.Name) and t.id == 'TOOLS' for t in node.targets))
    definitions = {}
    for node in assignment.value.elts:
        if not isinstance(node, ast.Call) or not isinstance(node.func, ast.Name) or node.func.id != 'tool' or node.keywords:
            raise RuntimeError('Unknown team tool source')
        # TEXT references occur inside nested dictionaries, so use a literal-only replacement.
        class TextLiteral(ast.NodeTransformer):
            def visit_Name(self, node):
                if node.id != 'TEXT':
                    raise RuntimeError('Unknown team tool constant')
                return ast.Dict(keys=[ast.Constant('type')], values=[ast.Constant('string')])
        values = [ast.literal_eval(TextLiteral().visit(copy.deepcopy(arg))) for arg in node.args]
        name, description, properties, *required = values
        definitions[name] = {'type': 'function', 'name': name, 'description': description,
            'inputSchema': {'type': 'object', 'properties': properties,
                            'required': required[0] if required else [], 'additionalProperties': False}}
    changed = {'orchestration_peers', 'orchestration_message', 'orchestration_chat_read'}
    return [definitions[item['name']] if item['name'] in changed else copy.deepcopy(item) for item in current]


def _helper(directory):
    path = directory / (HELPER_NAME + '.py')
    raw = path.read_bytes()
    if hashlib.sha256(raw).hexdigest() != HELPER_SHA:
        raise RuntimeError('Unreviewed team isolation helper source')
    spec = importlib.util.spec_from_file_location(HELPER_NAME, path)
    reviewed = importlib.util.module_from_spec(spec)
    exec(compile(raw, str(path), 'exec', dont_inherit=True), vars(reviewed))
    live = sys.modules.get(HELPER_NAME)
    if live is None:
        return reviewed
    if (type(live) is not ModuleType or live.__name__ != HELPER_NAME
            or Path(getattr(live, '__file__', '')).resolve() != path):
        raise RuntimeError('Unknown team isolation helper module')
    metadata = {'__name__', '__doc__', '__package__', '__loader__', '__spec__', '__file__', '__cached__'}
    actual = {k: v for k, v in vars(live).items() if k not in metadata}
    expected = {k: v for k, v in vars(reviewed).items() if k not in metadata}
    if actual.keys() != expected.keys():
        raise RuntimeError('Unknown team isolation helper globals')
    for name, desired in expected.items():
        value = actual[name]
        if isinstance(desired, FunctionType) and desired.__module__ == HELPER_NAME:
            if (not isinstance(value, FunctionType) or value.__module__ != HELPER_NAME
                    or value.__globals__ is not vars(live) or value.__closure__ is not None
                    or signature(value) != signature(desired)):
                raise RuntimeError('Unknown team isolation helper function: ' + name)
        elif type(desired) in (str, int, float, bool, type(None), tuple, frozenset):
            if type(value) is not type(desired) or value != desired:
                raise RuntimeError('Unknown team isolation helper constant: ' + name)
        elif value is not desired:
            raise RuntimeError('Unknown team isolation helper dependency: ' + name)
    return live


def _active_frames(originals):
    # Check after replacement: a new invocation can no longer enter the old code.
    # Reject rather than let a pre-cutover authorization frame cross the boundary.
    previous = {id(code): label for label, _, code, desired in originals if code != desired}
    for frame in sys._current_frames().values():
        while frame is not None:
            label = previous.get(id(frame.f_code))
            if label is not None:
                raise RuntimeError('Team isolation waits for an earlier call: ' + label)
            frame = frame.f_back


def apply(runtime, handler_class=None):
    import codex_runtime
    if sys.version_info[:2] != (3, 14) or type(runtime) is not codex_runtime.Runtime:
        raise RuntimeError('Unknown runtime for team isolation')
    handler_class = handler_class or _find_handler(runtime)
    if (type(handler_class) is not type or handler_class.__module__ != 'codex_canvas'
            or handler_class.__qualname__ != 'make_server.<locals>.Handler'):
        raise RuntimeError('Unknown team isolation HTTP handler')
    _http_closure(handler_class.do_GET, runtime, handler_class)
    cells = dict(zip(handler_class.do_GET.__code__.co_freevars, handler_class.do_GET.__closure__ or ()))
    canvas = cells['canvas'].cell_contents
    if _canvas_runtime(canvas) is not runtime:
        raise RuntimeError('Team isolation canvas belongs to another runtime')
    directory = Path(__file__).resolve().parent
    modules, sources, replacements = {}, {}, []
    for module_name in {key.split('.')[0] for key in EXPECTED | NEW_METHODS}:
        module = sys.modules.get(module_name)
        path = directory / (module_name + '.py')
        if module is None or Path(module.__file__).resolve() != path:
            raise RuntimeError('Unexpected team isolation module location: ' + module_name)
        modules[module_name], sources[module_name] = module, path.read_text()
    for target, allowed in (EXPECTED | NEW_METHODS).items():
        module_name, *path = target.split('.')
        module = modules[module_name]
        desired, static = source_function(sources[module_name], tuple(path), vars(module))
        if static or signature(desired) != allowed[1]:
            raise RuntimeError('Unreviewed team isolation replacement: ' + target)
        replacements.append((target, path, module, desired, allowed))
    helper = _helper(directory)
    instructions = source_instructions(sources['codex_runtime'])
    if digest(instructions) != INSTRUCTIONS[1]:
        raise RuntimeError('Unreviewed team isolation instructions')
    # Canvas mutations enter the Canvas lock before they call Runtime methods.
    if not canvas.lock.acquire(timeout=10):
        raise RuntimeError('Canvas remains busy; no team isolation update applied')
    try:
        if not runtime.lock.acquire(timeout=10):
            raise RuntimeError('Runtime remains busy; no team isolation update applied')
        try:
            if runtime.closed:
                raise RuntimeError('Runtime is closed; no team isolation update applied')
            originals, additions = [], []
            for target, path, module, desired, allowed in replacements:
                if len(path) == 1:
                    owner, name = module, path[0]
                    live = vars(owner).get(name)
                else:
                    owner, name = getattr(module, path[0]), path[-1]
                    if owner.__module__ != module.__name__ or owner.__name__ != path[0]:
                        raise RuntimeError('Unknown team isolation class: ' + target)
                    instance = canvas if path[0] == 'Canvas' else runtime
                    if target in NEW_METHODS and name not in vars(owner):
                        if hasattr(instance, name):
                            raise RuntimeError('Unexpected team isolation method override: ' + name)
                        additions.append((owner, name, desired))
                        continue
                    live = _bound(owner, name, instance)
                if (not isinstance(live, FunctionType) or live.__globals__ is not vars(module)
                        or live.__module__ != module.__name__ or signature(live) not in allowed
                        or live.__code__.co_freevars != desired.__code__.co_freevars):
                    actual = signature(live) if isinstance(live, FunctionType) else type(live).__name__
                    raise RuntimeError('Unknown live team isolation function: ' + target + '; ' + str(actual))
                originals.append((target, live, live.__code__, desired.__code__))
            previous_instructions, tools = codex_runtime.INSTRUCTIONS, codex_runtime.TOOLS
            if type(previous_instructions) is not str or digest(previous_instructions) not in INSTRUCTIONS:
                raise RuntimeError('Unknown live team isolation instructions')
            if type(tools) is not list or digest(tools) not in TOOLS:
                raise RuntimeError('Unknown live team isolation tools')
            desired_tools = _tools(sources['codex_runtime'], tools)
            if digest(desired_tools) != TOOLS[1]:
                raise RuntimeError('Unreviewed team isolation tools')
            previous_helper = sys.modules.get(HELPER_NAME)
            previous_tools = list(tools)
            if (not additions and all(old == desired for _, _, old, desired in originals)
                    and previous_instructions == instructions and tools == desired_tools
                    and previous_helper is helper):
                return {'status': 'already_applied', 'baseCommit': BASE_COMMIT}
            added = []
            try:
                sys.modules[HELPER_NAME] = helper
                for owner, name, desired in additions:
                    setattr(owner, name, desired)
                    added.append((owner, name))
                for _, live, _, desired in originals:
                    live.__code__ = desired
                tools[:] = desired_tools
                codex_runtime.INSTRUCTIONS = instructions
                _active_frames(originals)
            except BaseException:
                for _, live, old, _ in originals:
                    if live.__code__ is not old:
                        live.__code__ = old
                for owner, name in reversed(added):
                    delattr(owner, name)
                tools[:] = previous_tools
                codex_runtime.INSTRUCTIONS = previous_instructions
                if previous_helper is None:
                    sys.modules.pop(HELPER_NAME, None)
                else:
                    sys.modules[HELPER_NAME] = previous_helper
                raise
            return {'status': 'applied', 'baseCommit': BASE_COMMIT,
                    'methods': list(EXPECTED | NEW_METHODS)}
        finally:
            runtime.lock.release()
    finally:
        canvas.lock.release()
