"""Apply the supervisor exit fix without stopping native work."""
import hashlib
from pathlib import Path
import sys

from codex_source import signature, source_function

SOURCE_SHA256 = 'ea3aff231edbdb32df932263d86796e0b19d658df897c0ca225ba385cb1546b2'
BEFORE = '78c49908efcafd3a339d79da576f134b9937ffb569f4a703426cf4065dcb6768'
AFTER = 'e0c11b587f8b514b7389558497ea977d2adff3af133ef68a6c0040884b07b71c'


def apply(runtime):
    scripts = Path(__file__).resolve().parent
    module = sys.modules.get('codex_runtime')
    supervisor = sys.modules.get('codex_process_supervisor')
    path = scripts / 'codex_process_supervisor.py'
    if (module is None or not isinstance(runtime, module.Runtime) or runtime.closed
            or Path(module.__file__).resolve().parent != scripts
            or supervisor is None or Path(supervisor.__file__).resolve() != path):
        raise RuntimeError('The running backend source identity differs')
    raw = path.read_bytes()
    if hashlib.sha256(raw).hexdigest() != SOURCE_SHA256:
        raise RuntimeError('The reviewed supervisor source differs')
    desired, _ = source_function(raw, ['native_launch_environment'], vars(supervisor), str(path))
    if signature(desired) != AFTER:
        raise RuntimeError('The reviewed supervisor function differs')
    with runtime.lock:
        current = supervisor.native_launch_environment
        actual = signature(current)
        if actual not in {BEFORE, AFTER}:
            raise RuntimeError('The running supervisor function differs')
        if actual == AFTER:
            return {'status': 'already_applied'}
        current.__code__ = desired.__code__
        current.__defaults__ = desired.__defaults__
        current.__kwdefaults__ = desired.__kwdefaults__
    runtime.changed.set()
    return {'status': 'applied'}
