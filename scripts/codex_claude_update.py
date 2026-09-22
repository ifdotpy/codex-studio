"""Add Claude Code for new sessions without stopping existing Codex connections."""
import hashlib
import sys
from pathlib import Path
from types import FunctionType
from codex_active_task_update import signature
from codex_progress_update import source_function

EXPECTED = {'codex_runtime.AppServer.__init__': ['3f8ee5f36c276a94539783f5eb9436f4d1e16ab87537a55879a62d333e0554a6', 'db501ad9f19f742f958e0cc4888e3ecb6d98d464ba424f94ea4716f4c4a966f0'], 'codex_runtime.Runtime.connect': ['f1c8189a94cb7d78392b699787fb30abcc99b2478f6ee27a32d03a3b2ae5405f', 'a36f580c23835d0dc226d2992f3d44cb76a22e53c6cc42541f04d575942ba393'], 'codex_runtime.Runtime.worker_defaults': ['373f2ce0ade222f141921ec36300e51328aa6d6f5e24cd672390a213505c4c46', '460892b9732b55c30548139f099a067c51309728bfe4e85e8070d3396050f2e8'], 'codex_runtime.Runtime.create': ['39db08d6f272f7fa0664031f3fc69a03ce127439382d639895f00a8d064f5a74', '728cfc7cc9750bbf6e5f7ff7be951157123eda36a7d7dc89fca47150621e2e03'], 'codex_runtime.Runtime.new_lead': ['9fc1d508ab9e362bc8a3caa4cf15e3f515223a30c55f06e281c4cde4b4eea033', 'ae96c1163ed0c19a77c8fd567edb6aed998a686fb5ead8a55fb315c32bf5e50e'], 'codex_runtime.Runtime.set_account': ['154a1e8791415f2f02db899d5284dbebf362476d990897bbc9c9f9e11699d275', '20316d4d71c075a5013b5e4ca28bff02fbe2ffbe064f17102d602df69e9465bb'], 'codex_runtime.Runtime.conversation_settings': ['c802fb58c3e7c2f0087dfd89bbfbda27bb2bb005358bda1a1f73ba35bb813580', '8aa682083aba221acf6a7cda58b9fe1f3a4c66d7521ce1ae6840da6e556690d8'], 'codex_runtime.Runtime.send': ['fc88db151db647549f1d899ef884dddf943dda9a51e2ac7b3f6b18e1e9604f24', '04eccb45271a3a3d8d355b3d5fa28d01e66f230e8e843c7f7b5f0491bdca1e0a'], 'codex_runtime.Runtime.tool_definitions': ['1d4a376da2349fdfb6c4260344c5abae990e3fe094ba321380a5a1a08f5a0ed3', '61c6477e3e232adfc145eabc8a4893f69ab608330cc80d02e00915146bec294e'], 'codex_runtime.Runtime.new_thread_params': ['e162c895e862bd4a9befa87d3fc9d4e09fc3b1593883bb22474ce21b3b07c607', 'd4186afae1407e76eb1cabc5ec39fb32de810ba00f3cb4728bc751f9882431c7'], 'codex_runtime.Runtime.limits': ['556202351ffde94f4c535216da367aac59a887c778f9fab5b639bd78582d8cc2', 'd40048f79288684ae82ffff8702a83538b128a93d4b5c24c2032f6cb2a2f32dc'], 'codex_accounts.AccountStore.refresh': ['95d785a02585a78249d3ff6e2f41e0f81278046dcb5ae0a15668c51bc64557f8', '65609a7704a4477fe678e45cb7ac9d0f5ae78db743c8373aae813487c3001065'], 'codex_accounts.AccountStore.discover': ['81275d4a00380fea138772fc75f6ea5e8aa17c20de88f2fd605f832026ff6c31', '1511b716732c3ab17411ada7b92304a04b1bff9fc5f7869608be908fd6bb55b5'], 'codex_account_transfer.AccountTransfers.check_destination': ['6dce47803852b1ee2f7eefc678121e99cc222d6545d4ce7de22df96fc80fee18', '935e54abb3004ab11ee3ddc43a48f01cbae5a8f85bfde6d4340c7c0352bd2ca4'], 'codex_claude.transport': ['95f7590ed9499a1724a614be2304e51a2bd37f14e3fa65ae65b6e8cbbe172edb', 'ef0be5ad1e88c84f19af9e80c46be7da47c41e25a57b23e70ee57c35b5b213c2']}
BRIDGE = {'bridge.mjs': '7f0b41e49fdd6b611e53152ea3d987e0e8db607a9c7ecbce82638ad1f22e04d7', 'package.json': 'b16f3c40492c75a0b564a87db121442b4fa3c42609b91d54d11f5be7bb9285f8', 'package-lock.json': '4c49940618ae1e94e846ba59f7813ce5d118cdbc679bb72bcc8e4c79519d306a'}


def apply(runtime):
    import codex_runtime
    import codex_accounts
    import codex_account_transfer
    import codex_claude
    if sys.version_info[:2] != (3, 14) or type(runtime) is not codex_runtime.Runtime:
        raise RuntimeError("Unknown Claude provider runtime")
    directory = Path(__file__).resolve().parent
    for name, digest in BRIDGE.items():
        if hashlib.sha256((directory / 'claude_bridge' / name).read_bytes()).hexdigest() != digest:
            raise RuntimeError("Unreviewed Claude bridge source: " + name)
    replacements = []
    for target, allowed in EXPECTED.items():
        module_name, *path = target.split(".")
        name = path[-1]
        module = sys.modules[module_name]
        if Path(module.__file__).resolve().parent != directory:
            raise RuntimeError("Unexpected provider module: " + module_name)
        desired, static = source_function((directory / (module_name + ".py")).read_text(), path, vars(module))
        if signature(desired) != allowed[1]:
            raise RuntimeError("Unreviewed provider source: " + target)
        owner = getattr(module, path[0]) if len(path) == 2 else module
        replacements.append((owner, name, desired, static, allowed, target))
    if not runtime.lock.acquire(timeout=10):
        raise RuntimeError("Runtime busy; Claude update waits")
    originals = []
    try:
        if runtime.closed:
            raise RuntimeError("Runtime closed")
        for owner, name, desired, static, allowed, target in replacements:
            descriptor = vars(owner).get(name)
            if isinstance(descriptor, staticmethod) != static:
                raise RuntimeError("Unknown provider descriptor: " + target)
            live = descriptor.__func__ if static else descriptor
            if (not isinstance(live, FunctionType) or signature(live) not in allowed
                    or live.__globals__ is not desired.__globals__
                    or live.__code__.co_freevars != desired.__code__.co_freevars):
                raise RuntimeError("Unknown live provider method: " + target)
            originals.append((live, live.__code__, live.__defaults__, live.__kwdefaults__))
        try:
            for (live, *_), (_, _, desired, *_rest) in zip(originals, replacements):
                live.__code__ = desired.__code__
                live.__defaults__ = desired.__defaults__
                live.__kwdefaults__ = desired.__kwdefaults__
        except BaseException:
            for live, code, defaults, kwdefaults in reversed(originals):
                live.__code__, live.__defaults__, live.__kwdefaults__ = code, defaults, kwdefaults
            raise
        runtime.accounts.discovered = False
        return {"status": "applied", "methods": list(EXPECTED)}
    finally:
        runtime.lock.release()
