"""Shared local HTTP transport for authenticated Codex Studio CLI requests."""

from __future__ import annotations

import json
from urllib.error import HTTPError
from urllib.request import Request, urlopen


SESSION_ENDPOINT = "/api/session"
DESKTOP_ENDPOINT = "/api/desktop"
ENTITY_PULL_ENDPOINT = "/api/sync/pull"
ENTITY_SCOPE = "state:entities:v1"
ENTITY_PAGE_SIZE = 500


def request_json(url: str, path: str, data: object = None, token: str = "", *, timeout: float = 90) -> object:
    """Preserve codex-control's JSON and session-token request behavior."""
    request = Request(
        url.rstrip("/") + path,
        data=json.dumps(data).encode() if data is not None else None,
        headers={"Content-Type": "application/json", "X-Canvas-Token": token},
    )
    with urlopen(request, timeout=timeout) as response:
        return json.load(response)


def studio_json(url: str, path: str, token: str = "", *, timeout: float = 90) -> object:
    """Read a Studio endpoint and explain authentication and old-server failures."""
    try:
        return request_json(url, path, token=token, timeout=timeout)
    except HTTPError as error:
        try:
            body = json.load(error)
            detail = body.get("error") if isinstance(body, dict) else None
        except (ValueError, OSError):
            detail = None
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
        raise ValueError(message) from error


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
    entities: list[dict[str, object]] = []
    while True:
        path = f"{ENTITY_PULL_ENDPOINT}?scope={ENTITY_SCOPE}&after={after}&limit={ENTITY_PAGE_SIZE}"
        try:
            response = studio_json(url, path, token, timeout=timeout)
        except ValueError as error:
            if "HTTP 400" in str(error):
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
            if not isinstance(document, dict) or document.get("_deleted") is True:
                continue
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
            entities.append(entity)
        if sequence >= maximum:
            return entities
        if sequence <= after:
            raise ValueError("Studio entity pull did not advance its checkpoint")
        after = sequence
