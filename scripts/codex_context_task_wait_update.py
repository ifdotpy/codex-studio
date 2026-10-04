"""Apply exact task receipt recovery without stopping the backend."""
import ast
import hashlib
from pathlib import Path
import sys

from codex_source import signature, source_function

SOURCE_SHA256 = '22fe678dd0d47eb9ba7c8117223e7fce6995ed9ffb0c2cb2593af83f37acbb2f'
FUNCTIONS = (('_task_check_agent', None, '47fd4201a66b792a9b4af54a9fa68c4ac2fb530dcec87c22d74e667f640aca30'),
 ('_schedule_task_wait_check', None, '8fa4f33bdb4e8a74710e76282022af4806de6a37c558e2e3d8d99c365134ddf9'),
 ('_release_task_check', None, 'b4f5051896187170facc808c8db303176cb3a1c89fd0377fa67fe8eb1223f6a5'),
 ('_completed_task_receipt', None, 'a24b2476a38fbb53c41406f70c1a6ea5a04c28edd0f8168d3605a3042239e985'),
 ('_run_task_wait_check', None, 'c10f65a62228222aa4e1642bd6a4e160ae951609713a8ec9e2cf3509ecee19de'),
 ('claim_context_wait',
  '68ff09f819ae244822cb50c6bcb9b7776031ad9709f3270567627b2f54370d98',
  '621d6bb41721ba22fb8b90740d5e6245da27e9219ef2616a5154c120fad92cdb'))
CONSTANTS = {'TASK_CHECK_RETRY_SECONDS': 15,
 'TASK_CHECK_SECONDS': 20,
 'TASK_CHECK_TYPES': {'collabAgentToolCall',
                      'commandExecution',
                      'computerToolCall',
                      'dynamicToolCall',
                      'fileChange',
                      'mcpToolCall'}}


def apply(runtime):
    scripts = Path(__file__).resolve().parent
    module = sys.modules.get('codex_runtime')
    context = sys.modules.get('codex_context_repair')
    path = scripts / 'codex_context_repair.py'
    if (module is None or not isinstance(runtime, module.Runtime) or runtime.closed
            or Path(module.__file__).resolve().parent != scripts
            or context is None or Path(context.__file__).resolve() != path):
        raise RuntimeError('The running backend source identity differs')
    raw = path.read_bytes()
    if hashlib.sha256(raw).hexdigest() != SOURCE_SHA256:
        raise RuntimeError('The reviewed context repair source differs')
    values = {}
    for node in ast.parse(raw).body:
        if isinstance(node, ast.Assign) and len(node.targets) == 1:
            target = node.targets[0]
            if isinstance(target, ast.Name) and target.id in CONSTANTS:
                if target.id in values:
                    raise RuntimeError('The reviewed recovery constant is duplicated')
                values[target.id] = ast.literal_eval(node.value)
    if values != CONSTANTS:
        raise RuntimeError('The reviewed recovery constants differ')
    plans = []
    for name, before, after in FUNCTIONS:
        desired, _ = source_function(raw, [name], vars(context), str(path))
        if signature(desired) != after:
            raise RuntimeError('The reviewed recovery function differs: ' + name)
        plans.append((name, before, after, desired))
    with runtime.lock:
        already = True
        for name, before, after, desired in plans:
            current = vars(context).get(name)
            actual = None if current is None else signature(current)
            if actual not in {before, after}:
                raise RuntimeError('The running recovery function differs: ' + name)
            already = already and actual == after
        for name, value in values.items():
            if name in vars(context) and vars(context)[name] != value:
                raise RuntimeError('The running recovery constant differs: ' + name)
        if already and all(name in vars(context) for name in values):
            return {'status': 'already_applied'}
        vars(context).update(values)
        for name, before, after, desired in plans:
            current = vars(context).get(name)
            if current is None:
                vars(context)[name] = desired
            else:
                current.__code__ = desired.__code__
                current.__defaults__ = desired.__defaults__
                current.__kwdefaults__ = desired.__kwdefaults__
    runtime.changed.set()
    return {'status': 'applied'}
