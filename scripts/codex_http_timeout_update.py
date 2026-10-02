"""Apply reviewed HTTP timeout fixes without replacing native processes."""
import gc
import hashlib
import importlib
from pathlib import Path
import sys

from codex_source import signature, source_function

SOURCES = {'codex_session_costs': {'source': 'codex_session_costs.py',
                         'sha256': 'effc0aa303a36256ca8d9892dcc1ea15967cb612d6f0374dd39e5c24cb973b0a',
                         'functions': [{'path': ['SessionCostReader', '_start_refresh'],
                                        'before': '68a268ce7ba4d7f6cf7d3c62eb4a552e6b807ff17eaf7ea06df9a70a2266e045',
                                        'after': 'ca3b69ac0862f719e41541b029cbaea97afaf96a11b89d8a0b8dd755ad1e28b1',
                                        'static': False},
                                       {'path': ['SessionCostReader', '_background_refresh'],
                                        'before': '36009fedf2d5cd8eb175e1d368b142bf7e9462b40bea81ae5f8d286d4bfc9661',
                                        'after': '9b00d4f8162437c0e182ab96f9175b4a107ec0f88d4760102330348907fce57c',
                                        'static': False},
                                       {'path': ['SessionCostReader', 'snapshot'],
                                        'before': 'f8bcaa97a3975a763fc572d71da4ca9dfd464af745f0980a4d2bd47989cdc828',
                                        'after': '8bb177b8b904bbb66a30a2de09a3e19c05e7c936a5732f39a824786ebb7ca4de',
                                        'static': False}]},
 'codex_catalog': {'source': 'codex_catalog.py',
                   'sha256': '4f1b3bd3b2caad2e7a1759a6c0f78ba4bca4f682203c2bb83a0e6c6b98723482',
                   'functions': [{'path': ['ModelCatalogCache', 'read'],
                                  'before': 'dcfd8c754f26a6246b286e2b6c14abd2114119392e147c407f5f2f0b953b5c32',
                                  'after': 'bd02bc7a3b4cc337567770b45dc1785ac71bf2b2eb1e22013010808bed287642',
                                  'static': False}]},
 'codex_worker_accounts': {'source': 'codex_worker_accounts.py',
                           'sha256': '919659d020674650f1ef51162367dd86011aa48c81185748e4908e5c965e11da',
                           'functions': [{'path': ['catalog'],
                                          'before': '6ca74f0f22321d9fdf57a2f35f19f0c819b9832dd63136e808823b02fcc46436',
                                          'after': '965ecd3f2e25d16fc00762ce99a2c4886d8bcffa3effff804ebb1f065df5f756',
                                          'static': False}]}}

HANDLER = {'sha256': 'c616992842e5f3b6c0a58cc720b205c3893de4d50f05d8d642664776be82036d',
 'before': '32b130bb7a92d363e0cb250945bdb26e252ffee3ed480b41ba3a39b4c2676da2',
 'after': '2029f648ca40382fcefb83c34a1e57f5222c9c7f949031c57302ff56da000e08'}

def target(module, path):
    owner = module
    for part in path[:-1]:
        owner = getattr(owner, part)
    current = vars(owner).get(path[-1])
    return owner, current.__func__ if isinstance(current, staticmethod) else current


def apply(runtime):
    scripts = Path(__file__).resolve().parent
    module = sys.modules.get("codex_runtime")
    if (module is None or not isinstance(runtime, module.Runtime)
            or Path(module.__file__).resolve().parent != scripts):
        raise RuntimeError("The running backend source identity differs")
    replacements = []
    for name, spec in SOURCES.items():
        path = scripts / spec["source"]
        raw = path.read_bytes()
        if hashlib.sha256(raw).hexdigest() != spec["sha256"]:
            raise RuntimeError("The reviewed backend source differs: " + name)
        loaded = sys.modules.get(name) or importlib.import_module(name)
        if Path(loaded.__file__).resolve() != path.resolve():
            raise RuntimeError("The running module source identity differs: " + name)
        for item in spec["functions"]:
            desired, static = source_function(raw, item["path"], vars(loaded), str(path))
            if signature(desired) != item["after"] or static != item["static"]:
                raise RuntimeError("The reviewed function differs: " + ".".join(item["path"]))
            owner, current = target(loaded, item["path"])
            replacements.append((owner, item["path"][-1], current, desired, item))
    canvas = sys.modules.get("codex_canvas")
    path = scripts / "codex_canvas.py"
    raw = path.read_bytes()
    if (canvas is None or Path(canvas.__file__).resolve() != path.resolve()
            or hashlib.sha256(raw).hexdigest() != HANDLER["sha256"]):
        raise RuntimeError("The reviewed HTTP source differs")
    handlers = []
    for candidate in gc.get_objects():
        if (not isinstance(candidate, type) or candidate.__module__ != "codex_canvas"
                or candidate.__qualname__ != "make_server.<locals>.Handler"):
            continue
        current = vars(candidate).get("do_GET")
        if current is None or "canvas" not in current.__code__.co_freevars:
            continue
        index = current.__code__.co_freevars.index("canvas")
        if current.__closure__[index].cell_contents.runtime is runtime:
            handlers.append(candidate)
    if not handlers:
        raise RuntimeError("The running HTTP handler was not found")
    for owner in handlers:
        current = owner.do_GET
        desired, _ = source_function(raw, ("make_server", "Handler", "do_GET"),
                                     vars(canvas), str(path), closure=current.__closure__)
        if (signature(desired) != HANDLER["after"]
                or desired.__code__.co_freevars != current.__code__.co_freevars):
            raise RuntimeError("The reviewed HTTP function differs")
        replacements.append((owner, "do_GET", current, desired, HANDLER))
    changed = False
    with runtime.lock:
        # Check every source and live function before any code replacement.
        for owner, name, current, desired, spec in replacements:
            if current is None:
                if spec["before"] is not None:
                    raise RuntimeError("The running function is missing: " + name)
            elif signature(current) not in {spec["before"], spec["after"]}:
                raise RuntimeError("The running function differs: " + name + " (" + signature(current) + ")")
        for owner, name, current, desired, spec in replacements:
            if current is None:
                setattr(owner, name, staticmethod(desired) if spec["static"] else desired)
                changed = True
            elif signature(current) != spec["after"]:
                current.__code__ = desired.__code__
                current.__defaults__ = desired.__defaults__
                current.__kwdefaults__ = desired.__kwdefaults__
                changed = True
    if changed:
        runtime.changed.set()
    return {"status": "applied" if changed else "already_applied"}
