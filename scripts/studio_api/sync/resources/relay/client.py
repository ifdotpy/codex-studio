"""One-shot authenticated CLI notification client with safe notify-only retry."""

from __future__ import annotations

import os
import time
from urllib.error import HTTPError, URLError

from codex_api_client import request_json
from pydantic import ValidationError

from studio_api.sync.resources.models import ResourceRef
from studio_api.sync.resources.relay.models import ResourceNotifyAck, ResourceNotifyRequest

DEFAULT_CANVAS_URL = "http://127.0.0.1:4620"
NOTIFY_ENDPOINT = "/api/sync/notify"
API_STATE_ENDPOINT = "/api/state"
NOTIFY_TIMEOUT_SECONDS = 5
NOTIFY_ATTEMPTS = 2
NOTIFY_RETRY_DELAY_SECONDS = 0.1


class NotifyCommittedWriteError(RuntimeError):
    """The source write committed, but its UI invalidation was not confirmed."""


class ResourceRelayClient:
    """Lazy token bootstrap reused by one explicit CLI action."""

    def __init__(self, url: str | None = None) -> None:
        self.url = url or os.environ.get("CODEX_CANVAS_URL", DEFAULT_CANVAS_URL)
        self._token: str | None = None

    def _session_token(self) -> str:
        if self._token is not None:
            return self._token
        try:
            state = request_json(self.url, API_STATE_ENDPOINT, timeout=NOTIFY_TIMEOUT_SECONDS)
            token = state.get("token") if isinstance(state, dict) else None
            if not isinstance(token, str) or not token:
                raise NotifyCommittedWriteError("Studio returned no local session token")
            self._token = token
            return token
        except NotifyCommittedWriteError:
            raise
        except Exception as error:
            raise NotifyCommittedWriteError(f"Studio API token bootstrap failed: {error}") from error

    def notify(self, request_id: str, resources: list[ResourceRef]) -> ResourceNotifyAck:
        """Send one typed invalidation; retries never repeat the source operation."""
        payload = ResourceNotifyRequest(requestId=request_id, resources=resources)
        body = payload.model_dump(mode="json", by_alias=True)
        token = self._session_token()
        last_error: Exception | None = None
        for attempt in range(NOTIFY_ATTEMPTS):
            try:
                response = request_json(
                    self.url,
                    NOTIFY_ENDPOINT,
                    body,
                    token,
                    timeout=NOTIFY_TIMEOUT_SECONDS,
                )
                ack = ResourceNotifyAck.model_validate(response)
                if ack.requestId != request_id:
                    raise NotifyCommittedWriteError("Studio acknowledged a different notification identity")
                return ack
            except HTTPError as error:
                if error.code < 500 and error.code != 429:
                    raise NotifyCommittedWriteError(
                        f"Studio rejected the committed write's invalidation (HTTP {error.code})"
                    ) from error
                last_error = error
            except (URLError, TimeoutError, OSError, ValidationError, ValueError) as error:
                last_error = error
            except NotifyCommittedWriteError:
                raise
            if attempt + 1 < NOTIFY_ATTEMPTS:
                time.sleep(NOTIFY_RETRY_DELAY_SECONDS)

        raise NotifyCommittedWriteError(
            f"Studio did not confirm resource invalidation after {NOTIFY_ATTEMPTS} attempts: {last_error}"
        ) from last_error


def notify_external_write(
    request_id: str,
    resources: list[ResourceRef],
    *,
    url: str | None = None,
) -> ResourceNotifyAck:
    """Convenience call for one committed write."""
    return ResourceRelayClient(url).notify(request_id, resources)
