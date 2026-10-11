"""The URL rules of the VM browser bridge (docs/vm-browser-bridge.md).

The guest service and the Studio server on the Mac both use this file, so one set of rules
decides which links a VM agent can ask the person to open. Studio knows no tool: a link is a
tool's own loopback start page (kubelogin's http://localhost:8000/) or a provider page
(https), and a provider page can name a loopback `redirect_uri` that needs a forward.
"""
from __future__ import annotations

from dataclasses import dataclass
import ipaddress
from urllib.parse import SplitResult, parse_qs, urlsplit

MAX_URL = 8192
LOOPBACK_HOSTS = frozenset({"localhost", "127.0.0.1", "::1"})


class PolicyError(ValueError):
    """The link cannot open. The message is for the agent and the person."""


@dataclass(frozen=True)
class Redirect:
    """The loopback address that receives the sign-in result."""

    host: str
    port: int
    path: str

    def authority(self) -> str:
        return ("[::1]" if self.host == "::1" else self.host) + ":" + str(self.port)

    def record(self) -> dict[str, str | int]:
        return {"host": self.host, "port": self.port, "path": self.path}


@dataclass(frozen=True)
class StartPage:
    """A tool's own page on the guest loopback. The guest reads its first redirect."""

    url: str
    port: int


@dataclass(frozen=True)
class Provider:
    """A sign-in page that the person's browser opens."""

    url: str
    host: str
    redirect: Redirect | None
    client_id: str | None
    scope: str | None


def _text(value: object) -> str:
    if not isinstance(value, str) or not value:
        raise PolicyError("Give one link")
    if len(value) > MAX_URL:
        raise PolicyError("The link is longer than 8192 characters")
    if any(ord(character) <= 32 or ord(character) == 127 for character in value):
        raise PolicyError("The link contains spaces or control characters")
    return value


def _parts(text: str) -> tuple[SplitResult, str, int | None]:
    parts = urlsplit(text)
    if parts.username is not None or parts.password is not None:
        raise PolicyError("A link with a user name or password cannot open")
    host = (parts.hostname or "").lower()
    if not host:
        raise PolicyError("The link has no host")
    try:
        port = parts.port
    except ValueError:
        raise PolicyError("The link has an invalid port") from None
    return parts, host, port


def _origin(host: str, port: int) -> str:
    return ("[" + host + "]" if ":" in host else host) + ":" + str(port)


def _local_host(host: str) -> bool:
    if host == "localhost" or host.endswith(".localhost"):
        return True
    try:
        address = ipaddress.ip_address(host)
    except ValueError:
        return False
    return (address.is_private or address.is_loopback or address.is_link_local
            or address.is_unspecified or address.is_multicast or address.is_reserved)


def _first(query: dict[str, list[str]], *names: str) -> str | None:
    for name in names:
        values = query.get(name)
        if values and values[0]:
            return values[0][:500]
    return None


def classify(url: object, *, http_providers: frozenset[str] = frozenset()) -> StartPage | Provider:
    """Sort a link from an agent into a loopback start page or a provider page.

    `http_providers` holds test origins (`host:port`) that may use http. Production passes
    nothing, so every provider page uses https."""
    text = _text(url)
    parts, host, port = _parts(text)
    if parts.scheme.lower() == "http" and host in LOOPBACK_HOSTS \
            and _origin(host, port or 80) not in http_providers:
        if port is None or not 1024 <= port <= 65535:
            raise PolicyError("A loopback page needs a port from 1024 to 65535")
        return StartPage(text, port)
    return provider(text, http_providers=http_providers)


def provider(url: object, *, http_providers: frozenset[str] = frozenset()) -> Provider:
    """Check a provider page and find its loopback redirect."""
    text = _text(url)
    parts, host, port = _parts(text)
    scheme = parts.scheme.lower()
    if scheme == "https":
        if _local_host(host):
            raise PolicyError("An https link to a local or private address cannot open")
    elif not (scheme == "http" and _origin(host, port or 80) in http_providers):
        raise PolicyError("Only https sign-in links and loopback sign-in pages can open")
    query = parse_qs(parts.query, keep_blank_values=True)
    redirect = None
    target_text = _first(query, "redirect_uri")
    if target_text:
        target = urlsplit(target_text)
        target_host = (target.hostname or "").lower()
        if target_host in LOOPBACK_HOSTS:
            try:
                target_port = target.port
            except ValueError:
                raise PolicyError("The loopback redirect has an invalid port") from None
            if target.scheme.lower() != "http":
                raise PolicyError("A loopback redirect must use http")
            if target.username is not None or target.password is not None:
                raise PolicyError("A loopback redirect with a user name cannot open")
            if target_port is None or not 1024 <= target_port <= 65535:
                raise PolicyError("A loopback redirect needs a port from 1024 to 65535")
            redirect = Redirect(target_host, target_port, target.path or "/")
    return Provider(text, host, redirect, _first(query, "client_id"), _first(query, "scope", "scopes"))
