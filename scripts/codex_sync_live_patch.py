"""Live incremental entity and transcript sync patch"""
import hashlib
import importlib
import json
from pathlib import Path
import sys

import ast
from types import CodeType, FunctionType

from codex_source import signature


def _literal(value):
    # Constant arithmetic defaults such as 1024 * 1024; names and calls stay rejected.
    for node in ast.walk(value):
        if not isinstance(node, (ast.Constant, ast.BinOp, ast.UnaryOp, ast.operator, ast.unaryop,
                                 ast.Tuple, ast.List, ast.Dict, ast.Set, ast.expr_context)):
            raise ValueError("Unsupported default")
    return eval(compile(ast.Expression(value), "<default>", "eval"), {"__builtins__": {}})


def source_function(source, path, namespace, filename='<source>', closure=None,
                    allow_contextmanager=False):
    """Extract code without executing imports, decorators, defaults, or classes."""
    tree = ast.parse(source)
    node = tree
    compiled = compile(tree, filename, 'exec', dont_inherit=True)
    for name in path:
        nodes = [child for child in node.body
                 if isinstance(child, (ast.ClassDef, ast.FunctionDef)) and child.name == name]
        codes = [child for child in compiled.co_consts
                 if isinstance(child, CodeType) and child.co_name == name]
        if len(nodes) != 1 or len(codes) != 1:
            raise RuntimeError('Unknown source structure')
        node, compiled = nodes[0], codes[0]
    if not isinstance(node, ast.FunctionDef):
        raise RuntimeError('Expected a source function')
    decorators = node.decorator_list
    static = len(decorators) == 1 and isinstance(decorators[0], ast.Name) and decorators[0].id == 'staticmethod'
    manager = (allow_contextmanager and len(decorators) == 1 and
               isinstance(decorators[0], ast.Name) and decorators[0].id == 'contextmanager')
    if decorators and not static and not manager:
        raise RuntimeError('Unknown source function decorator')
    defaults = tuple(_literal(value) for value in node.args.defaults) or None
    if closure is None and compiled.co_freevars:
        # Only signature inspection uses empty cells; installed HTTP code keeps its live cells.
        closure = tuple((lambda item: lambda: item)(None).__closure__[0]
                        for _ in compiled.co_freevars)
    function = FunctionType(compiled, namespace, node.name, defaults, closure)
    function.__kwdefaults__ = {arg.arg: _literal(value)
                              for arg, value in zip(node.args.kwonlyargs, node.args.kw_defaults)
                              if value is not None} or None
    return function, static

HANDLER = json.loads(r"""{"sha": "90c97f7b31d5497b17c0bcfb1b782c8ecc77bd30c354a1d0855bde07d7bfdbce", "methods": {"send": ["bf24b2c10e4625a8476dcfd5820dcbf3f38bcc507626a6ad239b4704c38cb182", "7337fcb04da5eb2a4ed18bb35852480b99c268bec925ccb7fdf9e7e46406f4ca"], "stream_transcript": ["6000e5b3108c9f72667ee5346a2d22ac1f6a20935ff47e520876edceb285f384", "93edd59ff223f5e668a48274abebede62273779559fe27312a6cd4470facd62e"], "stream_sync": ["55c84e92db5473a3906a48f3639147315bf0fe51ca5a9d35cc04ec29cf1cb95c", "4284990a6e3a9de16d5b17b442cbe994d42e28a03bfa16883f45aa37cca259dd"], "do_POST": ["d3e41b54bdeb5ddcd8da7fc68c99e40cac4eb10701cea0ffef288b8ad521cde2", "9ddef90880e36899b1b36e26e9f8600e7f0cdd5358b4f8cca39b150b2f930ff5"]}}""")
MANIFEST = json.loads(r"""{"changed": {"codex_budget|_save": ["502ba624732067a04d79eaeecdcccbd9b1c6b6a5e5c837de3945b7e62fc5fc8f", "e204584bccfd5991ca26ea2a41801f0dd45a4ce284b73515b95fe4ef8bdfaa6f"], "codex_canvas|Canvas|connect": ["7b6116a481e1f42267168c27e92047d4685c8e097c23f2b2a86c8176cd2d8b1a", "60ee8eae8c933e0b1ab874fe8b941886dfe7dd18a251ca8ec093c223239a89fd"], "codex_canvas|Canvas|register_agent": ["3a703028557d61ff47b86a3d3e5834c83a4e4970754ef7a1f84f63d7b5043b22", "7173f25913fd7c4cc1169f7b89f093e6a0e06a8f8c1236d8b30822e10b192cd7"], "codex_canvas|Canvas|connect_chat": ["e875aff15e452eef0a52a4fcb6146cdbd4ae34e0754f280da031fea366004589", "ab33d933a3ce21d3306ce94bff5ff8090d2aa81891aa0b914435ae5b8d45cbaa"], "codex_canvas|Canvas|create_chat": ["4d5c64638d9bdaeb718f29cbaae4f11d53b8cdd48eda0fcd0850a629e4130df4", "b5f7e84aed5d8af51261bec42974192126db5ab6fdc92ef8308ee7eec6359a96"], "codex_canvas|Canvas|post": ["d43d09f06637dd975d78b09e4eca10f55535cb9dd46106b22215192405f40fd8", "42882afcfc71b884f7c880037f2ffaf6cc343cf18698ee3c38257f0eb7299517"], "codex_runtime|Runtime|chat_message": ["37b5d5d30e7b79e13932fbcb59ea541b96cdc2898f5c9d2e1cb3aab758a29a67", "3ae037a75c561207bf5d5405494a9b12ed8bfab4e2be7766e15649112b7e9009"], "codex_runtime|Runtime|db": ["bb18f49d4f5030c94c467bf0ed9900b73c346f7ef956e5d5343aa5f3205a4e30", "ae44741f1d6c25ffe29cdaa40701e782413aa64339b6ff99f44b031b9128ee76"], "codex_runtime|Runtime|records": ["ebe6e9406871c84d207882351694da88a93351c5e1319f28edff78348aa3d348", "d81db98a7ae69fd62e353e40c8051d36f1d837ee47d27bc0a9bf2fc0e8c55711"], "codex_runtime|Runtime|put": ["25edc1858bc381c043824b8fba42d4a0fd76526dae86b126ae671b8d39ad8c9e", "f88df6ff486b92fe1285cb73e1f9205534798c9d7e9c986208a8b2df720b64bd"], "codex_rules|RulesMixin|rules_action": ["6ae6a9fc1d855cc6335a568b5a45910d3f4b34a6485ad2c501d4293809a656f1", "1691b58ac6cfd66ec65c526b7a40550b81f0fda78227ddae8d3737120fb5fee2"], "codex_sync|SyncStore|_ensure_versions": ["87aef031c8ea3f415ba64163abfc7a7712e6cafcfa143e7fda08e7009d968b63", "4b8749d3e4f1e2d070a96730bd7922c24251fda1d9f69ebf7d91aebdb89a402b"], "codex_sync|SyncStore|_put": ["9df7f5656b6b44edbd0ed28fa6b6c4970e4959ec9c2f456c66122d680ccadade", "da54a97a97b638306cd4e9b1d618d838949333952c6d7fbbadbf505ec7abee93"], "codex_sync|SyncStore|pull": ["0394f11177d568f0249ff01354d1da3cedba8bd1dc021f134c31f2c4c6360d6c", "c5ef3bd6266e1af4286b318f0ecbdcf315ea12cd33b94d9a1ff6a1a0050f63f5"], "codex_workspace|WorkspaceMixin|projects": ["ebaf928c645533fe883626a583f3c237206d7255f15830bc2a53bac72cae59e6", "04f2ee36f6acf76be0861283294ce014ae939b2986ffa60ca144e0196a853f2d"]}, "added": {"codex_canvas|Canvas|_sync_chat_entity": "f5c4377fae96b6eaa62dfe81a3804ca7da7aaabce1e6f067cea9764d06f535a2", "codex_runtime|Runtime|invalidate_agent_records": "2e33c1137b9962c5ab65acab25315de8f9ba9fb30e819ffdef3b2a07d3611a77", "codex_runtime|Runtime|mark_agent_records_changed": "50d327482152541b509898f6b755cf1db05e5bf4d2d287831a62a6634f2bb3fb", "codex_sync|SyncStore|entity_sequence": "88a1bf7cf0e43928e001d3a0ee0c686574f477170005837919696a23bdc67a69", "codex_sync|SyncStore|transcript_pull": "015aca68f07094b099677ca2fa46ec1eea30c87de66a9c07cd50649047e7e135"}, "shas": {"codex_budget": "ca68a2ec862bbbbdd50c5ec91f87cdfb9ce3fe343c8d8af67e88dcf1c70f4e35", "codex_canvas": "90c97f7b31d5497b17c0bcfb1b782c8ecc77bd30c354a1d0855bde07d7bfdbce", "codex_runtime": "564139e68b28105d0338279b9680ba5e29d89b2054e0b2d1e94670bfc7b92654", "codex_rules": "8bd6a47d50c1ad3b149f724b0267034ff82fd9fba77a4779a62aa3cad8034698", "codex_sync": "cafdf7693224f2cf022d07cfcf8e7766133edeaf505aa9311abed244a6cdb1a8", "codex_workspace": "b42926b4ffbc7f58b8a48ad82fd2fef189617a570faea9753e27b4e580665d6c"}, "globals": []}""")


def _module(name, scripts):
    module = sys.modules.get(name)
    if module is None or Path(getattr(module, "__file__", "")).resolve().parent != scripts:
        raise RuntimeError("Unknown loaded module: " + name)
    raw = (scripts / (name + ".py")).read_bytes()
    if hashlib.sha256(raw).hexdigest() != MANIFEST["shas"][name]:
        raise RuntimeError("Unreviewed source: " + name)
    return module, raw


def _is_manager(raw, path):
    import ast
    node = ast.parse(raw)
    for name in path:
        node = next(n for n in node.body if isinstance(n, (ast.ClassDef, ast.FunctionDef)) and n.name == name)
    return any(isinstance(d, ast.Name) and d.id == "contextmanager" for d in node.decorator_list)


def _owner(module, path):
    return vars(module) if len(path) == 1 else getattr(module, path[0]).__dict__


def apply(runtime):
    scripts = Path(__file__).resolve().parent
    modules = {name: _module(name, scripts) for name in MANIFEST["shas"]}
    cls = sys.modules["codex_runtime"].Runtime
    work, adds = [], []
    for key, (base, new) in MANIFEST["changed"].items():
        name, *path = key.split("|")
        module, raw = modules[name]
        live = _owner(module, path)[path[-1]]
        # @contextmanager keeps the generator function in __wrapped__ and calls it by reference.
        live = getattr(live, "__wrapped__", live)
        own = (cls.__dict__.get(path[-1]) if len(path) == 2 and path[0] != "Runtime"
               and getattr(module, path[0]) in cls.__mro__ else None)
        # The live Runtime may hold the same mixin function, or the usage-resume tick wrapper.
        if own is not None and own is not live and not getattr(own, "_usage_resume_bridge", False):
            raise RuntimeError("Runtime has its own " + path[-1])
        current = signature(live)
        if current == new:
            continue
        if current != base:
            raise RuntimeError("Unknown loaded function: " + key)
        desired, static = source_function(raw, tuple(path), vars(module), closure=live.__closure__,
                                          allow_contextmanager=True)
        # A staticmethod keeps its function in __wrapped__; swap that function's code.
        if static and not isinstance(_owner(module, path)[path[-1]], staticmethod):
            raise RuntimeError("Reviewed source differs for " + key)
        if signature(desired) != new:
            raise RuntimeError("Reviewed source differs for " + key)
        work.append((live, desired))
    for key, expected in MANIFEST["added"].items():
        name, *path = key.split("|")
        module, raw = modules[name]
        live = _owner(module, path).get(path[-1])
        if isinstance(live, staticmethod):
            live = live.__func__
        live = getattr(live, "__wrapped__", live) if live is not None else None
        if live is not None:
            if signature(live) != expected:
                raise RuntimeError("Unknown loaded function: " + key)
            continue
        fn, static = source_function(raw, tuple(path), vars(module), allow_contextmanager=True)
        if signature(fn) != expected:
            raise RuntimeError("Reviewed source differs for " + key)
        if _is_manager(raw, path):
            import contextlib
            fn = contextlib.contextmanager(fn)
        adds.append((module if len(path) == 1 else getattr(module, path[0]), path[-1],
                     staticmethod(fn) if static else fn))
    # HTTP handler methods live in the make_server closure; swap their code.
    import gc
    canvas_module = sys.modules.get("codex_canvas")
    canvas_raw = (scripts / "codex_canvas.py").read_bytes()
    if canvas_module is None or hashlib.sha256(canvas_raw).hexdigest() != HANDLER["sha"]:
        raise RuntimeError("Unreviewed source: codex_canvas")
    handlers = [o for o in gc.get_objects() if isinstance(o, type)
                and o.__qualname__ == "make_server.<locals>.Handler" and o.__module__ == "codex_canvas"]
    if not handlers:
        raise RuntimeError("The live HTTP handler was not found")
    for handler_cls in handlers:
        for method, (base, desired_sig) in HANDLER["methods"].items():
            live = handler_cls.__dict__[method]
            current = signature(live)
            if current == desired_sig:
                continue
            if current != base:
                raise RuntimeError("Unknown loaded function: make_server|Handler|" + method)
            desired, _ = source_function(canvas_raw, ("make_server", "Handler", method), vars(canvas_module),
                                         closure=live.__closure__)
            if signature(desired) != desired_sig:
                raise RuntimeError("Reviewed source differs for make_server|Handler|" + method)
            work.append((live, desired))
    if not work and not adds:
        return {"status": "already_applied"}
    lock = runtime.lock if runtime is not None else None
    if lock is not None:
        lock.acquire()
    try:
        for spec in MANIFEST["globals"]:
            name, value = spec.split(":")
            if value in sys.modules or "." not in value:
                vars(modules[name][0]).setdefault(value, importlib.import_module(value))
            else:
                source, attr = value.rsplit(".", 1)
                vars(modules[name][0]).setdefault(attr, getattr(importlib.import_module(source), attr))
        for owner, name, fn in adds:
            if isinstance(owner, type):
                setattr(owner, name, fn)
            else:
                vars(owner)[name] = fn
        for live, desired in work:
            live.__code__, live.__defaults__, live.__kwdefaults__ = desired.__code__, desired.__defaults__, desired.__kwdefaults__
    finally:
        if lock is not None:
            lock.release()
    if runtime is not None:
        runtime.changed.set()
    return {"status": "applied", "functions": len(work), "added": len(adds)}
