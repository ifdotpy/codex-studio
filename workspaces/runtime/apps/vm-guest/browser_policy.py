"""The URL rules of the VM browser bridge (docs/vm-browser-bridge.md).

The guest service and the Studio server on the Mac both use this file, so one set of rules
decides which links a VM agent can ask the person to open. Studio knows no tool: a link is a
tool's own loopback start page (kubelogin's http://localhost:8000/) or a provider page
(https), and a provider page can name a loopback `redirect_uri` that needs a forward.
"""
from __future__ import annotations

from dataclasses import dataclass
import ipaddress
import re
from urllib.parse import SplitResult, parse_qs, urlsplit

MAX_URL = 8192
# The longest device code that the card shows, in characters.
USER_CODE_LIMIT = 64
LOOPBACK_HOSTS = frozenset({"localhost", "127.0.0.1", "::1"})
# The protocol markers of a link that a person must open to sign in or approve
# (docs/vm-browser-bridge.md, section 7). A link is a sign-in link when its query has every name
# of one set, each with a value. The names come from the protocols, never from a tool:
# an OAuth 2.0 or OpenID Connect authorization request (RFC 6749 4.1.1, RFC 9126 for
# `request_uri`), a device verification link with its code (RFC 8628 3.3.1), and a SAML 2.0
# request in the HTTP-Redirect binding.
SIGN_IN_MARKERS = (
    frozenset({"client_id", "response_type"}),
    frozenset({"client_id", "redirect_uri"}),
    frozenset({"client_id", "request_uri"}),
    frozenset({"user_code"}),
    frozenset({"SAMLRequest"}),
)
# The most sign-in links that one agent message can give to the person.
MESSAGE_LINK_LIMIT = 4
_LINK_START = re.compile(r"https?://", re.IGNORECASE | re.ASCII)
# A link ends at a character that is not printable ASCII or at one of these.
_LINK_STOP = frozenset("<>\"'`")
# Text punctuation after a link, not part of it.
_LINK_TRAIL = frozenset(".,;:!?*")
# Names for documentation and examples (RFC 2606, RFC 6761). A message that explains a sign-in
# protocol with them names no real sign-in.
DOCUMENTATION_DOMAINS = ("example.com", "example.net", "example.org")
DOCUMENTATION_SUFFIXES = (".example", ".invalid")


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
    # The OAuth `state` of the link. A pasted redirect must carry the same value.
    state: str | None = None
    # The `user_code` of a device code page (RFC 8628 verification_uri_complete).
    user_code: str | None = None


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


def _exact(query: dict[str, list[str]], name: str) -> str | None:
    """The one value of `name`, untruncated, or None when it is missing or repeated."""
    values = query.get(name)
    return values[0] if values and len(values) == 1 and values[0] else None


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
    user_code = _first(query, "user_code") if redirect is None else None
    return Provider(text, host, redirect, _first(query, "client_id"), _first(query, "scope", "scopes"),
                    state=_exact(query, "state"), user_code=user_code[:USER_CODE_LIMIT] if user_code else None)


@dataclass(frozen=True)
class Callback:
    """A sign-in redirect that the person pasted: the path and query for the tool's listener."""

    target: str
    state: str | None
    error: bool


def pasted(text: object, redirect: dict[str, object], state: str | None) -> Callback:
    """Check the address of the page that did not load after a sign-in (docs/vm-browser-bridge.md,
    section 6.3). It must be the request's loopback redirect with a code or an error and the
    `state` of the provider link."""
    value = _text(text)
    if "://" not in value:
        # Some phone browsers copy the address without the scheme.
        value = "http://" + value
    parts, host, port = _parts(value)
    if parts.scheme.lower() != "http" or host not in LOOPBACK_HOSTS:
        raise PolicyError("Paste the address of the page that did not load. It starts with "
                          "http://localhost or http://127.0.0.1")
    if port != redirect.get("port") or (parts.path or "/") != redirect.get("path"):
        raise PolicyError(f"The address is not for this sign-in: it must use port {redirect.get('port')} "
                          f"and the path {redirect.get('path')}")
    query = parse_qs(parts.query, keep_blank_values=True)
    if not _first(query, "code") and not _first(query, "error"):
        raise PolicyError("The address has no sign-in result. Paste it after the sign-in ends")
    if not state:
        raise PolicyError("This sign-in has no state value, so Studio cannot check a pasted address. "
                          "Use a desktop or the device code")
    if _exact(query, "state") != state:
        raise PolicyError("The address belongs to another sign-in: its state value is different")
    return Callback((parts.path or "/") + "?" + parts.query, state, bool(_first(query, "error")))


def _trim(link: str) -> str:
    while link:
        last = link[-1]
        if last in _LINK_TRAIL or (last == ")" and link.count(")") > link.count("(")) \
                or (last == "]" and link.count("]") > link.count("[")):
            link = link[:-1]
            continue
        return link
    return link


def _documentation_host(link: str) -> bool:
    """Whether the link's host is a name for documentation. The link is printable ASCII."""
    authority = link.split("://", 1)[1]
    for stop in "/?#":
        authority = authority.split(stop, 1)[0]
    host = authority.rpartition("@")[2].lower()
    host = host[1:].partition("]")[0] if host.startswith("[") else host.partition(":")[0]
    host = host.rstrip(".")
    return any(host == name or host.endswith("." + name) for name in DOCUMENTATION_DOMAINS) \
        or any(host.endswith(suffix) or host == suffix[1:] for suffix in DOCUMENTATION_SUFFIXES)


def _query_names(link: str) -> set[str]:
    """The query names with a value, split as `urlsplit` and `parse_qs` split them."""
    query = link.split("#", 1)[0].partition("?")[2]
    return {name for name, values in parse_qs(query, keep_blank_values=True).items() if any(values)}


def sign_in_links(text: object) -> list[str]:
    """The links in an agent's message that the person must open to sign in or approve.

    Studio shows each one as a sign-in card (docs/vm-browser-bridge.md, section 7). The rule
    reads only protocol markers in the query (`SIGN_IN_MARKERS`), so an ordinary link stays an
    ordinary link, and a link to a documentation name (`DOCUMENTATION_DOMAINS`) is an example.
    The URL rules of `classify` still decide whether the link can open."""
    if not isinstance(text, str) or "://" not in text:
        return []
    links: list[str] = []
    position = 0
    while len(links) < MESSAGE_LINK_LIMIT:
        match = _LINK_START.search(text, position)
        if match is None:
            break
        end = match.end()
        while end < len(text) and 32 < ord(text[end]) < 127 and text[end] not in _LINK_STOP:
            end += 1
        position = end
        link = _trim(text[match.start():end])
        if len(link) > MAX_URL or link in links or _documentation_host(link):
            continue
        names = _query_names(link)
        if any(marker <= names for marker in SIGN_IN_MARKERS):
            links.append(link)
    return links
