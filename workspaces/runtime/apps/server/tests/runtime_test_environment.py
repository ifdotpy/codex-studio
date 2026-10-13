"""One environment boundary for Python Runtime fixtures and seed processes."""
from __future__ import annotations

import atexit
from contextlib import ExitStack
import importlib.util
import os
from pathlib import Path
import threading
from unittest.mock import patch
import urllib.request

from runtime_home_guard import install_home_guard
install_home_guard()

_backends = {}
_active = []
_original_environment = None
_original_opener = None
_update_switch = None
_lock = threading.RLock()


def runtime_environment(backend_or_root):
    """Return the harness allow-list, with a replay pin and disposable homes."""
    from http_backend_harness import BackendProcess
    if isinstance(backend_or_root, BackendProcess):
        backend = backend_or_root
    else:
        root = Path(backend_or_root).absolute()
        root.mkdir(parents=True, exist_ok=True)
        key = str(root)
        backend = _backends.get(key)
        if backend is None:
            backend = BackendProcess(root, initialize_workspace=False)
            backend.create_codex_replay("restart-mid-turn-supervisor-replay.json")
            # General Runtime fixtures begin signed out, as on a clean machine.
            (backend.codex_home / "auth.json").write_text("{}", encoding="utf-8")
            _backends[key] = backend
            atexit.register(lambda backend=backend: backend.close() if backend.root.exists() else None)
    environment = backend._environment()
    if not isinstance(backend_or_root, BackendProcess):
        environment["CODEX_AGENTS_STATE_DIR"] = str(root)
    return environment


def make_runtime(runtime_class, root, *args, environment=None, environment_overrides=None, **kwargs):
    """Factory for setUp/tearDown fixtures whose lifetime crosses functions.

    Closing the last fixture restores the caller environment. Multiple live
    runtimes share an isolated process environment; out-of-order closes never
    restore an inherited environment while another fixture remains alive.
    """
    global _original_environment, _original_opener, _update_switch
    environment = runtime_environment(root) if environment is None else dict(environment)
    environment.update(environment_overrides or {})
    root = getattr(root, "state", root)
    token = object()
    with _lock:
        if not _active:
            _original_environment = dict(os.environ)
            _original_opener = urllib.request._opener
            # Disable the background preference before Runtime starts workers.
            # Keep dead proxies as defense for other accidental network calls.
            _update_switch = ExitStack()
            # Historical upgrade seeds predate the updater module entirely.
            if importlib.util.find_spec("codex_provider_upstream_releases") is not None:
                _update_switch.enter_context(patch("codex_provider_upstream_releases.updates_enabled", return_value=False))
        _active.append((token, environment))
        os.environ.clear()
        os.environ.update(environment)

    def release():
        global _original_environment, _original_opener, _update_switch
        with _lock:
            _active[:] = [item for item in _active if item[0] is not token]
            os.environ.clear()
            os.environ.update(_active[-1][1] if _active else _original_environment or {})
            if not _active:
                _original_environment = None
                urllib.request._opener = _original_opener
                _original_opener = None
                _update_switch.close()
                _update_switch = None

    try:
        runtime = runtime_class(root, *args, **kwargs)
    except BaseException:
        release()
        raise
    close = runtime.close
    released = False

    def isolated_close(*close_args, **close_kwargs):
        nonlocal released
        try:
            return close(*close_args, **close_kwargs)
        finally:
            with _lock:
                if not released:
                    released = True
                    release()

    runtime.close = isolated_close
    return runtime
