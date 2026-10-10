"""Shared local HTTP transport for authenticated Codex Studio CLI requests."""

from __future__ import annotations

import json
from http.client import HTTPResponse, IncompleteRead
import ipaddress
from urllib.error import HTTPError
from urllib.parse import urlsplit
from urllib.request import ProxyHandler, Request, build_opener, urlopen


SESSION_ENDPOINT = "/api/session"
DESKTOP_ENDPOINT = "/api/desktop"
ENTITY_PULL_ENDPOINT = "/api/sync/pull"
ENTITY_SCOPE = "state:entities:v1"
ENTITY_PAGE_SIZE = 500
ENTITY_PULL_MAX_PAGES = 100
_DIRECT_OPENER = build_opener(ProxyHandler({}))


class StudioHTTPError(ValueError):
    def __init__(self, message: str, status_code: int, detail: str | None):
        super().__init__(message)
        self.status_code = status_code
        self.detail = detail


def open_request(request: Request, *, timeout: float) -> HTTPResponse:
    """Open loopback Studio directly; only remote API URLs use environment proxies."""
    host = urlsplit(request.full_url).hostname
    is_loopback = host == "localhost"
    if host and not is_loopback:
        try:
            is_loopback = ipaddress.ip_address(host).is_loopback
        except ValueError:
            pass
    if is_loopback:
        response: HTTPResponse = _DIRECT_OPENER.open(request, timeout=timeout)
    else:
        response = urlopen(request, timeout=timeout)
    return response


def request_json(url: str, path: str, data: object = None, token: str = "", *, timeout: float = 90) -> object:
    """Preserve codex-control's JSON and session-token request behavior."""
    request = Request(
        url.rstrip("/") + path,
        data=json.dumps(data).encode() if data is not None else None,
        headers={"Content-Type": "application/json", "X-Canvas-Token": token},
    )
    try:
        with open_request(request, timeout=timeout) as response:
            body = response.read()
    except IncompleteRead as error:
        raise ValueError(f"Studio returned an incomplete response for {path}") from error
    try:
        return json.loads(body)
    except (UnicodeDecodeError, ValueError) as error:
        raise ValueError(f"Studio answered with non-JSON for {path}") from error


def studio_json(url: str, path: str, token: str = "", *, timeout: float = 90) -> object:
    """Read a Studio endpoint and explain authentication and old-server failures."""
    try:
        return request_json(url, path, token=token, timeout=timeout)
    except HTTPError as error:
        non_json = False
        try:
            body = json.loads(error.read())
            detail = (body.get("error") or body.get("detail")) if isinstance(body, dict) else None
        except (IncompleteRead, UnicodeDecodeError, ValueError, OSError):
            detail = None
            non_json = True
        finally:
            error.close()
        if error.code == 401:
            message = f"Studio rejected the session token for {path} (HTTP {error.code})"
        elif error.code == 403:
            message = f"Studio denied the request to {path} (HTTP 403); check the local origin and session token"
        elif error.code == 404:
            message = (
                f"Studio endpoint {path} is unavailable (HTTP 404); "
                "the backend may be older than this client"
            )
        else:
            message = f"Studio request to {path} failed (HTTP {error.code})"
        if isinstance(detail, str) and detail:
            message += f": {detail}"
        if non_json:
            message += "; server answered with non-JSON"
        raise StudioHTTPError(message, error.code, detail if isinstance(detail, str) else None) from error


def session_token(url: str, *, timeout: float = 90) -> str:
    """Return the local API token from the narrow session endpoint."""
    response = studio_json(url, SESSION_ENDPOINT, timeout=timeout)
    token = response.get("token") if isinstance(response, dict) else None
    if not isinstance(token, str) or not token:
        raise ValueError("Studio session response did not include a token")
    return token


def desktop_state_dir(url: str, *, timeout: float = 90) -> str:
    """Return Studio's state directory identity from its desktop endpoint."""
    response = studio_json(url, DESKTOP_ENDPOINT, timeout=timeout)
    state_dir = response.get("stateDir") if isinstance(response, dict) else None
    if not isinstance(state_dir, str) or not state_dir:
        raise ValueError("Studio desktop response did not include a state directory identity")
    return state_dir


def pull_entities(url: str, token: str = "", *, timeout: float = 90) -> list[dict[str, object]]:
    """Read all entity values using bounded pages from the entity pull scope."""
    after = 0
    pages = 0
    latest: dict[tuple[str, str], tuple[int, dict[str, object] | None]] = {}
    while True:
        if pages >= ENTITY_PULL_MAX_PAGES:
            raise ValueError(
                f"Studio entity pull exceeded its {ENTITY_PULL_MAX_PAGES}-page limit "
                f"at checkpoint {after}"
            )
        path = f"{ENTITY_PULL_ENDPOINT}?scope={ENTITY_SCOPE}&after={after}&limit={ENTITY_PAGE_SIZE}"
        try:
            response = studio_json(url, path, token, timeout=timeout)
        except StudioHTTPError as error:
            if error.status_code == 400 and error.detail == "Invalid sync scope":
                raise ValueError(
                    f"Studio does not support the {ENTITY_SCOPE} pull scope; "
                    "the backend may be older than this client"
                ) from error
            raise
        if not isinstance(response, dict):
            raise ValueError("Studio entity pull returned an invalid response")
        documents = response.get("documents")
        checkpoint = response.get("checkpoint")
        maximum = response.get("maxSeq")
        sequence = checkpoint.get("seq") if isinstance(checkpoint, dict) else None
        if (not isinstance(documents, list) or isinstance(sequence, bool)
                or not isinstance(sequence, int) or isinstance(maximum, bool)
                or not isinstance(maximum, int)):
            raise ValueError("Studio entity pull returned an invalid page")
        for document in documents:
            if not isinstance(document, dict):
                raise ValueError("Studio entity pull returned an invalid document")
            payload = document.get("payload")
            if not isinstance(payload, str):
                raise ValueError("Studio entity pull returned an invalid entity payload")
            try:
                entity = json.loads(payload)
            except ValueError as error:
                raise ValueError("Studio entity pull returned malformed JSON") from error
            if (not isinstance(entity, dict) or not isinstance(entity.get("collection"), str)
                    or not isinstance(entity.get("id"), str) or not isinstance(entity.get("value"), dict)):
                raise ValueError("Studio entity pull returned an invalid entity value")
            document_sequence = document.get("seq")
            if isinstance(document_sequence, bool) or not isinstance(document_sequence, int):
                raise ValueError("Studio entity pull returned an invalid document sequence")
            key = (entity["collection"], entity["id"])
            previous = latest.get(key)
            if previous is None or document_sequence >= previous[0]:
                latest[key] = (
                    document_sequence,
                    None if document.get("_deleted") is True else entity,
                )
        pages += 1
        if sequence >= maximum:
            return [entry for _, entry in latest.values() if entry is not None]
        if sequence <= after:
            raise ValueError("Studio entity pull did not advance its checkpoint")
        after = sequence
