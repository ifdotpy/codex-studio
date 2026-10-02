"""Apply reviewed monitor and terminal fixes while preserving native work."""
import hashlib
import importlib
from pathlib import Path
import sys

from codex_source import signature, source_function

SOURCES = {'codex_rules': {'source': 'codex_rules.py',
                 'sha256': '4a726c8ba2a10e84dbb52d478345ccb7c016b7773610bd8fd51c631675d0f9a8',
                 'functions': [{'path': ['RulesMixin', 'monitor_input'],
                                'before': '8a194de7e6576700410b9b262c1203e4b97f4f477510750a5c00e4774e7cd1a9',
                                'compatible': [],
                                'after': '849398c3ea60a70fa3d4e1494824924b10bdbffe45886919416d961edfa81132',
                                'static': False}]},
 'codex_terminals': {'source': 'codex_terminals.py',
                     'sha256': '5a140025343f055ac2e0f2d05914e48640ee6dfe4e0370317844daa6f507c2ab',
                     'functions': [{'path': ['TerminalManager', 'create'],
                                    'before': '8896104685ebd5c03b50d9c4c5055300998308c3e1104d3e6e4a85b51fb26089',
                                    'compatible': [],
                                    'after': '63a814a8e30b1513d81848a4cb1dff636a1f8039bf620299df679242f20059e3',
                                    'static': False},
                                   {'path': ['TerminalManager', 'spawn_result'],
                                    'before': '4a7ccd826da1262d8a86dfef31d98736023797355cb0655b05fd8c102faa0932',
                                    'compatible': [],
                                    'after': 'c980add560e92549a8aa34cdb512b1a63b2b8cb5702465d65b918fc0937c0f51',
                                    'static': False},
                                   {'path': ['TerminalManager', 'start_guard'],
                                    'before': None,
                                    'compatible': [],
                                    'after': '1ecd199fdb882e55371cc7b3ff8adfdca9608ca1681f8ea0fb1b42ba6ffbbcae',
                                    'static': False},
                                   {'path': ['TerminalManager', 'connect'],
                                    'before': '75b78ab9f43c5c7357a29716568993943ef9be747ff5289d95c00ddaf0a37345',
                                    'compatible': ['60e0186f74cd4fe6abcb243dd4a04bd785188efd88207a61fd1f7ebc8dd3e816'],
                                    'after': 'a6f2ebdfcfeb8758e5e606eb6ca7d200eedec376c58b4cdc2d0c0505e1ff60b7',
                                    'static': False},
                                   {'path': ['TerminalManager', 'action'],
                                    'before': '2a9035962931db2b9fd1d9333ec1189e3328dc2ee4f09464fed68534290d7578',
                                    'compatible': [],
                                    'after': '54a45018063d2020d5c8222e790ba6c9013cc544ec76f13ab134136ca7058efc',
                                    'static': False},
                                   {'path': ['TerminalManager', 'stop_process'],
                                    'before': None,
                                    'compatible': [],
                                    'after': '9d5ca41df76c97111cf384cc49e23d8ef6035fabc39153510c205c098f7eddea',
                                    'static': False},
                                   {'path': ['TerminalManager', 'close'],
                                    'before': '1c427c1af3e6c5c42fafc74a3b643610f3ed3c40ae84654b67fe9ddabd4a0dca',
                                    'compatible': [],
                                    'after': 'bd661a99805bd928e2336d928c405a963bf559dc4f7376e328c26671a4f61c94',
                                    'static': False}]},
 'codex_sync': {'source': 'codex_sync.py',
                'sha256': '4a6d1b435f04922dc8c18d59ab1f4cc7e904344085eaab2dd9b78de2721ee030',
                'functions': [{'path': ['SyncStore', 'entity_maintenance_needed'],
                               'before': None,
                               'compatible': [],
                               'after': '4a00362a6d1fc36a26cd9986c129502eb11ad14bb3e02d44c2024251be5ca610',
                               'static': False},
                              {'path': ['SyncStore', 'pull'],
                               'before': '26b754642933812730e2266424feaba8523004bdc0f5f6b17e174a76b3ca6d0e',
                               'compatible': [],
                               'after': '4129f1e3784347c150ac65a87c075eb07c73395e27e94d56b55175c44bc2d343',
                               'static': False}]},
 'codex_sqlite': {'source': 'codex_sqlite.py',
                  'sha256': '047023e48d8e10abda8312f15e214645b221c49b48ff7dc50b2359bb8e70f65d',
                  'functions': [{'path': ['diagnostics'],
                                 'before': 'ad558cd385ef2184f81b510e6608cd8153fcfad3f17381428c87d92bc07d7738',
                                 'compatible': [],
                                 'after': 'a2011a387dbb1570d5f43cac61aa681455f2cd5ea8b95914a8a49d63bae715d8',
                                 'static': False}]}}

def target(module, path):
    owner = module
    for part in path[:-1]:
        owner = getattr(owner, part)
    current = vars(owner).get(path[-1])
    return owner, current.__func__ if isinstance(current, staticmethod) else current


def apply(runtime):
    scripts = Path(__file__).resolve().parent
    module = sys.modules.get('codex_runtime')
    if (module is None or not isinstance(runtime, module.Runtime)
            or Path(module.__file__).resolve().parent != scripts):
        raise RuntimeError('The running backend source identity differs')
    replacements = []
    for name, spec in SOURCES.items():
        path = scripts / spec['source']
        raw = path.read_bytes()
        if hashlib.sha256(raw).hexdigest() != spec['sha256']:
            raise RuntimeError('The reviewed backend source differs: ' + name)
        loaded = sys.modules.get(name) or importlib.import_module(name)
        if Path(loaded.__file__).resolve() != path.resolve():
            raise RuntimeError('The running module source identity differs: ' + name)
        for item in spec['functions']:
            desired, static = source_function(raw, item['path'], vars(loaded), str(path))
            if signature(desired) != item['after'] or static != item['static']:
                raise RuntimeError('The reviewed function differs: ' + '.'.join(item['path']))
            replacements.append((loaded, item, desired))
    with runtime.lock:
        # Validate every function before changing any function or adding helpers.
        for loaded, item, desired in replacements:
            owner, current = target(loaded, item['path'])
            if current is None:
                if item['before'] is not None:
                    raise RuntimeError('The running function is missing: ' + '.'.join(item['path']))
            elif signature(current) not in {item['before'], item['after'], *item.get('compatible', [])}:
                raise RuntimeError('The running function differs: ' + '.'.join(item['path']) + ' (' + signature(current) + ')')
        # Existing terminal managers obtain the new guard lazily. Install helpers
        # before any existing method can call them. Native objects stay intact.
        for loaded, item, desired in sorted(replacements, key=lambda entry: entry[1]['before'] is not None):
            owner, current = target(loaded, item['path'])
            if current is None:
                setattr(owner, item['path'][-1], staticmethod(desired) if item['static'] else desired)
            else:
                current.__code__ = desired.__code__
                current.__defaults__ = desired.__defaults__
                current.__kwdefaults__ = desired.__kwdefaults__
    runtime.changed.set()
    return {'status': 'applied'}
