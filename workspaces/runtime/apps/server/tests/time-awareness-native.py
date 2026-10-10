#!/usr/bin/env python3
"""Real Codex clock boundaries and immutable prefixes, with a loopback provider.

No cloud model calls. Source contract:
https://github.com/openai/codex/blob/rust-v0.153.4/codex-rs/core/src/session/time_reminder.rs
"""

import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading
from unittest.mock import patch

spec = importlib.util.spec_from_file_location(
    "native_fixture", Path(__file__).with_name("workspace-native-turn.py")
)
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)


class ClockHandler(f.ResponsesHandler):
    def do_POST(self):
        if self.path != "/v1/responses":
            return self.reject()
        request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        self.server.requests.append(request)
        number = len(self.server.requests)
        calls = [
            {
                "type": "function_call",
                "name": "exec_command",
                "arguments": json.dumps(
                    {"cmd": "printf clock-success", "max_output_tokens": 100}
                ),
            },
            {
                "type": "function_call",
                "name": "exec_command",
                "arguments": json.dumps(
                    {"cmd": "printf clock-failure; exit 7", "max_output_tokens": 100}
                ),
            },
            {
                "type": "custom_tool_call",
                "name": "apply_patch",
                "input": "invalid patch",
            },
            {"type": "function_call", "name": "clock_unknown_tool", "arguments": "{}"},
        ]
        if number <= len(calls):
            item = {
                "id": f"fc_{number}",
                "call_id": f"call_{number}",
                "status": "completed",
                **calls[number - 1],
            }
        else:
            item = {
                "id": f"msg_{number}",
                "type": "message",
                "role": "assistant",
                "status": "completed",
                "content": [
                    {
                        "type": "output_text",
                        "text": "Clock fixture complete.",
                        "annotations": [],
                    }
                ],
            }
        response = {
            "id": f"resp_clock_{number}",
            "object": "response",
            "status": "completed",
            "model": request["model"],
            "output": [item],
            "usage": {"input_tokens": 1000, "output_tokens": 1, "total_tokens": 1001},
        }
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Connection", "close")
        self.end_headers()
        self.event(
            "response.created",
            response={**response, "status": "in_progress", "output": []},
        )
        self.event(
            "response.output_item.added",
            output_index=0,
            item={**item, "status": "in_progress"},
        )
        self.event("response.output_item.done", output_index=0, item=item)
        self.event("response.completed", response=response)
        self.close_connection = True


def main():
    with tempfile.TemporaryDirectory(prefix="codex-native-clock-") as directory:
        root = Path(directory)
        home, project = root / "home", root / "project"
        home.mkdir()
        project.mkdir()
        subprocess.run(["git", "init", "-q", str(project)], check=True)
        provider = f.Provider()
        provider.RequestHandlerClass = ClockHandler
        threading.Thread(target=provider.serve_forever, daemon=True).start()
        endpoint = f"http://127.0.0.1:{provider.server_port}"
        (home / "config.toml").write_text(f"""model = "gpt-5.6-sol"
model_provider = "local-clock"
[features]
plugins = false
remote_plugin = false
apps = false
skip_host_skill_discovery = true
[model_providers.local-clock]
name = "Local clock fixture"
base_url = "{endpoint}/v1"
wire_api = "responses"
requires_openai_auth = false
request_max_retries = 0
stream_max_retries = 0
""")
        environment = {
            "CODEX_HOME": str(home),
            "OPENAI_API_KEY": "",
            "CODEX_API_KEY": "",
            "NO_PROXY": "127.0.0.1,localhost",
            "no_proxy": "127.0.0.1,localhost",
        }
        environment.update(
            {
                key: endpoint
                for key in (
                    "HTTP_PROXY",
                    "HTTPS_PROXY",
                    "ALL_PROXY",
                    "http_proxy",
                    "https_proxy",
                    "all_proxy",
                )
            }
        )
        notifications = []
        server = None
        try:
            with patch.dict(os.environ, environment):
                server = f.AppServer(
                    root, notifications.append, lambda _: None, lambda: None
                )
                started = server.call(
                    "thread/start",
                    {
                        "cwd": str(project),
                        "model": "gpt-5.6-sol",
                        "config": {**f.THREAD_CONFIG, "model_provider": "local-clock"},
                        "developerInstructions": "Stable instructions. " * 1200,
                        "approvalPolicy": "never",
                        "sandbox": "danger-full-access",
                    },
                )
                thread = started["thread"]["id"]
                turn = server.call(
                    "turn/start",
                    {
                        "threadId": thread,
                        "input": [
                            {"type": "text", "text": "Test every clock boundary."}
                        ],
                    },
                )["turn"]["id"]
                completed = f.until(
                    lambda: next(
                        (
                            n
                            for n in notifications
                            if n.get("method") == "turn/completed"
                            and n["params"]["turn"]["id"] == turn
                        ),
                        None,
                    ),
                    "clock fixture completion",
                )
                assert completed["params"]["turn"]["status"] == "completed", completed
                assert len(provider.requests) == 5, len(provider.requests)
                for index, request in enumerate(provider.requests):
                    items = request["input"]
                    reminders = [
                        i for i in items if "<current_time_reminder>" in json.dumps(i)
                    ]
                    assert len(reminders) == index + 1, (index, reminders)
                    if index:
                        previous = provider.requests[index - 1]
                        assert (
                            items[: len(previous["input"])] == previous["input"]
                        ), f"Earlier input changed at request {index}"
                        assert request.get("tools") == previous.get(
                            "tools"
                        ), "Tool schemas changed"
                        assert request.get("instructions") == previous.get(
                            "instructions"
                        ), "Instructions changed"
                final_items = provider.requests[-1]["input"]
                outputs = [
                    i
                    for i in final_items
                    if i.get("type")
                    in {"function_call_output", "custom_tool_call_output"}
                ]
                assert len(outputs) == 4, outputs
                assert "clock-success" in json.dumps(outputs[0]), outputs[0]
                assert "clock-failure" in json.dumps(outputs[1]), outputs[1]
                executions = [
                    n["params"]["item"]
                    for n in notifications
                    if n.get("method") == "item/completed"
                    and n.get("params", {}).get("item", {}).get("type")
                    == "commandExecution"
                ]
                assert [item.get("exitCode") for item in executions] == [
                    0,
                    7,
                ], executions
                assert outputs[2]["type"] == "custom_tool_call_output", outputs[2]
                assert "invalid" in json.dumps(outputs[2]).lower(), outputs[2]
                assert (
                    "unsupported" in json.dumps(outputs[3]).lower()
                    or "unknown" in json.dumps(outputs[3]).lower()
                ), outputs[3]
                # Resume from disk in a new native process. Earlier reminders must
                # remain unchanged; only the newly accepted input adds a reminder.
                original_input = provider.requests[-1]["input"]
                server.close()
                server = f.AppServer(
                    root, notifications.append, lambda _: None, lambda: None
                )
                server.call(
                    "thread/resume",
                    {
                        "threadId": thread,
                        "config": {**f.THREAD_CONFIG, "model_provider": "local-clock"},
                    },
                )
                resumed = server.call(
                    "turn/start",
                    {
                        "threadId": thread,
                        "input": [{"type": "text", "text": "Continue after restart."}],
                    },
                )["turn"]["id"]
                f.until(
                    lambda: next(
                        (
                            n
                            for n in notifications
                            if n.get("method") == "turn/completed"
                            and n["params"]["turn"]["id"] == resumed
                        ),
                        None,
                    ),
                    "resumed clock fixture completion",
                )
                assert len(provider.requests) == 6
                replay = provider.requests[-1]["input"]
                assert (
                    replay[: len(original_input)] == original_input
                ), "Restart rewrote earlier clock metadata"
                assert (
                    len(
                        [
                            i
                            for i in replay
                            if "<current_time_reminder>" in json.dumps(i)
                        ]
                    )
                    == 6
                )
                assert not provider.unexpected, provider.unexpected
                print(
                    "PASS: native user, successful command, exit 7, failed freeform, unknown tool; 6 local requests; stable full prefixes, schemas, instructions and restart history; no external calls."
                )
        finally:
            if server:
                server.close()
            provider.shutdown()
            provider.server_close()


if __name__ == "__main__":
    main()
