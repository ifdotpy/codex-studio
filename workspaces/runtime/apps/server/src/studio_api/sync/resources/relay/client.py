"""One-shot authenticated CLI notification client with safe notify-only retry."""

from __future__ import annotations

import json
import os
from pathlib import Path
import time
from http.client import HTTPException
from urllib.error import HTTPError, URLError

from codex_api_client import desktop_state_dir, request_json, session_token
from pydantic import ValidationError

from studio_api.sync.resources.models import ResourceRef
from studio_api.sync.resources.relay.models import ResourceNotifyAck, ResourceNotifyRequest

DEFAULT_CANVAS_URL = "http://127.0.0.1:4620"
NOTIFY_ENDPOINT = "/api/sync/notify"
NOTIFY_TIMEOUT_SECONDS = 5
NOTIFY_ATTEMPTS = 2
NOTIFY_RETRY_DELAY_SECONDS = 0.1
RECONNECT_GUIDANCE = "Reconnect the event stream to receive a full resource baseline."


class NotifyCommittedWriteError(RuntimeError):
    """The source write committed, but its UI invalidation was not confirmed."""

    def __init__(self, message: str) -> None:
        super().__init__(f"{message}. {RECONNECT_GUIDANCE}")


class ResourceRelayClient:
    """Lazy token bootstrap reused by one explicit CLI action."""

    def __init__(self, source_root: str | Path, url: str | None = None) -> None:
        self.url = url or os.environ.get("CODEX_CANVAS_URL", DEFAULT_CANVAS_URL)
        self.source_root = Path(source_root).expanduser().resolve()
        self._token: str | None = None

    def _session_token(self) -> str:
        if self._token is not None:
            return self._token
        try:
            token = session_token(self.url, timeout=NOTIFY_TIMEOUT_SECONDS)
            state_dir = desktop_state_dir(self.url, timeout=NOTIFY_TIMEOUT_SECONDS)
            if Path(state_dir).expanduser().resolve() != self.source_root:
                raise NotifyCommittedWriteError(
                    f"Source state {self.source_root} does not match Studio API state {Path(state_dir).resolve()}"
                )
            self._token = token
            return token
        except NotifyCommittedWriteError:
            raise
        except Exception as error:
            raise NotifyCommittedWriteError(f"Studio API session/workspace bootstrap failed: {error}") from error

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
                    try:
                        error_body = json.loads(error.read())
                        detail = error_body.get("error") if isinstance(error_body, dict) else None
                    except (OSError, UnicodeDecodeError, ValueError):
                        detail = None
                    raise NotifyCommittedWriteError(
                        f"Studio rejected the committed write's invalidation (HTTP {error.code})"
                        + (f": {detail}" if isinstance(detail, str) and detail else "")
                    ) from error
                last_error = error
            except (URLError, HTTPException, TimeoutError, OSError, ValidationError, ValueError) as error:
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
    source_root: str | Path,
    url: str | None = None,
) -> ResourceNotifyAck:
    """Convenience call for one committed write."""
    return ResourceRelayClient(source_root, url).notify(request_id, resources)
