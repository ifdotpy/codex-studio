"""Execute the real exact-turn reader with deterministic native replies."""
import copy
import json
from oracle import turn
from codex_native_errors import NativeRpcError

item = dict(id="item", type="agentMessage", text="Full text\nwith Unicode λ and spacing.  ")
full = dict(id="turn", status="completed", items=[item], extra={"preserved": True})
summary = {**full, "items": [], "itemsView": "notLoaded"}
def page(value): return {"data": [value], "nextCursor": None}
cases = [
    ("full", [page(full)]),
    ("absent", [{"data": []}]),
    ("later-page", [{"data": [], "nextCursor": "next"}, page(full)]),
    ("coalesce", [page({**full, "items": [item, item]})]),
    ("conflict", [page({**full, "items": [item, {**item, "text": "different"}]})]),
    ("summary", [page(summary), {"data": [item]}, page(summary)]),
    ("item-pages", [page(summary), {"data": [item], "nextCursor": "next"}, {"data": [{"item": {**item, "id": "other"}, "turnId": "turn"}]}, page(summary)]),
    ("unsupported", [page(summary), {"rpcError": {"code": -32601}}, page(full)]),
    ("changed", [page(summary), {"data": [item]}, page({**summary, "status": "failed"})]),
    ("foreign", [page({**full, "items": [{**item, "threadId": "other"}]})]),
    ("cursor-cycle", [{"data": [], "nextCursor": "same"}, {"data": [], "nextCursor": "same"}]),
    ("wrapped-foreign", [page(summary), {"data": [{"item": item, "turnId": "other"}]}]),
]
output = []
for name, replies in cases:
    class Server:
        calls = []
        pending = copy.deepcopy(replies)
        def call(self, method, params, timeout):
            assert 0 < timeout <= 10
            self.calls.append({"method": method, "params": params})
            response = self.pending.pop(0)
            if "rpcError" in response: raise NativeRpcError(response["rpcError"])
            return response
    server = Server()
    try:
        result = turn.read_native_turn(server, "thread", "turn")
        failed = False
    except ValueError:
        result, failed = None, True
    output.append(dict(name=name, replies=replies, calls=server.calls, result=result, failed=failed))
print(json.dumps(output))
