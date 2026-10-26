"""Python copy of runtime-services/src/multi_server_transport_limits.rs, the owner; removed with the Python server."""

MULTI_SERVER_MAX_REQUEST_BYTES = 256 * 1024
MULTI_SERVER_MAX_RESPONSE_BYTES = 1024 * 1024


def _format_byte_limit(byte_count: int) -> str:
    if byte_count % (1024 * 1024) == 0:
        return f"{byte_count // (1024 * 1024)} MiB"
    if byte_count % 1024 == 0:
        return f"{byte_count // 1024} KiB"
    return f"{byte_count} bytes"


MULTI_SERVER_MAX_REQUEST_LABEL = _format_byte_limit(MULTI_SERVER_MAX_REQUEST_BYTES)
MULTI_SERVER_MAX_RESPONSE_LABEL = _format_byte_limit(MULTI_SERVER_MAX_RESPONSE_BYTES)
