"""Update only reviewed workspace controls while preserving retained callbacks."""
import hashlib
from pathlib import Path
import sys
from types import ModuleType

from codex_source import signature, source_function

RUNTIME_HASHES = frozenset({'7e2f7a03acdf9c6a1c8876f022b70c14821ef37a7119f33ed910845e18ba7450',
           '9b7fcf91543d2e469bc8581ec8d4c1cd868afe64a4b035a15fe0f5fd1beac7e7'})
WORKSPACE_HASH = 'c7b12ccd601564f1e298ccfbb2c3f7daabd9373d89aa7c6898e16893e3eda142'
CLAUDE_HASH = 'aab6888dbd4822c4a0d1e9127442f4fc5088e0ac4fb9fa164ddecfaa07516063'
FUNCTIONS = (('checkpoint_capture',
  '47c69f09e7829725888f96a1f9d71f7e9861cf42ded2ed85375ed25371749b23',
  '6f0311115552e994102f35c5f60229bb4e01357153de63a30e9860fb172fd9fb'),
 ('_restore_checkpoint_locked',
  'f0a9bc41945c36a09ace2948494a6bf4e140f9fbefc43bbe9cbc30408713d078',
  'b3236cd80eb63af949dd8a2c9931d203c97f5464ac58f46e4ba763ceb2383315'))
CLAUDE_FUNCTION = ('action',
 '4deb65d370af717f6bf92e35c92d85f72e60a50da3504ec9fd754927adf4c599',
 '3a9b2e9f23395df8e7c104330691fdafa3769a1a3279579db340c8e769d0f204')
WORKSPACE_DEPENDENCIES = (('_assert_workspace_idle', '9cc74d6a11fc425e337129856827fc3b310fe807edd1d2e552eef260af722972', 'function'),
 ('_assert_workspace_source', '26a39ae515f6d81fd3c31e7a4db68afda4524e149542b3a3e44aef384d5df009', 'function'),
 ('_capture_reserved_checkpoint',
  '6d0a0d20bcbb6b2977b9cdf14fba1e111fa5da4dcd4fdccf866be50a6d0ed895',
  'function'),
 ('_checkpoint_after_committed_turn',
  '27200e4aef1052172900853b4b3a311d7297b47203239720770de82615690b72',
  'function'),
 ('_checkpoint_completed_agent',
  'd8941e45c4636d877e2eba5f98ef49cd3e1fefeeb95af947820fe95a828ab890',
  'function'),
 ('_checkpoint_history_ids', '76907044c36bac3e4e5f387bd1c7228438aa0b2549e98fc88fd5b466e986db35', 'function'),
 ('_finish_workspace_operation',
  'c88e3cfc1def2f71fcfb65d89eb3b1aefdfd5b50dcb0279ac728653b756e0d50',
  'function'),
 ('_put_workspace_operation', 'a024cc1c51e687d8454598c49c5250618365d224a8557b25d6650629d1901e80', 'function'),
 ('_require_workspace_recovery',
  '93da720f0a459459df21c41964154efc8f2cbdf9d097319fd61de722403cc287',
  'function'),
 ('_reserve_checkpoint', 'c865940df0d7b888c352593bf89e66e3bd5cf01e4f6be9c66117815b726463da', 'function'),
 ('_resolve_workspace_idle', '4d224dc83951f84d77b70d066bdd28cc29a83dfac5d373f827cd66e1682ed892', 'function'),
 ('_settle_checkpoint', '8264e8b6aee2fa64e05062762e5cedabdd12f15b6a30037711c6f52dacc9eda2', 'function'),
 ('_update_workspace_operation',
  '020f08977afadb74179bcb2ca40793fe1277598402d41b1b55b4709fcfb9b985',
  'function'),
 ('_workspace_idle_snapshot', 'd0e1e60c5e01905740f0e67fcc6894728b8f00776627fdf3cf6d60b18b40abd8', 'function'),
 ('_workspace_operation', 'bb7ffeaa14cde0f7fdc985bbf28c8eda0067e390e8ea9344ea8e1411c1d82e13', 'function'),
 ('_workspace_operation_id', '0a93b5c7f3d4f574077e8cc89e58f38cbcfaf08e0f5bae72e3165524c6156309', 'static'),
 ('_workspace_operation_signature',
  'eb8cfa3a52a4cc7df77503891fb66452dc01efbfe6f6c360980cc15971af6f9c',
  'static'),
 ('_workspace_operations', '03fe3e88cd1240f9bfa0517205b7e3bc691ae89f2b2c49d00147954b80b503cb', 'function'),
 ('_workspace_provider_rejected',
  'ac872696c34ec15453f0e6145079420705894057223c7900daee7da19af38298',
  'function'),
 ('_workspace_provider_result', '92293bfdeb21d5dd840b0f7be4bbbba3aee8500c11634949e526e252c15cb7c0', 'static'),
 ('_workspace_restore_signature',
  'eaaac7e5f1ce155923cab6dbeee7bdd6e89d6a8592d0c7706a0ad1fb6ab99cc2',
  'class'),
 ('_workspace_source', 'affbedbd87c52abf823b92990b035539bc25e96cf1eed88a95ac41cc59b039f9', 'static'),
 ('capture_checkpoint', '838d94dbf22a49470fca2fb8a382e0fcbf040bd2d52d2090850a9fb89f4d730b', 'function'),
 ('checkpoint_after_turn', '2496fa20127c050cf96901774d6abf544c7490fbfac7019ea62bf0fd42324135', 'function'),
 ('git', '87eedb8aec943138e71cb67c6ffa05436debc3fab64e18ff1b098db4738b8db4', 'function'),
 ('queue_checkpoint_after_turn',
  'f631ccb1dc14ae7b093d4edb17688a9276f0afd1ec10f1b18df7b6692670e451',
  'function'),
 ('snapshot_tree', 'de09e098b1f25561e2b236f6c1ea3342e7697f3a7facb4bf3e4d21bfa68734a9', 'function'))
CLAUDE_DEPENDENCIES = (('_actor', 'dff42f6e053fb77e52906fa78b09b03187fa12c954eb5b448f77d4f2f30f9733'),
 ('_settings', 'c20b17ad4a68197db242b9c00872b60de06c9299c07b9c22d46c0c3c52832ade'),
 ('_pending', 'd7b9596d85928c0677a907d51fcabadc074bbcc9f698331c06a4b53d3843323a'))
PHASES = frozenset({'capture_pending',
           'capture_running',
           'local_mutation',
           'provider_pending',
           'provider_ready',
           'recovery_required'})
MODES = frozenset({'default', 'acceptEdits', 'auto', 'plan', 'bypassPermissions'})


def _identity(runtime, scripts, module, workspace, claude):
    if (runtime.closed or type(module) is not ModuleType
            or type(workspace) is not ModuleType or type(claude) is not ModuleType
            or sys.modules.get("codex_runtime") is not module
            or sys.modules.get("codex_workspace") is not workspace
            or sys.modules.get("codex_claude_controls") is not claude
            or type(runtime) is not getattr(module, "Runtime", None)
            or module.Runtime.__module__ != "codex_runtime"
            or getattr(module.Runtime.__dict__.get("__init__"), "__globals__", None) is not vars(module)
            or any(name in runtime.__dict__ for name, *_pin in FUNCTIONS + WORKSPACE_DEPENDENCIES)):
        raise RuntimeError("The running control owner differs")
    for current, name in ((module, "codex_runtime"), (workspace, "codex_workspace"),
                          (claude, "codex_claude_controls")):
        if (current.__name__ != name or not isinstance(getattr(current, "__file__", None), str)
                or Path(current.__file__).resolve() != scripts / (name + ".py")):
            raise RuntimeError("The running control source identity differs")
    expected = ((module, RUNTIME_HASHES), (workspace, {WORKSPACE_HASH}), (claude, {CLAUDE_HASH}))
    for current, accepted in expected:
        if hashlib.sha256(Path(current.__file__).read_bytes()).hexdigest() not in accepted:
            raise RuntimeError("The reviewed control source differs")


def _workspace_function(owner, runtime_class, workspace, name, mode):
    first_owner = next((base for base in runtime_class.__mro__ if name in base.__dict__), None)
    if first_owner is not owner:
        raise RuntimeError("The running workspace method owner differs")
    descriptor = owner.__dict__.get(name)
    actual = ("static" if isinstance(descriptor, staticmethod)
              else "class" if isinstance(descriptor, classmethod) else "function")
    function = descriptor.__func__ if actual != "function" else descriptor
    if actual != mode or getattr(function, "__globals__", None) is not vars(workspace):
        raise RuntimeError("The running workspace function globals differ")
    return function


def apply(runtime):
    scripts = Path(__file__).resolve().parent
    module = sys.modules.get("codex_runtime")
    workspace = sys.modules.get("codex_workspace")
    claude = sys.modules.get("codex_claude_controls")
    _identity(runtime, scripts, module, workspace, claude)
    desired = {}
    for name, _before, after in FUNCTIONS:
        wanted, static = source_function(Path(workspace.__file__).read_bytes(),
                                         ["WorkspaceMixin", name], vars(workspace), workspace.__file__)
        if static or wanted.__globals__ is not vars(workspace) or signature(wanted) != after:
            raise RuntimeError("The reviewed workspace control function differs")
        desired[name] = wanted
    name, _before, after = CLAUDE_FUNCTION
    wanted, static = source_function(Path(claude.__file__).read_bytes(), [name], vars(claude), claude.__file__)
    if static or wanted.__globals__ is not vars(claude) or signature(wanted) != after:
        raise RuntimeError("The reviewed Claude control function differs")
    desired_claude = wanted
    with runtime.lock:
        # Recheck files and owners after waiting for the existing operation lock.
        _identity(runtime, scripts, module, workspace, claude)
        owner = workspace.WorkspaceMixin
        if (type(owner) is not type or owner.__module__ != "codex_workspace"
                or owner not in module.Runtime.__mro__ or owner.WORKSPACE_OPERATION_ACTIVE != PHASES
                or workspace.ACTIVE_MONITOR_STATUSES != ("running", "starting", "approval")
                or claude._MODES != MODES):
            raise RuntimeError("The running workspace control contract differs")
        for name, expected, mode in WORKSPACE_DEPENDENCIES:
            function = _workspace_function(owner, module.Runtime, workspace, name, mode)
            if signature(function) != expected:
                raise RuntimeError("The running workspace control dependency differs")
        for name, expected in CLAUDE_DEPENDENCIES:
            function = vars(claude).get(name)
            if getattr(function, "__globals__", None) is not vars(claude) or signature(function) != expected:
                raise RuntimeError("The running Claude control dependency differs")
        changes = []
        for name, before, after in FUNCTIONS:
            current = _workspace_function(owner, module.Runtime, workspace, name, "function")
            actual = signature(current)
            if actual not in {before, after}:
                raise RuntimeError("The running workspace control function differs")
            if actual != after:
                changes.append((current, desired[name]))
        name, before, after = CLAUDE_FUNCTION
        current = vars(claude).get(name)
        if getattr(current, "__globals__", None) is not vars(claude):
            raise RuntimeError("The running Claude control function globals differ")
        actual = signature(current)
        if actual not in {before, after}:
            raise RuntimeError("The running Claude control function differs")
        if actual != after:
            changes.append((current, desired_claude))
        # All targets and dependencies pass before the first edit.
        for current, wanted in changes:
            current.__defaults__ = wanted.__defaults__
            current.__kwdefaults__ = wanted.__kwdefaults__
            current.__code__ = wanted.__code__
    return {"status": "applied" if changes else "already_applied"}
