"""Apply the exact file-metadata route without interrupting active agents."""
from pathlib import Path
import sys
from types import FunctionType
from codex_active_task_update import signature
from codex_progress_update import source_function, _http_closure

_EXPECTED = {'route_old': 'efcb921bfccd63c4bdfee7b71c77395a2d5b46adace2959fa4cf61f25b0351ce', 'route_new': '42dab8a766ab8a53f7d3dcf4f263f3157ef89a085df1fa35adb541df8bbda88a', 'info': '603c07aa146224cb4101e92ccc50f6ab911091a9dd843b15da5c97ae4c110631'}


def apply(runtime, handler_class):
    if sys.version_info[:2] != (3, 14):
        raise RuntimeError('This update requires the reviewed Python 3.14 runtime')
    from codex_workspace import WorkspaceMixin
    import codex_workspace
    import codex_canvas
    if not isinstance(runtime, WorkspaceMixin) or runtime.closed:
        raise RuntimeError('Unknown or closed runtime')
    if (handler_class.__module__ != 'codex_canvas'
            or handler_class.__qualname__ != 'make_server.<locals>.Handler'):
        raise RuntimeError('Unknown HTTP handler')
    source = Path(__file__).parent
    route, route_static = source_function((source/'codex_canvas.py').read_text(),
                               ('make_server','Handler','do_GET'), vars(codex_canvas))
    info, info_static = source_function((source/'codex_workspace.py').read_text(),
                              ('WorkspaceMixin','file_info'), vars(codex_workspace))
    if route_static or info_static or signature(route) != _EXPECTED['route_new'] or signature(info) != _EXPECTED['info']:
        raise RuntimeError('Unreviewed replacement source')
    if not runtime.lock.acquire(timeout=10):
        raise RuntimeError('Runtime remains busy; no file update applied')
    missing=object()
    previous=vars(WorkspaceMixin).get('file_info', missing)
    current=vars(handler_class).get('do_GET')
    try:
        if runtime.closed:
            raise RuntimeError('Runtime is closed; no file update applied')
        if (not isinstance(current,FunctionType) or current.__globals__ is not vars(codex_canvas)
                or signature(current) not in {_EXPECTED['route_old'],_EXPECTED['route_new']}):
            raise RuntimeError('Unknown live HTTP implementation')
        _http_closure(current,runtime,handler_class)
        if current.__code__.co_freevars != route.__code__.co_freevars:
            raise RuntimeError('HTTP closure names changed')
        if previous is not missing and (not isinstance(previous,FunctionType)
                or previous.__globals__ is not vars(codex_workspace) or signature(previous) != _EXPECTED['info']):
            raise RuntimeError('Unknown live file metadata method')
        if 'file_info' in vars(runtime) or getattr(type(runtime),'file_info',missing) is not previous:
            raise RuntimeError('Unknown file metadata method binding')
        if signature(current)==_EXPECTED['route_new'] and previous is not missing:
            return {'status':'already_applied','baseCommit':'a6fa02e'}
        old_code=current.__code__
        try:
            WorkspaceMixin.file_info=info
            current.__code__=route.__code__
        except BaseException:
            current.__code__=old_code
            if previous is missing:delattr(WorkspaceMixin,'file_info')
            else:WorkspaceMixin.file_info=previous
            raise
        return {'status':'applied','baseCommit':'a6fa02e','methods':['file_info','do_GET']}
    finally:
        runtime.lock.release()
