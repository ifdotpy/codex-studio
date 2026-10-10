"""Canonical typed request and response contracts for external invalidations."""

from __future__ import annotations

from typing import Annotated, Literal

from pydantic import Field, StrictStr

from studio_api.models import ContractModel
from studio_api.sync.resources.models import ResourceRef

MAX_NOTIFY_REQUEST_ID_LENGTH = 200
MAX_NOTIFY_RESOURCES = 256
NotifyRequestId = Annotated[StrictStr, Field(min_length=1, max_length=MAX_NOTIFY_REQUEST_ID_LENGTH)]
NotifyResources = Annotated[list[ResourceRef], Field(min_length=1, max_length=MAX_NOTIFY_RESOURCES)]


class ResourceNotifyRequest(ContractModel):
    """One post-commit invalidation request from an external process."""

    requestId: NotifyRequestId
    resources: NotifyResources


class ResourceNotifyAck(ContractModel):
    """Acknowledgement for a newly published or exactly deduplicated request."""

    requestId: NotifyRequestId
    accepted: Literal[True]
