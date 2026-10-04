"""Apply the startup receipt selection fix without stopping native work."""
import hashlib
from pathlib import Path
import sys

from codex_source import signature, source_function

SOURCE_SHA256 = 'cf073a6fbe1f5f5f58643a58e719a8f2f41071b928eeb2fb82d4d959e5f2ed09'
BEFORE = '3d92f2a77584ab11d3ddec66b238136693ca618835fefc4948a5d210dcef60c3'
AFTER = '1d8f3a28dec97f61751bf72ea63904d050447237ecaf47a738eb0ab0cd00575d'


def apply(runtime):
    scripts = Path(__file__).resolve().parent
    module = sys.modules.get('codex_runtime')
    path = scripts / 'codex_runtime.py'
    if (module is None or not isinstance(runtime, module.Runtime) or runtime.closed
            or Path(module.__file__).resolve().parent != scripts
            or Path(module.__file__).resolve() != path):
        raise RuntimeError('The running backend source identity differs')
    raw = path.read_bytes()
    if hashlib.sha256(raw).hexdigest() != SOURCE_SHA256:
        raise RuntimeError('The reviewed runtime source differs')
    desired, _ = source_function(raw, ['Runtime', 'recover_monitor_receipts'], vars(module), str(path))
    if signature(desired) != AFTER:
        raise RuntimeError('The reviewed receipt function differs')
    with runtime.lock:
        current = module.Runtime.recover_monitor_receipts
        actual = signature(current)
        if actual not in {BEFORE, AFTER}:
            raise RuntimeError('The running receipt function differs')
        if actual == AFTER:
            return {'status': 'already_applied'}
        current.__code__ = desired.__code__
        current.__defaults__ = desired.__defaults__
        current.__kwdefaults__ = desired.__kwdefaults__
    runtime.changed.set()
    return {'status': 'applied'}
