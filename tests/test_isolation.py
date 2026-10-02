"""Clear inherited live-supervisor routing before a Studio test imports code."""
import os

SUPERVISOR_ENV = (
    "CODEX_AGENTS_SUPERVISOR_MODE",
    "CODEX_AGENTS_STATE_DIR",
    "CODEX_AGENTS_SUPERVISOR_FALLBACK",
)


def isolate_supervisor_environment():
    for name in SUPERVISOR_ENV:
        os.environ.pop(name, None)


isolate_supervisor_environment()
