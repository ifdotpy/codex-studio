"""Apply workspace read isolation and the compact inbox route without a restart."""
import gc
from http.server import HTTPServer
from pathlib import Path
import sys
from types import CodeType, FunctionType

from codex_active_task_update import compile_function, signature

EXPECTED = {'do_GET': ('0967520427aa11defca65644d9f19d30ef5f87669a8f7baad04e88b65036b86f',
            '2e4377faede7f249729e1a0b60d61e465b8634be5e07d238d69c7d20ffbdca08'),
 'workspace_snapshot': ('75f685231bba148b4f4fcef392db0611249b32658a54ef0cd1e383658a195f21',
                        '9af6a56dd0257d819ac80e11f44e5dfe7edf56fd1893cc829b37d353c70fd684')}


def handler_function(source, live):
    code = compile(source, '<workspace-read-update>', 'exec', dont_inherit=True)
    for name in ('make_server', 'Handler', 'do_GET'):
        code = next(value for value in code.co_consts
                    if isinstance(value, CodeType) and value.co_name == name)
    if code.co_freevars != live.__code__.co_freevars:
        raise RuntimeError('Unknown workspace HTTP closure')
    return FunctionType(code, live.__globals__, live.__name__, live.__defaults__, live.__closure__)


def apply(runtime):
    import codex_canvas
    import codex_runtime
    import codex_workspace
    if sys.version_info[:2] != (3, 14) or type(runtime) is not codex_runtime.Runtime:
        raise RuntimeError('Unknown runtime for workspace read update')
    directory = Path(__file__).resolve().parent
    for module in (codex_canvas, codex_workspace):
        if Path(module.__file__).resolve() != directory / (module.__name__ + '.py'):
            raise RuntimeError('Unexpected workspace module location')
    handlers = []
    for server in gc.get_objects():
        if not isinstance(server, HTTPServer):
            continue
        live = server.RequestHandlerClass.do_GET
        if not isinstance(live, FunctionType) or live.__globals__ is not vars(codex_canvas):
            continue
        closure = dict(zip(live.__code__.co_freevars,
                           (cell.cell_contents for cell in live.__closure__ or ())))
        if getattr(closure.get('canvas'), 'runtime', None) is runtime:
            handlers.append(live)
    if len(handlers) != 1:
        raise RuntimeError('Expected one known HTTP handler for workspace read update')
    workspace = codex_workspace.WorkspaceMixin.workspace_snapshot
    desired_workspace = compile_function((directory / 'codex_workspace.py').read_text(),
        'WorkspaceMixin', 'workspace_snapshot', vars(codex_workspace))
    desired_http = handler_function((directory / 'codex_canvas.py').read_text(), handlers[0])
    staged = [('workspace_snapshot', workspace, desired_workspace), ('do_GET', handlers[0], desired_http)]
    for name, live, desired in staged:
        if signature(desired) != EXPECTED[name][1]:
            raise RuntimeError('Unreviewed workspace replacement: ' + name)
    if not runtime.lock.acquire(timeout=10):
        raise RuntimeError('Runtime remains busy; no workspace read update applied')
    changes = []
    try:
        if runtime.closed or runtime.workspace_snapshot.__func__ is not workspace:
            raise RuntimeError('Unknown live workspace method binding')
        for name, live, desired in staged:
            if signature(live) not in EXPECTED[name]:
                raise RuntimeError('Unknown live workspace function: ' + name)
            if signature(live) != EXPECTED[name][1]:
                changes.append((live, live.__code__, live.__defaults__, live.__kwdefaults__, desired))
        try:
            for live, code, defaults, kwdefaults, desired in changes:
                live.__code__, live.__defaults__, live.__kwdefaults__ = (
                    desired.__code__, desired.__defaults__, desired.__kwdefaults__)
        except BaseException:
            for live, code, defaults, kwdefaults, desired in reversed(changes):
                live.__code__, live.__defaults__, live.__kwdefaults__ = code, defaults, kwdefaults
            raise
        return {'status': 'applied' if changes else 'already_applied',
                'functions': [name for name, _, _ in staged]}
    finally:
        runtime.lock.release()
