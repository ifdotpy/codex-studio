"""Canonical version and signing domain for the multi-server access wire protocol."""

from typing import Final


ACCESS_PROTOCOL_VERSION: Final = 1
MULTI_SERVER_SIGNING_DOMAIN_PREFIX = "studio-multi-server-v"
MULTI_SERVER_SIGNING_DOMAIN = (
    f"{MULTI_SERVER_SIGNING_DOMAIN_PREFIX}{ACCESS_PROTOCOL_VERSION}"
)
