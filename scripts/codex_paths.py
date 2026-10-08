"""Path syntax helpers for requests that target another operating system."""

from pathlib import PurePosixPath, PureWindowsPath


def is_absolute_on_any_platform(value: str) -> bool:
    """Check absolute syntax without assuming the caller's OS is the target OS.

    The receiving server still validates the path with its native filesystem
    before using it.
    """
    return PurePosixPath(value).is_absolute() or PureWindowsPath(value).is_absolute()
