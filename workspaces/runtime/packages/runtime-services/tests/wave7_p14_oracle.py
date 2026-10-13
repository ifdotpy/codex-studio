"""Real Python relay and token publisher oracle, without a Runtime or provider."""
import json
from pathlib import Path
import sys

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE / "turn_lifecycle"))
from oracle_guard import install_guard
install_guard(block_processes=True)
sys.path.insert(0, str(HERE.parents[2] / "apps/server/src"))
from fastapi import HTTPException
from pydantic import ValidationError
from studio_api.sync.resources.hub import ResourceHub
from studio_api.sync.resources.models import TokenRateSnapshot
from studio_api.sync.resources.relay.models import ResourceNotifyRequest
from studio_api.sync.resources.relay.receipts import NotifyReceipts

hub = ResourceHub("workspace")
receipts = NotifyReceipts()


def publish(resources):
    others = [resource for resource in resources if resource.root.kind != "state"]
    if others:
        hub.publish_many(others)
    if any(resource.root.kind == "state" for resource in resources):
        hub.publish_entity_sequence(0, reset=True)


steps = json.load(sys.stdin)
if len(sys.argv) > 1 and sys.argv[1] == "documents":
    import asyncio
    from studio_api.sync.resources.models import ResourceRef
    loop = asyncio.new_event_loop()
    documents = []
    for step in steps:
        current = ResourceHub("workspace", token_rates=TokenRateSnapshot.model_validate(step["rates"]), entity_sequence=step["sequence"])
        current.epoch = "epoch"
        subscription = current.subscribe([ResourceRef.model_validate(value) for value in step["resources"]], loop=loop, reconnect=step["reconnect"])
        documents.append({"resources": subscription.initial.model_dump(mode="json", by_alias=True, exclude_none=True),
                          "rates": subscription.initial_token_rates.model_dump(mode="json", by_alias=True, exclude_none=True)})
        subscription.close()
    loop.close()
    print(json.dumps(documents))
    sys.exit(0)

if len(sys.argv) > 1 and sys.argv[1] == "stream":
    import asyncio
    from studio_api.sync.resources.models import ResourceRef

    async def stream_documents():
        documents = []
        for case in steps:
            current = ResourceHub("workspace")
            current.epoch = "epoch"
            loop = asyncio.get_running_loop()
            subscription = current.subscribe([ResourceRef.model_validate(value) for value in case["resources"]], loop=loop)
            events = []
            for step in case["steps"]:
                if step["op"] == "rates":
                    current.publish_token_rates(TokenRateSnapshot.model_validate(step["body"]))
                elif step["op"] == "notify":
                    current.publish_many(ResourceRef.model_validate(value) for value in step["resources"])
                else:
                    gate = loop.create_future()
                    loop.call_soon(gate.set_result, None)
                    await gate
                    event = await subscription.next_event() if subscription._wake.is_set() else None
                    events.append(None if event is None else event.model_dump(mode="json", by_alias=True, exclude_none=True))
            subscription.close()
            documents.append(events)
        return documents

    print(json.dumps(asyncio.run(stream_documents())))
    sys.exit(0)

outputs = []
for step in steps:
    try:
        if step["op"] == "restart":
            hub = ResourceHub("workspace")
            receipts = NotifyReceipts()
            result = True
        elif step["op"] == "rates":
            before = hub._revision
            hub.publish_token_rates(TokenRateSnapshot.model_validate(step["body"]))
            result = hub._revision != before
        else:
            result = receipts.publish_once("workspace", ResourceNotifyRequest.model_validate(step["body"]), publish)
    except HTTPException as error:
        result = error.status_code
    except ValidationError:
        result = 400
    outputs.append({"result": result, "revision": hub._revision,
                    "rates": hub._token_rates.model_dump(mode="json", by_alias=True)})
print(json.dumps(outputs, ensure_ascii=False))
