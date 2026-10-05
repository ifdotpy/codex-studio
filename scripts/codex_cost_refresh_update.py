"""Apply exact Claude cache checks and partial tariffs to retained readers."""
import hashlib
from pathlib import Path
import sys

from codex_source import signature, source_function

RUNTIME_HASHES = frozenset({
    "7e2f7a03acdf9c6a1c8876f022b70c14821ef37a7119f33ed910845e18ba7450",
    "9b7fcf91543d2e469bc8581ec8d4c1cd868afe64a4b035a15fe0f5fd1beac7e7",
})
SOURCES = {
    "codex_session_costs": "85d79400703e7856313fceab73577b4438dadeefa7508886ec305d9fd5ac97fe",
    "codex_pricing": "51c4cdc580c427b21d4d728826070fa5e9f83fba5071387d2220ddcc33d8233e",
}
FUNCTIONS = (
    ("codex_session_costs", ("SessionCostReader", "_compute"),
     "29d6e3dba1e8a3bf8de5f6551a6391b35fd93494d8f8e00b6a4ddbc37df8e582",
     "322a724fea282f8f271eaa2cda0ac5241c78cbfbebc1d97d448f2fdbcc2f2b08"),
    ("codex_pricing", ("price_usage",),
     "059943f52519fccce8b8a043e70bc87a40fa8b84333583368c717cf84e31b990",
     "ecc508c8bc4a69684494144ce96a7e42e04818f27dad7c6c1d0bfd865038dc07"),
)
DEPENDENCIES = (
    ("codex_session_costs", ("SessionCostReader", "_cost_usage_groups"),
     "d40c0b66bd52dda0e69a99ac7253d7bae87023595f97fa83d84941a033af07f5"),
    ("codex_session_costs", ("SessionCostReader", "_log_rows"),
     "e2942bffa1659222d1b6e825730fb9e447174e29301f9e78c3ca6c90863a5829"),
    ("codex_session_costs", ("SessionCostReader", "_claude_signature"),
     "7a7a7933118c5e2a5a3e1ba13cf4a8c1e326c84bc4079e7173c0d343f043c508"),
    ("codex_session_costs", ("SessionCostReader", "_load_persisted"),
     "df90acd1af53905dab293a5f32924b975d74cc2f5e2b3d66dd5effd1abb1b9dc"),
    ("codex_session_costs", ("SessionCostReader", "_usage_state"),
     "e75e472beee015302d92264906e4cf541dcd5c4ef80f55f8a034d427b71f14f3"),
    ("codex_pricing", ("lookup",),
     "4fbd2993ed926143a483da281701cf8b51433e008c0f1a28808e0549eefd10e2"),
    ("codex_pricing", ("amount",),
     "dda392f707e84aa9f1cd6791a3d0758cd328abf234fcf380738eac4c175fe775"),
)


def _method(module, path):
    owner = module
    for name in path:
        owner = getattr(owner, name)
    return owner


def apply(runtime):
    scripts = Path(__file__).resolve().parent
    module = sys.modules.get("codex_runtime")
    if (module is None or type(runtime) is not getattr(module, "Runtime", None)
            or runtime.closed or module.Runtime.__module__ != "codex_runtime"
            or Path(module.__file__).resolve() != scripts / "codex_runtime.py"):
        raise RuntimeError("The running backend source identity differs")
    if hashlib.sha256(Path(module.__file__).read_bytes()).hexdigest() not in RUNTIME_HASHES:
        raise RuntimeError("The running backend source differs")
    loaded, desired = {}, {}
    for name, expected in SOURCES.items():
        target = sys.modules.get(name)
        path = scripts / (name + ".py")
        if target is None or Path(target.__file__).resolve() != path:
            raise RuntimeError("The running cost module identity differs")
        raw = path.read_bytes()
        if hashlib.sha256(raw).hexdigest() != expected:
            raise RuntimeError("The reviewed cost source differs: " + name)
        loaded[name] = target
        for entry_name, entry_path, _, after in FUNCTIONS:
            if entry_name != name:
                continue
            function, static = source_function(raw, entry_path, vars(target), str(path))
            if static or signature(function) != after:
                raise RuntimeError("The reviewed cost function differs")
            desired[(name, entry_path)] = function
    with runtime.lock:
        if (runtime.closed or sys.modules.get("codex_runtime") is not module
                or type(runtime) is not module.Runtime):
            raise RuntimeError("The running backend owner differs")
        if any(sys.modules.get(name) is not target for name, target in loaded.items()):
            raise RuntimeError("The running cost module owner differs")
        costs, pricing = loaded["codex_session_costs"], loaded["codex_pricing"]
        if (not isinstance(costs.SessionCostReader, type)
                or costs.SessionCostReader.__module__ != "codex_session_costs"
                or costs.price_usage is not pricing.price_usage):
            raise RuntimeError("The running cost class or pricing alias differs")
        for name, path, expected in DEPENDENCIES:
            function = _method(loaded[name], path)
            if function.__globals__ is not vars(loaded[name]) or signature(function) != expected:
                raise RuntimeError("The running cost guard differs: " + ".".join(path))
        changes = []
        for name, path, before, after in FUNCTIONS:
            current = _method(loaded[name], path)
            if current.__globals__ is not vars(loaded[name]):
                raise RuntimeError("The running cost function owner differs")
            current_signature = signature(current)
            if current_signature not in {before, after}:
                raise RuntimeError("The running cost function differs")
            if current_signature != after:
                changes.append((current, desired[(name, path)]))
        # Publish the compatible tariff fix first. Existing readers and aliases stay intact.
        for current, wanted in reversed(changes):
            current.__defaults__ = wanted.__defaults__
            current.__kwdefaults__ = wanted.__kwdefaults__
            current.__code__ = wanted.__code__
    return {"status": "applied" if changes else "already_applied"}
