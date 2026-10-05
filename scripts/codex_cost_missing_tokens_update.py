"""Apply the nullable token fix without replacing existing cost readers."""
import hashlib
from pathlib import Path
import sys

from codex_source import signature, source_function

SOURCE_HASH = "c43da6ffc88f016b9fbc93c06ccf2c6d9ea34034e56f5258aba940bb7e03621b"
RUNTIME_HASHES = frozenset({
    "7e2f7a03acdf9c6a1c8876f022b70c14821ef37a7119f33ed910845e18ba7450",
    "9b7fcf91543d2e469bc8581ec8d4c1cd868afe64a4b035a15fe0f5fd1beac7e7",
})
BEFORE = "16d198433626fa2202cab3e2c17ddebbe58ae8c1412af004e24d0357d4b19fef"
AFTER = "29d6e3dba1e8a3bf8de5f6551a6391b35fd93494d8f8e00b6a4ddbc37df8e582"
GROUPS = "d40c0b66bd52dda0e69a99ac7253d7bae87023595f97fa83d84941a033af07f5"


def apply(runtime):
    scripts = Path(__file__).resolve().parent
    module = sys.modules.get("codex_runtime")
    costs = sys.modules.get("codex_session_costs")
    if (module is None or type(runtime) is not getattr(module, "Runtime", None)
            or runtime.closed
            or Path(module.__file__).resolve() != scripts / "codex_runtime.py"
            or costs is None
            or Path(costs.__file__).resolve() != scripts / "codex_session_costs.py"):
        raise RuntimeError("The running backend source identity differs")
    if hashlib.sha256(Path(module.__file__).read_bytes()).hexdigest() not in RUNTIME_HASHES:
        raise RuntimeError("The running backend source differs")
    path = scripts / "codex_session_costs.py"
    raw = path.read_bytes()
    if hashlib.sha256(raw).hexdigest() != SOURCE_HASH:
        raise RuntimeError("The reviewed cost source differs")
    desired, static = source_function(raw, ["SessionCostReader", "_compute"], vars(costs), str(path))
    if static or desired.__globals__ is not vars(costs) or signature(desired) != AFTER:
        raise RuntimeError("The reviewed cost function differs")
    with runtime.lock:
        if runtime.closed:
            raise RuntimeError("The running backend is closed")
        if (sys.modules.get("codex_runtime") is not module
                or sys.modules.get("codex_session_costs") is not costs):
            raise RuntimeError("The running module identity differs")
        if (type(runtime) is not module.Runtime
                or module.Runtime.__module__ != "codex_runtime"):
            raise RuntimeError("The running backend owner differs")
        owner = costs.SessionCostReader
        if not isinstance(owner, type) or owner.__module__ != "codex_session_costs":
            raise RuntimeError("The running cost class owner differs")
        current = owner._compute
        groups = owner._cost_usage_groups
        if (getattr(current, "__globals__", None) is not vars(costs)
                or getattr(groups, "__globals__", None) is not vars(costs)):
            raise RuntimeError("The running cost function owner differs")
        if signature(groups) != GROUPS:
            raise RuntimeError("The running cost guard differs")
        current_signature = signature(current)
        if current_signature not in {BEFORE, AFTER}:
            raise RuntimeError("The running cost function differs")
        if current_signature == AFTER:
            return {"status": "already_applied"}
        current.__code__ = desired.__code__
        current.__defaults__ = desired.__defaults__
        current.__kwdefaults__ = desired.__kwdefaults__
    return {"status": "applied"}
