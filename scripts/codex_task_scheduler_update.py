"""Apply exact historical task completion and the active scheduler scope."""
import hashlib
from pathlib import Path
import sys

from codex_source import signature, source_function

SOURCES = ('8a7cda7ac5d1a3be896da919416ff479c8d65285381e5b718c69260ca10ff84a',
           '012f54d730b83b7f3dc478354945fb94eb2fd245d48bc5b72e403a65a0e6eaf2')
FUNCTIONS = {
    'record_task': ('02230a41c80aa34dc33bdf92ecce9d2a3165b7b9ae2f9aefacf1a000a47051dc',
                    '45491db50507205f66d5760cf1fd94d228698e4cbb80912ad20f8721d6190c65'),
    'scheduler_agents': ('ab2138ef38e99eb6cabe86a7f9666861c788b07e3a734f3a63feede0e94ed38b',
                         '3f3a656130e0d61c321abcc8df4d632ceec908465966137d6a3b0c17fcff9979'),
}
DEPENDENCIES = {
    'connection_current': 'f8ea88adeb09f9e5b2fe314c212843d8f3f109838b6741c0c5006533e2015174',
    'notification': '6506db3841966afa088cce02a9667f2e80ffafdacf37ac97933b93a0acfe32e3',
    'item': '6dd9edf1583d5847632c483c1b07317cfc4a4d2534128c03dd6623ee775670c9',
    'touch_ui': '4c6d91a96a04a64b256c4778b59c95c8b6830251cb5a696160bbc122c712de46',
    'put': 'c4459a753a4a43f1b2787577f87eeaec809139bff394fa6b52ec9e9bb772198e',
    'mark_agent_records_changed': '50d327482152541b509898f6b755cf1db05e5bf4d2d287831a62a6634f2bb3fb',
}


def apply(runtime):
    scripts = Path(__file__).resolve().parent
    module = sys.modules.get('codex_runtime')
    path = scripts / 'codex_runtime.py'
    if (module is None or not isinstance(runtime, module.Runtime) or runtime.closed
            or Path(module.__file__).resolve() != path):
        raise RuntimeError('The running backend source identity differs')
    raw = path.read_bytes()
    if hashlib.sha256(raw).hexdigest() not in SOURCES:
        raise RuntimeError('The reviewed task or scheduler source differs')
    desired = {name: source_function(raw, ['Runtime', name], vars(module), str(path))[0]
               for name in FUNCTIONS}
    if any(signature(function) != FUNCTIONS[name][1] for name, function in desired.items()):
        raise RuntimeError('The reviewed task or scheduler function differs')
    with runtime.lock:
        if any(name in vars(runtime) for name in FUNCTIONS):
            raise RuntimeError('The running task or scheduler callback differs')
        for name, expected in DEPENDENCIES.items():
            if signature(getattr(module.Runtime, name, None)) != expected:
                raise RuntimeError('The running task or scheduler guard differs: ' + name)
        for name, accepted in FUNCTIONS.items():
            if signature(getattr(module.Runtime, name, None)) not in accepted:
                raise RuntimeError('The running task or scheduler function differs: ' + name)
        if all(signature(getattr(module.Runtime, name)) == accepted[1]
               for name, accepted in FUNCTIONS.items()):
            return {'status': 'already_applied'}
        for name, replacement in desired.items():
            current = getattr(module.Runtime, name)
            current.__code__ = replacement.__code__
            current.__defaults__ = replacement.__defaults__
            current.__kwdefaults__ = replacement.__kwdefaults__
        with runtime.__dict__.setdefault('_scheduler_agent_cache_lock', module.threading.RLock()):
            runtime.__dict__.pop('_scheduler_agent_roster', None)
    runtime.changed.set()
    return {'status': 'applied'}
