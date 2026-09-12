"""Apply the exact per-turn model fix without stopping agents or monitors."""
from pathlib import Path
import sys

from codex_active_task_update import compile_function, signature

EXPECTED = {
    "prepared_result": (
        "cfbbc59ae01a44d5649dfeac4614fdc3d534362ce2328065f12388d95930ed8b",
        "e59baed43b656e27bd3a9d1987f1879d4603bd41eff9a0c7273c9711733a450d"),
    "start": (
        "fdd17c31c9c6790b4e8bd6e1bbb6dd39d5d2ca685ac44756f1c3da8b703adcba",
        "7f4fae6c692137efaee1bfa5ec93dbe832cea00526c235ebcaf84ab426671552"),
    "run_native_action": (
        "ba0fb957048e2d0466f649fecfa0fcbfb3b080d8ce51fc1e9edbf28f9cb0032a",
        "9756e74ea13ba1d381f540bd52ab249149a4a6cfb290868f4b372b9ef67e85e0"),
}


def apply(runtime):
    import codex_runtime
    if sys.version_info[:2] != (3, 14) or type(runtime) is not codex_runtime.Runtime:
        raise RuntimeError("Unknown runtime for the per-turn model update")
    source = Path(__file__).resolve().with_name("codex_runtime.py")
    if Path(codex_runtime.__file__).resolve() != source:
        raise RuntimeError("Unexpected live runtime source")
    desired = {}
    for name, (_, wanted) in EXPECTED.items():
        replacement = compile_function(source.read_text(), "Runtime", name, vars(codex_runtime), str(source))
        if signature(replacement) != wanted:
            raise RuntimeError("Unreviewed model replacement: " + name)
        desired[name] = replacement
    if not runtime.lock.acquire(timeout=10):
        raise RuntimeError("Runtime remains busy; no model update applied")
    originals = {}
    try:
        if runtime.closed:
            raise RuntimeError("Runtime is closed; no model update applied")
        for name, allowed in EXPECTED.items():
            if name in vars(runtime):
                raise RuntimeError("Instance model method override: " + name)
            live = vars(codex_runtime.Runtime).get(name)
            if (signature(live) not in allowed or live.__globals__ is not vars(codex_runtime)
                    or live.__code__.co_freevars != desired[name].__code__.co_freevars):
                raise RuntimeError("Unknown live model method: " + name)
            originals[name] = (live, live.__code__)
        if all(signature(live) == EXPECTED[name][1] for name, (live, _) in originals.items()):
            return {"status": "already_applied", "baseCommit": "d6cc3e1"}
        try:
            for name, (live, _) in originals.items():
                live.__code__ = desired[name].__code__
        except BaseException:
            for live, code in originals.values():
                live.__code__ = code
            raise
        return {"status": "applied", "baseCommit": "d6cc3e1", "methods": list(originals)}
    finally:
        runtime.lock.release()
