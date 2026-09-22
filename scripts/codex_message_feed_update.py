"""Apply reviewed message feed without interrupting active agents."""
import sys
from pathlib import Path
from types import FunctionType
from codex_active_task_update import signature
from codex_progress_update import source_function
from codex_resource_removal_update import _find_handler

EXPECTED = {'codex_runtime.Runtime.chat_read': ['738b31dbb7ac55854e50344e729fc4293af49929272d756e0503c0458a145d2f', '23b85f324e0eb1c0cd0bb355f541d06636573184eb79ee308fa4ccbcf92c96e2']}

def apply(runtime):
    import codex_runtime
    if sys.version_info[:2] != (3, 14) or type(runtime) is not codex_runtime.Runtime:
        raise RuntimeError("Unknown message feed runtime")
    handler = _find_handler(runtime)
    replacements = []
    directory = Path(__file__).resolve().parent
    for target, allowed in EXPECTED.items():
        module_name, *path = target.split(".")
        module = sys.modules[module_name]
        if Path(module.__file__).resolve().parent != directory:
            raise RuntimeError("Unexpected message feed module: " + module_name)
        desired, static = source_function((directory / (module_name + ".py")).read_text(), path, vars(module))
        if static or signature(desired) != allowed[1]:
            raise RuntimeError("Unreviewed message feed source: " + target)
        owner = handler if path[0] == "make_server" else getattr(module, path[0])
        replacements.append((owner, path[-1], desired, allowed, target))
    if not runtime.lock.acquire(timeout=10):
        raise RuntimeError("Runtime busy; message feed update waits")
    originals = []
    try:
        if runtime.closed:
            raise RuntimeError("Runtime closed")
        for owner, name, desired, allowed, target in replacements:
            live = vars(owner).get(name)
            if live is None:
                if allowed[0] is not None:
                    raise RuntimeError("Missing live chat method: " + target)
            elif (not isinstance(live, FunctionType) or signature(live) not in allowed
                  or live.__globals__ is not desired.__globals__
                  or live.__code__.co_freevars != desired.__code__.co_freevars):
                raise RuntimeError("Unknown live chat method: " + target)
            originals.append((owner, name, live, None if live is None else
                              (live.__code__, live.__defaults__, live.__kwdefaults__)))
        try:
            for (owner, name, live, _), (_, _, desired, _, _) in zip(originals, replacements):
                if live is None:
                    setattr(owner, name, desired)
                else:
                    live.__code__ = desired.__code__
                    live.__defaults__ = desired.__defaults__
                    live.__kwdefaults__ = desired.__kwdefaults__
        except BaseException:
            for owner, name, live, previous in reversed(originals):
                if live is None:
                    if name in vars(owner): delattr(owner, name)
                else:
                    live.__code__, live.__defaults__, live.__kwdefaults__ = previous
            raise
        return {"status": "applied", "methods": list(EXPECTED)}
    finally:
        runtime.lock.release()
