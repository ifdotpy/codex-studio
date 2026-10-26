"""Python copy of runtime-schema/src/multi_server_protocol.rs, the owner; removed with the Python server."""

from typing import Final


ACCESS_PROTOCOL_VERSION: Final = 1
MULTI_SERVER_SIGNING_DOMAIN_PREFIX = "studio-multi-server-v"
MULTI_SERVER_SIGNING_DOMAIN = (
    f"{MULTI_SERVER_SIGNING_DOMAIN_PREFIX}{ACCESS_PROTOCOL_VERSION}"
)
