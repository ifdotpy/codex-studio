"""Authoritative shared values consumed by generated Rust and TypeScript contracts."""

MAX_USER_ATTACHMENT_BYTES = 20 * 1024 * 1024


def user_attachment_limit_label() -> str:
    """Return the display label derived from the byte-valued attachment cap."""
    mib, remainder = divmod(MAX_USER_ATTACHMENT_BYTES, 1024 * 1024)
    if remainder:
        return f"{MAX_USER_ATTACHMENT_BYTES} bytes"
    return f"{mib} MiB"
