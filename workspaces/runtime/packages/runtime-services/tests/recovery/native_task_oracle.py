"""Native task proof decisions from the production reader, preserving duplicates."""
import copy
import json
import time
from types import SimpleNamespace
from task_oracle import output, task

agent, work = output[0]["agent"], output[0]["task"]
results = []
for kind in ("commandExecution", "fileChange", "dynamicToolCall", "mcpToolCall"):
    for status in ("completed", "failed", "declined", "inProgress"):
        for count in (1, 2):
            stored = {**work, "type": kind, "kind": "command" if kind == "commandExecution" else "tool"}
            item = dict(id="item", type=kind, status=status, contentItems=[dict(type="inputText", text="native complete text")])
            entries = [dict(turnId="turn", item=item)] * count
            class Server:
                calls = []
                def call(self, method, params, timeout):
                    self.calls.append(dict(method=method,params=params))
                    return dict(data=copy.deepcopy(entries))
                def after_events(self, callback): callback()
            server = Server()
            job=SimpleNamespace(server=server, connection="connection", thread="thread", task=stored, absence_source=None)
            proofs=task._native_proofs(job,[job],time.monotonic()+5,[])
            results.append(dict(agent=agent, task=stored, entries=entries, expected=proofs.get(stored["id"]), calls=server.calls))
print(json.dumps(results))
