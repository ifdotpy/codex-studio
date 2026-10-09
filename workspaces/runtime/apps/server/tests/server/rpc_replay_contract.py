"""Shared explicit allowlist for fixture RPCs that intentionally return empty."""

EMPTY_RESULT_METHODS = frozenset({
    "thread/loaded/list",
    "thread/name/set",
    "thread/metadata/update",
    "turn/interrupt",
})
