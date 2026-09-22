"""Expose the reviewed native review tool without restarting active agents."""
import ast
import hashlib
import importlib
from pathlib import Path
import sys
from types import FunctionType

from codex_active_task_update import signature
from codex_progress_update import digest, source_function, source_instructions

BASE_COMMIT = '962792c'
# BEGIN REVIEWED INPUTS
EXPECTED = {'codex_runtime.Runtime.tool_definitions': ['13fe92bce87c320a02c6549abb73ce5d6da12227bf82410f778fa19d076d1a6b',
                                            'ca97261114055d51b350c516770ab72c9c79fe9973d6e33c166ca474f0180bb4'],
 'codex_runtime.Runtime.dispatch': ['f292cc479e57713129bf6b357734ccef4376830af12330d39c9bdfac5e387ace',
                                    'd2c550773ccf233c458eca32581970bab778154dd5f222497fbd3575a11ce793'],
 'codex_runtime.Runtime.notification': ['143d829b3d40c95ce7901653ca6e44c6c1b7ab68051be617f27784dd1da620c9',
                                        'db8bfa2ea7bb77e4f94b1113ea7e82bed3537435fdd950d387e13d612380816a'],
 'codex_runtime.Runtime.dynamic': ['ab901b2e1e658cab5d2126a8e904dba16236eaf0ae5f5c746c29e5f27f669759',
                                   '188a983ca1726b5e128887eb7e0bcc84a0786cec7bc8a8bef9ec6a02be8d801c'],
 'codex_runtime.Runtime.run_native_action': ['d1838dc91719db821c24a4988c391c52c0095363859c4a14e31692169fef6dbe',
                                             'dc5eebef2e1fb286cbccb54eab8c8060bd44d51c010f822897725dd23fccdaf8'],
 'codex_runtime.Runtime.interrupt': ['3bba471054395b3df0f4c9b8930a832c8d0439c7152df3d970a263ca6a9760ad',
                                     'bd6313c1e86f655324a072083cef4fa623bc5f8dafa4e628033073d121e8597c'],
 'codex_runtime.Runtime.parent_event': ['c7d671907bf3bb0198f3eb805b001a39b3eed8a049b4e7a73642378cad3104f9',
                                        'c7d671907bf3bb0198f3eb805b001a39b3eed8a049b4e7a73642378cad3104f9'],
 'codex_runtime.Runtime.new_thread_params': ['e2e34a2a76cb8224e75ca2d29e149ccebf81c4252dc234608b3d2ae8d5a6eacb',
                                             'e2e34a2a76cb8224e75ca2d29e149ccebf81c4252dc234608b3d2ae8d5a6eacb'],
 'codex_tool_requests.RequestMixin.tool_request_key': ['37506da0285a19e869c9e6134f568239874e32f7d09384c28813ceab2e13a151',
                                                       'dd4292f98eced4c2fff17b4ff5616798add10e1400f8bcde4fc0eebf7b5d6443'],
 'codex_tool_requests.RequestMixin.reserve_tool_request': ['b827f657572343dea3883e9eab56ec7c9eaf8eb0cc26d16e50c4ccf8229e4096',
                                                           'da834f9cdb328d14fae6975788e5132f02ad4f8a65e058043e8549ec21211ea6'],
 'codex_tool_requests.RequestMixin.finish_tool_request': ['55c39ff1b3d5da9c5a38316528632efeb8f6ebebce6e2a6a5ebd7bd0dd855a26',
                                                          '5eb30c439dd49079e34ce6e1ea0c46af2e471d20c20ee63fd0984e7754fd6d70'],
 'codex_tool_requests.RequestMixin.request_action': ['5b4414a2029f2de0b15ebff8227e63194bced7b4f1aa738ef309f63140ea68b8',
                                                     '575cd7275f7cd8446a27cb8be753ad013d29ee528928e8f1c95ac76a01d06ff3'],
 'codex_tool_requests._identity_command': ['aa469fc101d24df38cae9b5abb552a53b7d6e415a98e49628972df2791346e0b',
                                           'ccdcf751a75b289a36ec4ab35f8513164a166a1d6beeba1c2c3f222e3105e8dd']}
REVIEW_SHA256 = 'a6c58a379f4de767ef577b9cb752382d8194fe16b007096771409022a5bab2d5'
TOOLS = ('aa2087d1a34738014481b31e7b5bcf06c9db286a9a2b7c736c54f6be2e9b65a9',
 'a6be3056f32a64f1dc4e0919ef8989e73e5f9a3b4a96b67fa0cdbc869dfc2146')
INSTRUCTIONS = ('8b48f15da523f9241eddb1e8905662e5fb59f88c11e455964f6c0cfa223d91f7',
 'bb97eaf912ca1ad78ddc932a4ff922e7e467e09d7e21a1c22c6811db29e406b8')
TOOL_FACTORY = 'ec144586e2d513cf5defc8c5e618c5ed11865815fb627dff46a25b442e94c7ee'
# END REVIEWED INPUTS


def _install(live, desired):
    live.__code__, live.__defaults__, live.__kwdefaults__ = desired.__code__, desired.__defaults__, desired.__kwdefaults__


def _checked_review(directory):
    path = directory / 'codex_agent_review.py'
    raw = path.read_bytes()
    if hashlib.sha256(raw).hexdigest() != REVIEW_SHA256:
        raise RuntimeError('Unreviewed agent review module source')
    module = sys.modules.get('codex_agent_review')
    if module is None:
        return None
    if Path(getattr(module, '__file__', '')).resolve() != path:
        raise RuntimeError('Unknown agent review module origin')
    namespace = vars(module)
    for node in ast.parse(raw).body:
        if isinstance(node, ast.FunctionDef):
            live = namespace.get(node.name)
            desired, static = source_function(raw, (node.name,), namespace)
            if (static or not isinstance(live, FunctionType) or live.__globals__ is not namespace
                    or live.__module__ != module.__name__ or signature(live) != signature(desired)):
                raise RuntimeError('Unknown loaded agent review function: ' + node.name)
        elif isinstance(node, ast.Assign):
            value = ast.literal_eval(node.value)
            if any(not isinstance(target, ast.Name) or namespace.get(target.id) != value for target in node.targets):
                raise RuntimeError('Unknown agent review constant')
        elif isinstance(node, ast.Import):
            for alias in node.names:
                if namespace.get(alias.asname or alias.name) is not importlib.import_module(alias.name):
                    raise RuntimeError('Unknown agent review import')
        elif isinstance(node, ast.ImportFrom):
            imported = importlib.import_module(node.module)
            for alias in node.names:
                if namespace.get(alias.asname or alias.name) is not getattr(imported, alias.name):
                    raise RuntimeError('Unknown agent review import')
        elif not (isinstance(node, ast.Expr) and isinstance(node.value, ast.Constant) and isinstance(node.value.value, str)):
            raise RuntimeError('Unknown agent review module structure')
    return module


def apply(runtime):
    import codex_runtime
    import codex_tool_requests
    if sys.version_info[:2] != (3, 14) or type(runtime) is not codex_runtime.Runtime:
        raise RuntimeError('Unknown agent review runtime')
    if not EXPECTED or not REVIEW_SHA256 or not all(TOOLS + INSTRUCTIONS) or not TOOL_FACTORY:
        raise RuntimeError('Agent review source review is incomplete')
    directory = Path(__file__).resolve().parent
    _checked_review(directory)
    replacements = []
    for target, allowed in EXPECTED.items():
        module_name, *path = target.split('.')
        module = sys.modules.get(module_name)
        if module is None or Path(getattr(module, '__file__', '')).resolve() != directory / (module_name + '.py'):
            raise RuntimeError('Unexpected agent review module: ' + module_name)
        desired, static = source_function((directory / (module_name + '.py')).read_text(), path, vars(module))
        if signature(desired) != allowed[1]:
            raise RuntimeError('Unreviewed agent review replacement: ' + target)
        owner = getattr(module, path[0]) if len(path) > 1 else module
        replacements.append((owner, path[-1], desired, static, allowed, target))
    instructions = source_instructions((directory / 'codex_runtime.py').read_text())
    if digest(instructions) != INSTRUCTIONS[1]:
        raise RuntimeError('Unreviewed agent review instructions')
    if not runtime.start_lock.acquire(timeout=10):
        raise RuntimeError('Native connection is busy; agent review update waits')
    try:
        if not runtime.lock.acquire(timeout=10):
            raise RuntimeError('Runtime is busy; agent review update waits')
        try:
            if runtime.closed:
                raise RuntimeError('Runtime closed')
            runtime_names = {target.split('.')[-1] for target in EXPECTED
                             if target.startswith(('codex_runtime.Runtime.', 'codex_tool_requests.RequestMixin.'))}
            if runtime_names.intersection(vars(runtime)):
                raise RuntimeError('Unknown agent review runtime override')
            inherited_names = {target.split('.')[-1] for target in EXPECTED
                               if target.startswith('codex_tool_requests.RequestMixin.')}
            if inherited_names.intersection(vars(codex_runtime.Runtime)):
                raise RuntimeError('Unknown agent review request override')
            originals = []
            for owner, name, desired, static, allowed, target in replacements:
                descriptor = vars(owner).get(name)
                live = descriptor.__func__ if isinstance(descriptor, staticmethod) else descriptor
                if live is None and allowed[0] is None:
                    originals.append((owner, name, None, desired, static, None))
                    continue
                if (not isinstance(live, FunctionType) or isinstance(descriptor, staticmethod) != static
                        or signature(live) not in allowed or live.__globals__ is not desired.__globals__
                        or live.__module__ != desired.__module__ or live.__code__.co_freevars != desired.__code__.co_freevars):
                    raise RuntimeError('Unknown live agent review method: ' + target)
                if signature(live) != allowed[1]:
                    originals.append((owner, name, live, desired, static, (live.__code__, live.__defaults__, live.__kwdefaults__)))
            tools = codex_runtime.TOOLS
            previous_instructions = codex_runtime.INSTRUCTIONS
            if type(tools) is not list or digest(tools) not in TOOLS:
                raise RuntimeError('Unknown live agent review tools')
            if type(previous_instructions) is not str or digest(previous_instructions) not in INSTRUCTIONS:
                raise RuntimeError('Unknown live agent review instructions')
            factory = codex_runtime.tool
            if (not isinstance(factory, FunctionType) or factory.__globals__ is not vars(codex_runtime)
                    or factory.__module__ != codex_runtime.__name__ or signature(factory) != TOOL_FACTORY):
                raise RuntimeError('Unknown agent review tool factory')
            prior_module = _checked_review(directory)
            had_import = 'review_tools' in vars(codex_runtime)
            prior_import = vars(codex_runtime).get('review_tools')
            if had_import and (prior_module is None or prior_import is not prior_module.review_tools):
                raise RuntimeError('Unknown agent review tools import')
            previous_tools = list(tools)
            try:
                module = importlib.import_module('codex_agent_review')
                _checked_review(directory)
                definitions = module.review_tools(factory, codex_runtime.TEXT)
                if (type(definitions) is not list or len(definitions) != 1
                        or definitions[0].get('name') != 'orchestration_review'):
                    raise RuntimeError('Unexpected agent review tool definition')
                # Retain all existing tool objects and the list held by callbacks.
                # Older native tool snapshots use the existing workspace bridge.
                desired_tools = tools if digest(tools) == TOOLS[1] else [*tools, *definitions]
                if digest(desired_tools) != TOOLS[1]:
                    raise RuntimeError('Unreviewed agent review tool definition')
                changed = bool(originals or digest(tools) != TOOLS[1]
                               or previous_instructions != instructions or not had_import)
                codex_runtime.review_tools = module.review_tools
                for owner, name, live, desired, static, _ in originals:
                    if live is None:
                        setattr(owner, name, staticmethod(desired) if static else desired)
                    else:
                        _install(live, desired)
                tools[:] = desired_tools
                codex_runtime.INSTRUCTIONS = instructions
            except BaseException:
                for owner, name, live, _, _, previous in reversed(originals):
                    if live is None:
                        if name in vars(owner):
                            delattr(owner, name)
                    else:
                        live.__code__, live.__defaults__, live.__kwdefaults__ = previous
                tools[:] = previous_tools
                codex_runtime.INSTRUCTIONS = previous_instructions
                if had_import:
                    codex_runtime.review_tools = prior_import
                else:
                    vars(codex_runtime).pop('review_tools', None)
                if prior_module is None:
                    sys.modules.pop('codex_agent_review', None)
                raise
            return {'status': 'applied' if changed else 'already_applied', 'baseCommit': BASE_COMMIT,
                    'methods': list(EXPECTED), 'addedTools': ['orchestration_review'],
                    'agentReviewSha256': REVIEW_SHA256}
        finally:
            runtime.lock.release()
    finally:
        runtime.start_lock.release()
