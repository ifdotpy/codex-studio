"""Apply cost invalidation without replacing bound analytics callbacks."""
import hashlib
from pathlib import Path
import sys

from codex_source import signature, source_function

SOURCE_HASH = 'ba3a71a6624917d80019cb5d4d8de408dfb03c4bce4e9d975dcb6514ecdb061a'
RUNTIME_HASHES = frozenset({
    '7e2f7a03acdf9c6a1c8876f022b70c14821ef37a7119f33ed910845e18ba7450',
    '9b7fcf91543d2e469bc8581ec8d4c1cd868afe64a4b035a15fe0f5fd1beac7e7',
})
BEFORE = '6278ad43b71a2a2ba34c86c8ee3c7fdcf634a0fa5043f2232965c0c0da2e46e6'
AFTER = '974cdbc5ccda9713aab3e3d08c2440bcf38b9f7eedc7fddf700cd1e17f883a05'
DEPENDENCIES = {
    'analytics_agent': 'fb1b9c9fe1326c6934471f9d7c9938a9cd8129a8e56a29cb362cce5cd9427939',
    'analytics_budget_capture': '7024ae782e0013bc6f60d31d02badcf8a25ebf1d3b4ea351b050f0e92a0f6ba7',
}


def apply(runtime):
    scripts = Path(__file__).resolve().parent
    module = sys.modules.get('codex_runtime')
    analytics = sys.modules.get('codex_analytics')
    if (module is None or not isinstance(runtime, module.Runtime) or runtime.closed
            or Path(module.__file__).resolve().parent != scripts
            or analytics is None or Path(analytics.__file__).resolve().parent != scripts):
        raise RuntimeError('The running backend source identity differs')
    if hashlib.sha256(Path(module.__file__).read_bytes()).hexdigest() not in RUNTIME_HASHES:
        raise RuntimeError('The running backend source differs')
    path = scripts / 'codex_analytics.py'
    raw = path.read_bytes()
    if hashlib.sha256(raw).hexdigest() != SOURCE_HASH:
        raise RuntimeError('The reviewed analytics source differs')
    desired, _ = source_function(raw, ['AnalyticsMixin', 'analytics_event'], vars(analytics), str(path))
    if signature(desired) != AFTER:
        raise RuntimeError('The reviewed analytics function differs')
    with runtime.lock:
        if runtime.closed:
            raise RuntimeError('The running backend is closed')
        owner = analytics.AnalyticsMixin
        for name, expected in DEPENDENCIES.items():
            method = getattr(owner, name, None)
            if (signature(method) != expected
                    or getattr(getattr(runtime, name, None), '__func__', None) is not method):
                raise RuntimeError('The running analytics guard differs: ' + name)
        current = owner.analytics_event
        if (current.__globals__ is not vars(analytics)
                or getattr(runtime.analytics_event, '__func__', None) is not current):
            raise RuntimeError('The running analytics callback identity differs')
        current_signature = signature(current)
        if current_signature not in {BEFORE, AFTER}:
            raise RuntimeError('The running analytics function differs')
        if current_signature == AFTER:
            return {'status': 'already_applied'}
        current.__code__ = desired.__code__
        current.__defaults__ = desired.__defaults__
        current.__kwdefaults__ = desired.__kwdefaults__
    return {'status': 'applied'}
