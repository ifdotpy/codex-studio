"""Apply agent modes and the complete model catalog without restarting work."""
import ast
import hashlib
import importlib.util
from pathlib import Path
import sys
from types import FunctionType, MethodType, ModuleType

from codex_active_task_update import signature
from codex_progress_update import source_function, _http_closure
from codex_resource_removal_update import _find_handler

BASE_COMMIT = '0b0625e'
EXPECTED = {'codex_canvas.make_server.Handler.do_POST': ('b1ca1845e6a542e241321f2910da4f3edc388711c4268ebed326f7e8a113a7db',
                                              'decb0105e2be4dc46614eb8112f7b3f8b99484d6e53920de2bba1cf275338771'),
 'codex_catalog.ModelCatalogCache.read': ('0e6c49f9ffb2d5f428ba0020ebedac7886e6bfbe9f8fe791699190d4bc403024',
                                          '901f5b23b3cd645663bd951aadac371cc07fa54d166d498ea8309371ec118c4e'),
 'codex_chat_reviews.review_schedule': ('4c44491c0fae5f08d608f785250cb5c966cf317552317c646740cb2b53cde671',
                                        '506caa3cbe2e21c813c92216ee0f250a267cc52874edc4b6f08185e1e8438852'),
 'codex_chat_reviews.review_tick': ('e2094cbd0396ad019db3c298072109fff2708602dd3616020647172a53614124',
                                    '0eca3737cdf1bc7edd26c4b33bc7be5f8f38fd64f1d0ef4fdc697bec4f353e26'),
 'codex_efficiency.EfficiencyMixin.model_tool_result': ('aecdb1f1d280fb153f3ffa161fe50e5a6bfa223ee04f9fdec5aafed5e5b3f1d2',
                                                        'd353f9759416e4384bf4f19438906af3dcb0f619934ae3d064360542a40c003d'),
 'codex_efficiency.EfficiencyMixin.model_turn_context': ('51781d0f57f57541123efb55c26b3c0f81d85292ef2d144455a6a9bde9c5ecbb',
                                                         '856efd3fe7d51d2009ee51d00230e61cf48e9e56ce43ab15d3b21315c935b4be'),
 'codex_runtime.Runtime.agent': ('dabfda15f0e6f94d370e3834b78893c1f1f4922ded2f2b0a57987f88f69ae97f',
                                 'ccf79f5308465d49a6545d81eaf5afcae1463b724361d2573fe494274879939a'),
 'codex_runtime.Runtime.chat_message': ('d9b157c0d6c051a82b9ed74dd8919393097df1657ecdf9f3b4ccec1ef105df9d',
                                        'b1a3a90e723b6f4dfb5568412b066b2714c58c5ec12afb9224a767580f33fe5f'),
 'codex_runtime.Runtime.conversation_settings': ('185b567877fa4619d3c6c5f564bd235a9f6cfd57b283cac6ae751041ffbf892e',
                                                 'c802fb58c3e7c2f0087dfd89bbfbda27bb2bb005358bda1a1f73ba35bb813580'),
 'codex_runtime.Runtime.create': ('00fc2674294910256a14bfbc210dfdf2b291e2177ae5187990c6edacb77f06a9',
                                  '39db08d6f272f7fa0664031f3fc69a03ce127439382d639895f00a8d064f5a74'),
 'codex_runtime.Runtime.native_action': ('d5902623c1f95b33616838b7235c78106bbb468335c2b6d200c7926b5d3538e0',
                                         '71c665aa2afec153428aaff002d00a722738acc1c30d2fbd4cf663b7e8e0529c'),
 'codex_runtime.Runtime.new_lead': ('8aab441f00c6e6a2a3e3fb7fded4758bda29b05f2bf3bddd12abf1eedcaf31d4',
                                    '9fc1d508ab9e362bc8a3caa4cf15e3f515223a30c55f06e281c4cde4b4eea033'),
 'codex_runtime.Runtime.records': ('d81924ce7e5eafab2be65e08548a9077a322ec2bcc8dbd85917c4f1abb341c8a',
                                   'caceaca54f22d164a4a258da8b5760007d80f333346b9c501ff63b5e207b82a8'),
 'codex_runtime.Runtime.send': ('1f54db3e0594af33f273df6a235cf02bb6ae12a187f83d819d9079d926f7d1da',
                                '62b77be1c3618219eae7ffdc070bcaf55a172ddbceb6a4d135bb9cccbfdb486d'),
 'codex_runtime.Runtime.spawn_agents': ('f2a5a1c5be2921a3bad4aca84dc695225fcdbce6ae72441bfe695fe4057b6e7c',
                                        '8fdf537059ad918fecef98e05d8989c169101a9dc6344ca4f7fd424d69ce69ac'),
 'codex_work.WorkMixin.work_action': ('07a3fc273b3e024f1778616172d6551466b8b6056ad56c8c4b946f55d5efc1fa',
                                      '83907197198082242cf723594b4f8707b4aaa0374bc55a5cc629b40dc9203527')}
HELPER_NAME = 'codex_agent_modes'
HELPER_SHA = '4531c5fc42034330191e19d32527c204e5fe00b56bf4bc700380bdbef693f522'
DEFAULT_MODEL = 'gpt-6-astra'
MISSING = object()


def _helper(directory):
    path = directory / (HELPER_NAME + '.py')
    raw = path.read_bytes()
    if hashlib.sha256(raw).hexdigest() != HELPER_SHA:
        raise RuntimeError('Unreviewed agent mode helper source')
    spec = importlib.util.spec_from_file_location(HELPER_NAME, path)
    reviewed = importlib.util.module_from_spec(spec)
    exec(compile(raw, str(path), 'exec', dont_inherit=True), vars(reviewed))
    live = sys.modules.get(HELPER_NAME)
    if live is None:
        return reviewed
    if (type(live) is not ModuleType or live.__name__ != HELPER_NAME
            or Path(getattr(live, '__file__', '')).resolve() != path):
        raise RuntimeError('Unknown agent mode helper module')
    metadata = {'__name__', '__doc__', '__package__', '__loader__', '__spec__', '__file__', '__cached__'}
    actual = {k: v for k, v in vars(live).items() if k not in metadata}
    expected = {k: v for k, v in vars(reviewed).items() if k not in metadata}
    if actual.keys() != expected.keys():
        raise RuntimeError('Unknown agent mode helper globals')
    for name, desired in expected.items():
        value = actual[name]
        if isinstance(desired, FunctionType) and desired.__module__ == HELPER_NAME:
            if (not isinstance(value, FunctionType) or value.__module__ != HELPER_NAME
                    or value.__globals__ is not vars(live) or value.__closure__ is not None
                    or signature(value) != signature(desired)):
                raise RuntimeError('Unknown agent mode helper function: ' + name)
        elif type(desired) in (str, int, float, bool, type(None), tuple, frozenset):
            if type(value) is not type(desired) or value != desired:
                raise RuntimeError('Unknown agent mode helper constant: ' + name)
        elif value is not desired:
            raise RuntimeError('Unknown agent mode helper dependency: ' + name)
    return live


def _active_frames(originals):
    previous = {id(old[0]): target for target, _, old, desired in originals
                if old[0] is not desired.__code__}
    for frame in sys._current_frames().values():
        while frame is not None:
            target = previous.get(id(frame.f_code))
            if target is not None:
                raise RuntimeError('Agent modes wait for an earlier call: ' + target)
            frame = frame.f_back


def _live_function(owner, name, static, instance):
    descriptor = vars(owner).get(name)
    if static != isinstance(descriptor, staticmethod):
        raise RuntimeError('Unknown agent mode method descriptor: ' + name)
    live = descriptor.__func__ if static else descriptor
    if instance is not None:
        classes = type(instance).__mro__
        if owner not in classes:
            raise RuntimeError('Unknown agent mode owner: ' + name)
        if name in vars(instance) or any(name in vars(cls) for cls in classes[:classes.index(owner)]):
            raise RuntimeError('Unexpected agent mode method override: ' + name)
        bound = getattr(instance, name)
        if static:
            valid = bound is live
        else:
            valid = isinstance(bound, MethodType) and bound.__self__ is instance and bound.__func__ is live
        if not valid:
            raise RuntimeError('Unknown agent mode method binding: ' + name)
    return live


def apply(runtime, handler_class=None):
    import codex_runtime
    if sys.version_info[:2] != (3, 14) or type(runtime) is not codex_runtime.Runtime:
        raise RuntimeError('Unknown runtime for agent modes')
    handler_class = handler_class or _find_handler(runtime)
    if (type(handler_class) is not type or handler_class.__module__ != 'codex_canvas'
            or handler_class.__qualname__ != 'make_server.<locals>.Handler'):
        raise RuntimeError('Unknown agent mode HTTP handler')
    _http_closure(handler_class.do_GET, runtime, handler_class)
    directory = Path(__file__).resolve().parent
    modules, sources, replacements = {}, {}, []
    for name in {target.split('.')[0] for target in EXPECTED}:
        module = sys.modules.get(name)
        path = directory / (name + '.py')
        if module is None or Path(getattr(module, '__file__', '')).resolve() != path:
            raise RuntimeError('Unknown agent mode module location: ' + name)
        modules[name], sources[name] = module, path.read_text()
    for target, allowed in EXPECTED.items():
        name, *path = target.split('.')
        method = path[-1]
        module = modules[name]
        if len(path) == 3:
            owner = handler_class
            desired, static = source_function(sources[name], tuple(path), vars(module),
                                              closure=owner.do_POST.__closure__)
        else:
            class_name = path[0] if len(path) == 2 else None
            owner = vars(module).get(class_name) if class_name else module
            if class_name and (not isinstance(owner, type) or owner.__module__ != name or owner.__name__ != class_name):
                raise RuntimeError('Unknown agent mode class: ' + target)
            desired, static = source_function(sources[name], tuple(path), vars(module))
        if signature(desired) != allowed[1]:
            raise RuntimeError('Unreviewed agent mode replacement: ' + target)
        replacements.append((target, module, owner, method, desired, static, allowed))
    assignments = [node for node in ast.parse(sources['codex_runtime']).body
                   if isinstance(node, ast.Assign) and any(isinstance(target, ast.Name)
                   and target.id == 'DEFAULT_LEAD_MODEL' for target in node.targets)]
    if len(assignments) != 1 or ast.literal_eval(assignments[0].value) != DEFAULT_MODEL:
        raise RuntimeError('Unreviewed lead model default')
    helper = _helper(directory)
    if not runtime.lock.acquire(timeout=10):
        raise RuntimeError('Runtime remains busy; no agent mode update applied')
    try:
        if runtime.closed:
            raise RuntimeError('Runtime is closed; no agent mode update applied')
        originals = []
        cache = vars(runtime).get('_catalog_cache')
        if cache is not None and type(cache) is not modules['codex_catalog'].ModelCatalogCache:
            raise RuntimeError('Unknown live model catalog cache')
        for target, module, owner, method, desired, static, allowed in replacements:
            if isinstance(owner, ModuleType) or owner is handler_class:
                instance = None
            else:
                instance = cache if owner.__name__ == 'ModelCatalogCache' else runtime
            live = _live_function(owner, method, static, instance)
            if (not isinstance(live, FunctionType) or live.__globals__ is not vars(module)
                    or live.__module__ != module.__name__ or signature(live) not in allowed
                    or live.__code__.co_freevars != desired.__code__.co_freevars):
                actual = signature(live) if isinstance(live, FunctionType) else type(live).__name__
                raise RuntimeError('Unknown live agent mode function: ' + target + '; ' + str(actual))
            originals.append((target, live,
                              (live.__code__, live.__defaults__, live.__kwdefaults__), desired))
        previous_model = vars(codex_runtime).get('DEFAULT_LEAD_MODEL', MISSING)
        if previous_model is not MISSING and (type(previous_model) is not str or previous_model != DEFAULT_MODEL):
            raise RuntimeError('Unknown live lead model default')
        if previous_model is MISSING and vars(codex_runtime).get('LEAD_MODELS') != ('gpt-6-astra', 'gpt-5.6-sol'):
            raise RuntimeError('Unknown previous lead model defaults')
        previous_helper = sys.modules.get(HELPER_NAME)
        if (all(signature(live) == signature(desired) for _, live, _, desired in originals)
                and previous_model == DEFAULT_MODEL and previous_helper is helper):
            return {'status': 'already_applied', 'baseCommit': BASE_COMMIT}
        try:
            sys.modules[HELPER_NAME] = helper
            codex_runtime.DEFAULT_LEAD_MODEL = DEFAULT_MODEL
            for _, live, _, desired in originals:
                live.__defaults__ = desired.__defaults__
                live.__kwdefaults__ = desired.__kwdefaults__
                live.__code__ = desired.__code__
            # An earlier settings call must settle before it can overwrite a mode change.
            # An old catalog callback may settle later. The new read retains that future.
            _active_frames(originals)
        except BaseException:
            for _, live, previous, _ in originals:
                live.__code__, live.__defaults__, live.__kwdefaults__ = previous
            if previous_model is MISSING:
                vars(codex_runtime).pop('DEFAULT_LEAD_MODEL', None)
            else:
                codex_runtime.DEFAULT_LEAD_MODEL = previous_model
            if previous_helper is None:
                sys.modules.pop(HELPER_NAME, None)
            else:
                sys.modules[HELPER_NAME] = previous_helper
            raise
        return {'status': 'applied', 'baseCommit': BASE_COMMIT, 'methods': list(EXPECTED)}
    finally:
        runtime.lock.release()
