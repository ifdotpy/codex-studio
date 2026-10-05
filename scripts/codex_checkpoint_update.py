"""Move retained checkpoint callbacks outside notification locks, without restart."""
import hashlib
from pathlib import Path
import sys

from codex_source import signature, source_function

SOURCE_HASH = "0fa845cb60e5b7000fbd0c7f62514fbe40d6e3ffbf920dee6ab78a2bd345879a"
RUNTIME_HASHES = frozenset({
    "7e2f7a03acdf9c6a1c8876f022b70c14821ef37a7119f33ed910845e18ba7450",
    "9b7fcf91543d2e469bc8581ec8d4c1cd868afe64a4b035a15fe0f5fd1beac7e7",
})
FUNCTIONS = (
    ("_reserve_checkpoint", "9b53bfc50babb52559acb3291c4eafb0378f50708cbc882ff811b83a72f8aa05", "c865940df0d7b888c352593bf89e66e3bd5cf01e4f6be9c66117815b726463da"),
    ("queue_checkpoint_after_turn", "a95bd7468baa81e67c89688332a6150d8b9960dfd9e63680c3953ba5da7f6e45", "f631ccb1dc14ae7b093d4edb17688a9276f0afd1ec10f1b18df7b6692670e451"),
    ("_checkpoint_completed_agent", None, "d8941e45c4636d877e2eba5f98ef49cd3e1fefeeb95af947820fe95a828ab890"),
    ("_checkpoint_after_committed_turn", None, "27200e4aef1052172900853b4b3a311d7297b47203239720770de82615690b72"),
    ("_capture_reserved_checkpoint", "3492b3823bb3c5277067deb9f74f2de1f97c9eab75f4bad54f3abafc233e8a6f", "6d0a0d20bcbb6b2977b9cdf14fba1e111fa5da4dcd4fdccf866be50a6d0ed895"),
    ("_workspace_idle_snapshot", None, "d0e1e60c5e01905740f0e67fcc6894728b8f00776627fdf3cf6d60b18b40abd8"),
    ("_resolve_workspace_idle", None, "4d224dc83951f84d77b70d066bdd28cc29a83dfac5d373f827cd66e1682ed892"),
    ("_assert_workspace_idle", "2c43d6b7106bdb142aa84338aea7c3b8a0e1bbb386f5123688d80ec964d9c578", "9cc74d6a11fc425e337129856827fc3b310fe807edd1d2e552eef260af722972"),
)
DEPENDENCIES = (
    ("_workspace_source", "affbedbd87c52abf823b92990b035539bc25e96cf1eed88a95ac41cc59b039f9", True),
    ("_assert_workspace_source", "26a39ae515f6d81fd3c31e7a4db68afda4524e149542b3a3e44aef384d5df009", False),
    ("_settle_checkpoint", "8264e8b6aee2fa64e05062762e5cedabdd12f15b6a30037711c6f52dacc9eda2", False),
    ("_workspace_operation", "bb7ffeaa14cde0f7fdc985bbf28c8eda0067e390e8ea9344ea8e1411c1d82e13", False),
    ("_put_workspace_operation", "a024cc1c51e687d8454598c49c5250618365d224a8557b25d6650629d1901e80", False),
    ("checkpoint_after_turn", "2496fa20127c050cf96901774d6abf544c7490fbfac7019ea62bf0fd42324135", False),
)
PHASES = {"provider_pending", "provider_ready", "local_mutation",
          "recovery_required", "capture_pending", "capture_running"}


def apply(runtime):
    scripts = Path(__file__).resolve().parent
    module = sys.modules.get("codex_runtime")
    workspace = sys.modules.get("codex_workspace")
    if (module is None or type(runtime) is not getattr(module, "Runtime", None)
            or runtime.closed or Path(module.__file__).resolve() != scripts / "codex_runtime.py"
            or workspace is None or Path(workspace.__file__).resolve() != scripts / "codex_workspace.py"):
        raise RuntimeError("The running backend source identity differs")
    if hashlib.sha256(Path(module.__file__).read_bytes()).hexdigest() not in RUNTIME_HASHES:
        raise RuntimeError("The running backend source differs")
    path = scripts / "codex_workspace.py"
    raw = path.read_bytes()
    if hashlib.sha256(raw).hexdigest() != SOURCE_HASH:
        raise RuntimeError("The reviewed checkpoint source differs")
    desired = {}
    for name, _, after in FUNCTIONS:
        function, static = source_function(raw, ["WorkspaceMixin", name], vars(workspace), str(path))
        if static or function.__globals__ is not vars(workspace) or signature(function) != after:
            raise RuntimeError("The reviewed checkpoint function differs")
        desired[name] = function
    with runtime.lock:
        if (runtime.closed or sys.modules.get("codex_runtime") is not module
                or sys.modules.get("codex_workspace") is not workspace
                or type(runtime) is not module.Runtime
                or module.Runtime.__module__ != "codex_runtime"):
            raise RuntimeError("The running backend owner differs")
        owner = workspace.WorkspaceMixin
        if (type(owner) is not type or owner.__module__ != "codex_workspace"
                or owner not in module.Runtime.__mro__
                or owner.WORKSPACE_OPERATION_ACTIVE != PHASES
                or workspace.ACTIVE_MONITOR_STATUSES != ("running", "starting", "approval")):
            raise RuntimeError("The running workspace owner differs")
        for name, expected, static in DEPENDENCIES:
            descriptor = owner.__dict__.get(name)
            function = descriptor.__func__ if isinstance(descriptor, staticmethod) else descriptor
            if (isinstance(descriptor, staticmethod) != static
                    or getattr(function, "__globals__", None) is not vars(workspace)
                    or signature(function) != expected):
                raise RuntimeError("The running checkpoint dependency differs")
        changes = []
        additions = []
        for name, before, after in FUNCTIONS:
            current = owner.__dict__.get(name)
            if current is None and before is None:
                if any(name in base.__dict__ for base in module.Runtime.__mro__):
                    raise RuntimeError("The new checkpoint method has another owner")
                additions.append((name, desired[name]))
                continue
            if (getattr(current, "__globals__", None) is not vars(workspace)
                    or signature(current) not in {before, after}):
                raise RuntimeError("The running checkpoint function differs")
            if signature(current) != after:
                changes.append((current, desired[name]))
        # Publish new helpers before any retained function can call them.
        for name, function in additions:
            setattr(owner, name, function)
        for current, wanted in changes:
            current.__defaults__ = wanted.__defaults__
            current.__kwdefaults__ = wanted.__kwdefaults__
            current.__code__ = wanted.__code__
    return {"status": "applied" if changes or additions else "already_applied"}
