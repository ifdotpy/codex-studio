"""One environment boundary for Python Runtime fixtures and seed processes."""
from __future__ import annotations

import atexit
from contextlib import ExitStack
import importlib
import importlib.util
import os
from pathlib import Path
import sys
import threading
from typing import Any, Callable, Mapping, Protocol, TypeVar, cast
from unittest.mock import patch
import urllib.request

# Test support stays outside the production import graph and mypy source set.
# This typed entry point is shared by source-side unittest modules and scripts.
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "tests"))
importlib.import_module("runtime_home_guard").install_home_guard()


class _Closable(Protocol):
    def close(self) -> None: ...


_Runtime = TypeVar("_Runtime", bound=_Closable)

_backends: dict[str, Any] = {}
_active: list[tuple[object, dict[str, str]]] = []
_original_environment: dict[str, str] | None = None
_original_opener: urllib.request.OpenerDirector | None = None
_update_switch: ExitStack | None = None
_lock = threading.RLock()


def runtime_environment(backend_or_root: Any) -> dict[str, str]:
    """Return the harness allow-list, with a replay pin and disposable homes."""
    BackendProcess = importlib.import_module("http_backend_harness").BackendProcess
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
            def cleanup_backend(backend: Any = backend) -> None:
                if backend.root.exists():
                    backend.close()
            atexit.register(cleanup_backend)
    environment = cast(dict[str, str], backend._environment())
    if not isinstance(backend_or_root, BackendProcess):
        environment["CODEX_AGENTS_STATE_DIR"] = str(root)
    return environment


def make_runtime(
    runtime_class: Callable[..., _Runtime],
    root: Any,
    *args: Any,
    environment: Mapping[str, str] | None = None,
    environment_overrides: Mapping[str, str] | None = None,
    **kwargs: Any,
) -> _Runtime:
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
            _original_opener = getattr(urllib.request, "_opener", None)
            # Disable the background preference before Runtime starts workers.
            # Keep dead proxies as defense for other accidental network calls.
            _update_switch = ExitStack()
            # Historical upgrade seeds predate the updater module entirely.
            if importlib.util.find_spec("codex_provider_upstream_releases") is not None:
                _update_switch.enter_context(patch("codex_provider_upstream_releases.updates_enabled", return_value=False))
        _active.append((token, environment))
        os.environ.clear()
        os.environ.update(environment)

    def release() -> None:
        global _original_environment, _original_opener, _update_switch
        with _lock:
            _active[:] = [item for item in _active if item[0] is not token]
            os.environ.clear()
            os.environ.update(_active[-1][1] if _active else _original_environment or {})
            if not _active:
                _original_environment = None
                setattr(urllib.request, "_opener", _original_opener)
                _original_opener = None
                assert _update_switch is not None
                _update_switch.close()
                _update_switch = None

    try:
        runtime = runtime_class(root, *args, **kwargs)
    except BaseException:
        release()
        raise
    close = runtime.close
    released = False

    def isolated_close() -> None:
        nonlocal released
        try:
            return close()
        finally:
            with _lock:
                if not released:
                    released = True
                    release()

    setattr(runtime, "close", isolated_close)
    return runtime
