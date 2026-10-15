"""Standard-library validator for the analytics context window contract."""
import math


def is_context_window(value: object) -> bool:
    """Return whether a decoded value matches nonnegative-number-or-null."""
    if value is None:
        return True
    if not isinstance(value, (int, float)) or isinstance(value, bool):
        return False
    try:
        return math.isfinite(value) and value >= 0
    except OverflowError:
        return False


def canonical_context_window(value: object) -> int | float | None:
    """Keep valid native values and represent malformed values as JSON null."""
    return value if is_context_window(value) else None
