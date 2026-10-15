"""Standard-library validator for the analytics context window contract."""
import math


def is_context_window(value: object) -> bool:
    """Return whether a decoded value matches nonnegative-number-or-null."""
    if value is None:
        return True
    return (
        isinstance(value, (int, float))
        and not isinstance(value, bool)
        and math.isfinite(value)
        and value >= 0
    )


def canonical_context_window(value: object) -> int | float | None:
    """Keep valid native values and represent malformed values as JSON null."""
    return value if is_context_window(value) else None
