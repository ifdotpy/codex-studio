"""Apply the backend diagnostics fix without stopping native work."""
import hashlib
from pathlib import Path
import sys

from codex_source import signature, source_function

SOURCE_SHA256 = '4ab812f8d9872c2b1d66e25c08690ecd02c9120f728c34feaf773c687343d3be'
BEFORE = '73dbe4d590945b71c56797ac8af1f26ce846affbaec9e8246f7f3d6137959c3b'
AFTER = '92bf9567633dc6cf018c90fb0a6842d77e5993cf7db15c01ca396760fb27c002'


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
