"""Authenticated API endpoint for invalidations from external CLI writers."""

from __future__ import annotations

from collections.abc import Sequence
from typing import TYPE_CHECKING, Any

from fastapi import APIRouter, HTTPException, Request

from studio_api.models import ErrorResponse
from studio_api.responses import register_route_components
from studio_api.sync.resources.models import ResourceRef
from studio_api.sync.resources.relay.models import ResourceNotifyAck, ResourceNotifyRequest
from studio_api.sync.resources.relay.receipts import notify_receipts

if TYPE_CHECKING:
    from studio_api.context import ApiContext


ERROR_RESPONSES: dict[int | str, dict[str, Any]] = {
    status: {"model": ErrorResponse} for status in (400, 403, 409, 413, 415, 500, 503)
}


def create_router(context: ApiContext) -> APIRouter:
    router = APIRouter()

    @router.post(
        "/api/sync/notify",
        response_model=ResourceNotifyAck,
        responses=ERROR_RESPONSES,
    )
    def notify(request: Request, payload: ResourceNotifyRequest) -> object:
        try:
            hub = context.resource_hub()

            def publish(resources: Sequence[ResourceRef]) -> None:
                state_change = False
                other_resources = []
                for resource in resources:
                    if resource.root.kind == "state":
                        state_change = True
                    else:
                        other_resources.append(resource)
                if other_resources:
                    hub.publish_many(other_resources)
                if state_change:
                    hub.publish_entity_sequence(context.entity_sequence() or 0, reset=True)

            notify_receipts.publish_once(
                hub.workspace_id,
                payload,
                publish,
            )
        except HTTPException:
            raise
        except Exception as error:
            raise HTTPException(status_code=503, detail="Resource invalidation could not be published") from error
        return ResourceNotifyAck(requestId=payload.requestId, accepted=True)

    register_route_components(
        router.routes[-1],
        {
            "ResourceRef": ResourceRef.model_json_schema(ref_template="#/components/schemas/{model}"),
            "ResourceNotifyRequest": ResourceNotifyRequest.model_json_schema(
                ref_template="#/components/schemas/{model}"
            ),
            "ResourceNotifyAck": ResourceNotifyAck.model_json_schema(
                ref_template="#/components/schemas/{model}"
            ),
        },
    )
    return router
