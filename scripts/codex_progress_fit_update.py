"""Apply the exact progress-fit route and instructions without stopping agents."""
import hashlib
from pathlib import Path
import sys
from types import FunctionType

from codex_active_task_update import signature
from codex_progress_update import source_function, _http_closure

_EXPECTED = {
    "route_old": "a39bace85a134ba483867a02f2fc79c8b53596e799a6b2b8065b51774502c830",
    "route_new": "495849636d0d7042ea094f0428610b65462e076bb72f30e894ae7cc394e7177d",
    "context_old": "20883abdaa933c2a48706ff0c6bacf8b654d25245a33c2197ae4c291a8b0b911",
    "context_new": "9303e575e4bddf6fcb0ac5822a7c47d4ac610dbb3e3977bff0ffaed75ff9f1ca",
}
_LAYOUT_SHA256 = "1fece368a3d59e21381eb82d412e7974b3b29cf1490b4e722411237d9d8b0969"


def apply(runtime, handler_class):
    if sys.version_info[:2] != (3, 14):
        raise RuntimeError("This update requires the reviewed Python 3.14 runtime")
    import codex_canvas
    import codex_progress
    from codex_runtime import Runtime
    if type(runtime) is not Runtime or runtime.closed:
        raise RuntimeError("Unknown or closed runtime")
    if (handler_class.__module__ != "codex_canvas"
            or handler_class.__qualname__ != "make_server.<locals>.Handler"):
        raise RuntimeError("Unknown HTTP handler")
    source = Path(__file__).resolve().parent
    if (Path(codex_canvas.__file__).resolve().parent != source
            or Path(codex_progress.__file__).resolve().parent != source):
        raise RuntimeError("Unknown live progress source directory")
    if hashlib.sha256((source / "codex_progress_layout.py").read_bytes()).hexdigest() != _LAYOUT_SHA256:
        raise RuntimeError("Unreviewed progress layout helper")
    route, route_static = source_function((source / "codex_canvas.py").read_text(),
                                         ("make_server", "Handler", "do_POST"), vars(codex_canvas))
    context, context_static = source_function((source / "codex_progress.py").read_text(),
                                             ("progress_context",), vars(codex_progress))
    if (route_static or context_static or signature(route) != _EXPECTED["route_new"]
            or signature(context) != _EXPECTED["context_new"]):
        raise RuntimeError("Unreviewed progress replacement source")
    if not runtime.lock.acquire(timeout=10):
        raise RuntimeError("Runtime remains busy; no progress fit update applied")
    try:
        if runtime.closed:
            raise RuntimeError("Runtime is closed; no progress fit update applied")
        live_route = vars(handler_class).get("do_POST")
        live_context = vars(codex_progress).get("progress_context")
        for name, function, namespace in (("route", live_route, vars(codex_canvas)),
                                          ("context", live_context, vars(codex_progress))):
            if (not isinstance(function, FunctionType) or function.__globals__ is not namespace
                    or signature(function) not in {_EXPECTED[name + "_old"], _EXPECTED[name + "_new"]}):
                raise RuntimeError("Unknown live progress " + name)
        _http_closure(handler_class.do_GET, runtime, handler_class)
        if live_route.__code__.co_freevars != route.__code__.co_freevars:
            raise RuntimeError("HTTP closure names changed")
        if signature(live_route) == _EXPECTED["route_new"] and signature(live_context) == _EXPECTED["context_new"]:
            return {"status": "already_applied", "baseCommit": "193d962"}
        if "codex_progress_layout" in sys.modules:
            raise RuntimeError("Progress layout helper was already loaded before this update")
        old_route, old_context = live_route.__code__, live_context.__code__
        try:
            live_context.__code__ = context.__code__
            live_route.__code__ = route.__code__
        except BaseException:
            live_route.__code__, live_context.__code__ = old_route, old_context
            raise
        return {"status": "applied", "baseCommit": "193d962", "methods": ["progress_context", "do_POST"]}
    finally:
        runtime.lock.release()
