"""Bounded process-local exact request identity receipts for relay retries."""

from __future__ import annotations

from collections import OrderedDict
import json
import threading
from collections.abc import Callable, Iterable, Sequence

from fastapi import HTTPException

from studio_api.sync.resources.models import ResourceRef
from studio_api.sync.resources.relay.models import ResourceNotifyRequest

MAX_NOTIFY_RECEIPTS = 256


def _normalized_resources(resources: Iterable[ResourceRef]) -> tuple[ResourceRef, ...]:
    """Canonicalize order and duplicates because invalidation is a set operation."""
    by_key = {
        json.dumps(resource.model_dump(mode="json", by_alias=True), sort_keys=True, separators=(",", ":")): resource
        for resource in resources
    }
    return tuple(by_key[key] for key in sorted(by_key))


def _body_signature(resources: Iterable[ResourceRef]) -> str:
    return json.dumps(
        [resource.model_dump(mode="json", by_alias=True) for resource in _normalized_resources(resources)],
        sort_keys=True,
        separators=(",", ":"),
        ensure_ascii=False,
    )


class NotifyReceipts:
    """Serialize publication and retain a bounded set of exact request receipts."""

    def __init__(self, capacity: int = MAX_NOTIFY_RECEIPTS) -> None:
        if capacity < 1:
            raise ValueError("Receipt capacity must be positive")
        self._capacity = capacity
        self._lock = threading.RLock()
        self._receipts: OrderedDict[tuple[str, str], str] = OrderedDict()

    def publish_once(
        self,
        workspace_id: str,
        request: ResourceNotifyRequest,
        publish: Callable[[Sequence[ResourceRef]], object],
    ) -> bool:
        """Publish once for this process, returning false for an exact replay.

        `publish` is called only for a new receipt. It must raise before a receipt
        is stored when publication fails so a client can retry the same identity.
        """
        identity = (workspace_id, request.requestId)
        signature = _body_signature(request.resources)
        with self._lock:
            previous = self._receipts.get(identity)
            if previous is not None:
                if previous != signature:
                    raise HTTPException(status_code=409, detail="Request identity already has different resources")
                self._receipts.move_to_end(identity)
                return False

            publish(_normalized_resources(request.resources))
            self._receipts[identity] = signature
            if len(self._receipts) > self._capacity:
                self._receipts.popitem(last=False)
            return True


notify_receipts = NotifyReceipts()
