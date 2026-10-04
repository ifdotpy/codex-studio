"""Release retained cost readers' history snapshots before the price loop."""
import hashlib
from pathlib import Path
import sys

from codex_source import signature, source_function

SOURCE_HASH = "29faaed1925f7c4c5d5dbdbcd6735afe0ad7a9d5aa5cc4ed19def00bd3b61c05"
RUNTIME_HASHES = frozenset({
    "7e2f7a03acdf9c6a1c8876f022b70c14821ef37a7119f33ed910845e18ba7450",
    "9b7fcf91543d2e469bc8581ec8d4c1cd868afe64a4b035a15fe0f5fd1beac7e7",
})
BEFORE = "d40c0b66bd52dda0e69a99ac7253d7bae87023595f97fa83d84941a033af07f5"
AFTER = "f2c4bb1b078aa826c390fbab1699d88ef70f7f5fe97d80d131fb31f01e16a122"
COMPUTE_BEFORE = "322a724fea282f8f271eaa2cda0ac5241c78cbfbebc1d97d448f2fdbcc2f2b08"
COMPUTE_AFTER = "8fc71ad493091be9df2719cec7b52771b8d81d7ef350ea1612a4a2f3fa8c5146"
CONNECT = "5a47fd3e98b810b445994f0fd2aa3d5a1ddcbd504ef40494be23c882ba32b085"
FUNCTIONS = (("_cost_usage_groups", BEFORE, AFTER),
             ("_compute", COMPUTE_BEFORE, COMPUTE_AFTER))


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
    desired = {}
    for name, _, after in FUNCTIONS:
        function, static = source_function(raw, ["SessionCostReader", name], vars(costs), str(path))
        if static or function.__globals__ is not vars(costs) or signature(function) != after:
            raise RuntimeError("The reviewed cost function differs")
        desired[name] = function
    with runtime.lock:
        if (runtime.closed or sys.modules.get("codex_runtime") is not module
                or sys.modules.get("codex_session_costs") is not costs
                or type(runtime) is not module.Runtime
                or module.Runtime.__module__ != "codex_runtime"):
            raise RuntimeError("The running backend owner differs")
        owner = costs.SessionCostReader
        if not isinstance(owner, type) or owner.__module__ != "codex_session_costs":
            raise RuntimeError("The running cost class owner differs")
        if (getattr(owner._connect, "__globals__", None) is not vars(costs)
                or signature(owner._connect) != CONNECT):
            raise RuntimeError("The running read connection differs")
        changes = []
        for name, before, after in FUNCTIONS:
            current = getattr(owner, name)
            if getattr(current, "__globals__", None) is not vars(costs):
                raise RuntimeError("The running cost function owner differs")
            current_signature = signature(current)
            if current_signature not in {before, after}:
                raise RuntimeError("The running cost function differs")
            if current_signature != after:
                changes.append((current, desired[name]))
        # Validate both functions before changing either retained callback.
        for current, wanted in changes:
            current.__defaults__ = wanted.__defaults__
            current.__kwdefaults__ = wanted.__kwdefaults__
            current.__code__ = wanted.__code__
    return {"status": "applied" if changes else "already_applied"}
