#!/usr/bin/env python3
"""Exercise native steering and history forks against a loopback Responses fixture.

Uses the installed Codex binary, a temporary CODEX_HOME, and no cloud provider.
"""

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from unittest.mock import patch

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
from codex_runtime import AppServer, THREAD_CONFIG, TOOLS


class Provider(ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self):
        super().__init__(("127.0.0.1", 0), ResponsesHandler)
        self.started = threading.Event()
        self.release = threading.Event()
        self.requests = []
        self.unexpected = []
        self.lock = threading.Lock()


class ResponsesHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def do_CONNECT(self):
        self.reject()

    def do_GET(self):
        self.reject()

    def reject(self):
        self.server.unexpected.append((self.command, self.path))
        self.send_response(502)
        self.send_header("Content-Length", "0")
        self.end_headers()

    def do_POST(self):
        if self.path != "/v1/responses":
            return self.reject()
        request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        with self.server.lock:
            self.server.requests.append(request)
            number = len(self.server.requests)
        response_id = f"resp_local_{number}"
        answer = f"Local fixture response {number}."
        item = {
            "id": f"msg_local_{number}",
            "type": "message",
            "role": "assistant",
            "status": "completed",
            "content": [{"type": "output_text", "text": answer, "annotations": []}],
        }
        response = {
            "id": response_id,
            "object": "response",
            "created_at": 1,
            "status": "completed",
            "output": [item],
            "model": request["model"],
            "usage": {
                "input_tokens": 10,
                "output_tokens": 5,
                "total_tokens": 15,
                "input_tokens_details": {"cached_tokens": 0},
                "output_tokens_details": {"reasoning_tokens": 0},
            },
        }
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Connection", "close")
        self.end_headers()
        try:
            self.event(
                "response.created",
                response={**response, "status": "in_progress", "output": []},
            )
            if number == 1:
                self.server.started.set()
                if not self.server.release.wait(15):
                    raise TimeoutError(
                        "The test did not release its first model response"
                    )
            self.event(
                "response.output_item.added",
                output_index=0,
                item={**item, "status": "in_progress", "content": []},
            )
            self.event(
                "response.content_part.added",
                item_id=item["id"],
                output_index=0,
                content_index=0,
                part={"type": "output_text", "text": "", "annotations": []},
            )
            self.event(
                "response.output_text.delta",
                item_id=item["id"],
                output_index=0,
                content_index=0,
                delta=answer,
            )
            self.event(
                "response.output_text.done",
                item_id=item["id"],
                output_index=0,
                content_index=0,
                text=answer,
            )
            self.event(
                "response.content_part.done",
                item_id=item["id"],
                output_index=0,
                content_index=0,
                part=item["content"][0],
            )
            self.event("response.output_item.done", output_index=0, item=item)
            self.event("response.completed", response=response)
        except (BrokenPipeError, ConnectionResetError):
            # A native steer can cancel the stream before the next model request.
            pass
        finally:
            self.close_connection = True

    def event(self, event_type, **fields):
        value = {"type": event_type, **fields}
        self.wfile.write(f"event: {event_type}\ndata: {json.dumps(value)}\n\n".encode())
        self.wfile.flush()


def until(predicate, description, timeout=20):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = predicate()
        if value:
            return value
        time.sleep(0.02)
    raise AssertionError(f"Timed out: {description}")


def run():
    with tempfile.TemporaryDirectory(
        prefix="codex-workspace-native-turn-"
    ) as directory:
        root = Path(directory)
        home = root / "codex-home"
        home.mkdir()
        project = root / "project"
        project.mkdir()
        subprocess.run(["git", "init", "-q", str(project)], check=True)
        provider = Provider()
        provider_thread = threading.Thread(target=provider.serve_forever, daemon=True)
        provider_thread.start()
        endpoint = f"http://127.0.0.1:{provider.server_port}"
        (home / "config.toml").write_text(f"""model = "gpt-5.6-sol"
model_provider = "local-probe"
[features]
plugins = false
remote_plugin = false
apps = false
skip_host_skill_discovery = true
[model_providers.local-probe]
name = "Local protocol fixture"
base_url = "{endpoint}/v1"
wire_api = "responses"
requires_openai_auth = false
request_max_retries = 0
stream_max_retries = 0
""")
        notifications = []
        requests = []
        server = None
        # A loopback proxy rejects accidental external HTTP requests. The Codex
        # home is isolated, so the process cannot use the user's saved login.
        environment = {
            "CODEX_HOME": str(home),
            "HTTP_PROXY": endpoint,
            "HTTPS_PROXY": endpoint,
            "ALL_PROXY": endpoint,
            "http_proxy": endpoint,
            "https_proxy": endpoint,
            "all_proxy": endpoint,
            "NO_PROXY": "127.0.0.1,localhost",
            "no_proxy": "127.0.0.1,localhost",
            "OPENAI_API_KEY": "",
            "CODEX_API_KEY": "",
        }
        try:
            with patch.dict(os.environ, environment):
                server = AppServer(
                    root, notifications.append, requests.append, lambda: None
                )
                started = server.call(
                    "thread/start",
                    {
                        "cwd": str(project),
                        "model": "gpt-5.6-sol",
                        "config": {**THREAD_CONFIG, "model_provider": "local-probe"},
                        "dynamicTools": TOOLS,
                        "approvalPolicy": "never",
                        "sandbox": "read-only",
                    },
                )
                thread_id = started["thread"]["id"]
                turn = server.call(
                    "turn/start",
                    {
                        "threadId": thread_id,
                        "input": [{"type": "text", "text": "native-initial-probe"}],
                    },
                )
                turn_id = turn["turn"]["id"]
                assert provider.started.wait(15), (
                    "No local Responses request",
                    notifications,
                )
                steered = server.call(
                    "turn/steer",
                    {
                        "threadId": thread_id,
                        "expectedTurnId": turn_id,
                        "input": [{"type": "text", "text": "native-steer-probe"}],
                    },
                )
                assert steered["turnId"] == turn_id, steered
                provider.release.set()
                completed = until(
                    lambda: next(
                        (
                            message
                            for message in notifications
                            if message.get("method") == "turn/completed"
                            and message.get("params", {}).get("turn", {}).get("id")
                            == turn_id
                        ),
                        None,
                    ),
                    "the native turn to complete",
                )
                assert completed["params"]["turn"]["status"] == "completed", completed
                assert "native-steer-probe" in json.dumps(
                    provider.requests[1:]
                ), provider.requests
                later = server.call(
                    "turn/start",
                    {
                        "threadId": thread_id,
                        "input": [
                            {"type": "text", "text": "native-later-turn-excluded"}
                        ],
                    },
                )
                until(
                    lambda: any(
                        message.get("method") == "turn/completed"
                        and message.get("params", {}).get("turn", {}).get("id")
                        == later["turn"]["id"]
                        and message["params"]["turn"]["status"] == "completed"
                        for message in notifications
                    ),
                    "the second native turn to complete",
                )
                original = server.call(
                    "thread/read", {"threadId": thread_id, "includeTurns": True}
                )
                assert "native-steer-probe" in json.dumps(original), original
                assert "native-later-turn-excluded" in json.dumps(original), original
                forked = server.call(
                    "thread/fork",
                    {
                        "threadId": thread_id,
                        "lastTurnId": turn_id,
                        "cwd": str(project),
                        "config": {**THREAD_CONFIG, "model_provider": "local-probe"},
                    },
                )
                fork_id = forked["thread"]["id"]
                assert fork_id != thread_id, forked
                history = server.call(
                    "thread/read", {"threadId": fork_id, "includeTurns": True}
                )
                assert history["thread"]["turns"], history
                for marker in (
                    "native-initial-probe",
                    "native-steer-probe",
                    "Local fixture response",
                ):
                    assert marker in json.dumps(history), (marker, history)
                assert "native-later-turn-excluded" not in json.dumps(history), history
                assert requests == [], requests
                assert provider.unexpected == [], provider.unexpected
                assert all(
                    request["model"] == "gpt-5.6-sol" for request in provider.requests
                )
                print(
                    f"PASS: real Codex active turn/steer and completed thread/fork history; "
                    f"{len(provider.requests)} local Responses requests; no external requests."
                )
        except Exception:
            provider.release.set()
            print(
                (root / "app-server.log").read_text(errors="replace")[-12000:],
                file=sys.stderr,
            )
            raise
        finally:
            provider.release.set()
            if server:
                server.close()
            provider.shutdown()
            provider.server_close()
            provider_thread.join(2)


if __name__ == "__main__":
    run()
