"""Authoritative cross-language product values for generated consumers."""

MAX_USER_ATTACHMENT_BYTES = 20 * 1024 * 1024
MAX_PROJECT_SECRET_VALUE_BYTES = 16 * 1024
MAX_PROGRESS_FILE_BYTES = 128 * 1024


def user_attachment_limit_label() -> str:
    """Return the display label derived from the byte-valued attachment cap."""
    mib, remainder = divmod(MAX_USER_ATTACHMENT_BYTES, 1024 * 1024)
    if remainder:
        return f"{MAX_USER_ATTACHMENT_BYTES} bytes"
    return f"{mib} MiB"


def project_secret_value_limit_label() -> str:
    """Return the display label derived from the UTF-8 byte-valued secret cap."""
    kib, remainder = divmod(MAX_PROJECT_SECRET_VALUE_BYTES, 1024)
    if remainder:
        return f"{MAX_PROJECT_SECRET_VALUE_BYTES} bytes"
    return f"{kib} KiB"


def progress_file_limit_label() -> str:
    """Return the display label derived from the progress-file byte cap."""
    kib, remainder = divmod(MAX_PROGRESS_FILE_BYTES, 1024)
    if remainder:
        return f"{MAX_PROGRESS_FILE_BYTES} bytes"
    return f"{kib} KiB"
