"""Apply the assignment delivery default without restarting native sessions."""
import ast
import hashlib
import json
from pathlib import Path
import sys
from types import FunctionType

from codex_active_task_update import compile_function, signature

EXPECTED = {'dynamic': ('282a4c1fda1e179c4dd95aa0fb3dc2384d91bc0afd8c4984decc3ecb7732225c',
             'ab901b2e1e658cab5d2126a8e904dba16236eaf0ae5f5c746c29e5f27f669759'),
 'tool': ('7a8b25063e1132ea354070b9202a8752510e6c884891e96f7ed9e37cfdd5a424',
          'a262b46ad954d414d0b71fc7f9e02d8354e221113fb96c54d4ccd517167a282d')}


def send_definition(source, namespace):
    tree = ast.parse(source)
    nodes = [node for node in tree.body if
        isinstance(node, ast.Assign) and any(isinstance(target, ast.Name) and target.id == 'TOOLS' for target in node.targets)
        or isinstance(node, ast.For) and isinstance(node.iter, ast.Name) and node.iter.id == 'TOOLS']
    scope = dict(namespace)
    exec(compile(ast.Module(body=nodes, type_ignores=[]), '<send-default-tools>', 'exec'), scope)
    return next(tool for tool in scope['TOOLS'] if tool['name'] == 'orchestration_send')


def definition_hash(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True).encode()).hexdigest()


def apply(runtime):
    import codex_runtime
    if sys.version_info[:2] != (3, 14) or type(runtime) is not codex_runtime.Runtime:
        raise RuntimeError('Unknown runtime for send default update')
    path = Path(__file__).resolve().with_name('codex_runtime.py')
    if Path(codex_runtime.__file__).resolve() != path:
        raise RuntimeError('Unexpected runtime source location')
    source = path.read_text()
    desired = compile_function(source, 'Runtime', 'dynamic', vars(codex_runtime))
    definition = send_definition(source, vars(codex_runtime))
    if signature(desired) != EXPECTED['dynamic'][1] or definition_hash(definition) != EXPECTED['tool'][1]:
        raise RuntimeError('Unreviewed send default replacement')
    if not runtime.lock.acquire(timeout=10):
        raise RuntimeError('Runtime remains busy; no send default update applied')
    try:
        live = codex_runtime.Runtime.dynamic
        tools = [tool for tool in codex_runtime.TOOLS if tool['name'] == 'orchestration_send']
        if (runtime.closed or not isinstance(live, FunctionType) or live.__globals__ is not vars(codex_runtime)
                or runtime.dynamic.__func__ is not live or signature(live) not in EXPECTED['dynamic']
                or len(tools) != 1 or definition_hash(tools[0]) not in EXPECTED['tool']):
            raise RuntimeError('Unknown live send implementation; no update applied')
        if signature(live) == EXPECTED['dynamic'][1] and definition_hash(tools[0]) == EXPECTED['tool'][1]:
            return {'status': 'already_applied'}
        before = (live.__code__, live.__defaults__, live.__kwdefaults__, tools[0]['description'])
        try:
            live.__code__, live.__defaults__, live.__kwdefaults__ = (
                desired.__code__, desired.__defaults__, desired.__kwdefaults__)
            tools[0]['description'] = definition['description']
        except BaseException:
            live.__code__, live.__defaults__, live.__kwdefaults__, tools[0]['description'] = before
            raise
        return {'status': 'applied', 'default': 'steer active turns; start idle turns',
                'pendingMessages': 'Existing receipts retain their accepted delivery mode'}
    finally:
        runtime.lock.release()
