"""Python-written browser bridge and browser configuration differential corpus."""
import base64
import contextlib
import json
import os
from pathlib import Path
import socket
import sys
import tempfile
import threading
from types import SimpleNamespace
from unittest.mock import patch

HERE = Path(__file__).resolve()
PACKAGE = HERE.parents[2]
SERVER = PACKAGE / "../../apps/server/src"
VM_GUEST = PACKAGE / "../../apps/vm-guest"
SHARED = HERE.parents[1] / "turn_lifecycle"
sys.path.insert(0, str(SHARED))
from oracle_guard import install_guard  # noqa: E402

guard = install_guard(block_processes=True, allow_loopback=True)
with contextlib.redirect_stdout(sys.stderr):
    guard.self_test()

sys.path.insert(0, str(SERVER.resolve()))
sys.path.insert(0, str(VM_GUEST.resolve()))
import codex_browser  # noqa: E402
import codex_browser_bridge as bridge  # noqa: E402


def response_record(raw):
    if not raw:
        return None
    head, _, body = raw.partition(b"\r\n\r\n")
    lines = head.decode("latin-1").split("\r\n")
    status = int(lines[0].split(" ")[1])
    headers = {}
    for line in lines[1:]:
        name, _, value = line.partition(":")
        headers[name.lower()] = value.strip()
    return {"status": status, "headers": headers, "body": body.decode("utf-8")}


def run_forward(case):
    data = dict(server="peer", bridgeId="0123456789abcdef0123456789abcdef", token="0123456789abcdef",
                host="127.0.0.1", port=43210, path="/oauth/callback", expiresAt=2000.0)
    data.update(case.get("input", {}))
    calls = []
    default_reply = {"status": 200, "contentType": "text/plain; charset=iso-8859-1",
                     "body": base64.b64encode(b"accepted").decode("ascii"), "done": True}
    replies = iter(case.get("replies", [case.get("reply", default_reply)]))
    def exchange(server, action, payload):
        calls.append({"server": server, "action": action, "payload": payload})
        if case.get("unknown"):
            raise RuntimeError("unknown")
        return next(replies)
    runtime = SimpleNamespace(lock=threading.RLock())
    requests = case.get("requests", [case.get("request", b"GET /oauth/callback?code=example-code HTTP/1.1\r\n"
                    b"User-Agent: test-agent\r\nAccept: text/html\r\nAccept-Language: en\r\n"
                    b"Cookie: secret-cookie\r\nAuthorization: secret-auth\r\n\r\n")])
    with patch.object(bridge.time, "time", return_value=case.get("now", 1000.0)):
        try:
            result = bridge.open_remote_forward(runtime, data, exchange, listen_port=0)
            if case.get("replay"):
                result = bridge.open_remote_forward(runtime, data, exchange, listen_port=0)
            forward = runtime.__dict__["_browser_remote_forwards"][data["bridgeId"]]
            responses = []
            for raw in requests:
                client = socket.create_connection(("127.0.0.1", forward.bound_port), timeout=2)
                client.settimeout(2)
                chunks_to_send = raw if isinstance(raw, list) else [raw]
                for chunk in chunks_to_send:
                    client.sendall(chunk)
                wire = b"".join(chunks_to_send)
                if b"\r\n\r\n" not in wire:
                    client.shutdown(socket.SHUT_WR)
                chunks = []
                while True:
                    try:
                        chunk = client.recv(65536)
                    except ConnectionResetError:
                        break
                    if not chunk:
                        break
                    chunks.append(chunk)
                client.close()
                responses.append(response_record(b"".join(chunks)))
            return {"open": result, "responses": responses, "closed": forward.closed, "calls": calls}
        except Exception as error:
            return {"error": {"type": type(error).__name__, "message": str(error)}, "calls": calls}
        finally:
            for forward in list(runtime.__dict__.get("_browser_remote_forwards", {}).values()):
                forward.close("oracle-cleanup")


def config_corpus(root):
    cases = []
    for name, target_bits, shared_bits, skill, runtime, valid_services, service, expected_reason in [
        ("enabled-default", {}, {}, True, True, True, True, None),
        ("plugins-disabled", {"features": {"plugins": False}}, {}, True, True, True, True,
         "Plugins are disabled in this account's Codex configuration"),
        ("plugin-disabled", {"plugins": {codex_browser.PLUGIN: {"enabled": False}}}, {}, True, True, True, True,
         "The Chrome plugin is disabled in this account's Codex configuration"),
        ("node-disabled", {"mcp_servers": {"node_repl": {"enabled": False}}}, {}, True, True, True, True,
         "node_repl is disabled in this account's Codex configuration"),
        ("custom-runtime", {"mcp_servers": {"node_repl": {"command": "/different/node"}}}, {}, True, True, True, True,
         "This account uses its own node_repl runtime"),
        ("skill-missing", {}, {}, False, True, True, True,
         "The control-chrome skill for the installed Chrome plugin version was not found"),
        ("runtime-missing", {}, {}, True, False, True, True,
         "The node_repl runtime from the Codex configuration is missing"),
        ("invalid-services", {}, {}, True, True, False, True,
         "NODE_REPL_TRUSTED_SERVICES is not valid JSON"),
        ("service-missing", {}, {}, True, True, True, False,
         "The browser service file is missing"),
        ("account-node", {}, {}, True, True, True, True, None),
    ]:
        base = root / name
        home, shared_home = base / "account", base / "shared"
        home.mkdir(parents=True)
        shared_home.mkdir(parents=True)
        node_path, service_path = base / "node-runtime", base / "browser-service.mjs"
        node_path.write_text("fake runtime")
        service_path.write_text("fake service")
        plugin = shared_home / "plugins/cache/openai-bundled/chrome/latest/skills/control-chrome"
        plugin.mkdir(parents=True)
        (plugin / "SKILL.md").write_text("fixture")
        if not skill:
            (plugin / "SKILL.md").unlink()
        if not service:
            service_path.unlink()
        services = "not-json" if not valid_services else json.dumps({"browser": str(service_path)})
        shared_config = {"mcp_servers": {"node_repl": {
            "command": str(node_path) if runtime else str(base / "missing-node"),
            "args": ["node-repl.mjs"], "startup_timeout_sec": 5,
            "env": {"NODE_REPL_TRUSTED_SERVICES": services,
                    "NODE_REPL_TRUSTED_CODE_PATHS": str(base / "trusted"),
                    "NODE_REPL_NODE_PATH": str(base / "modules"), "SECRET": "omit-me"}}}}
        target_config = target_bits
        if name == "account-node":
            target_config = {"mcp_servers": {"node_repl": {
                "command": str(node_path), "env": {
                    "ACCOUNT_SETTING": "keep",
                    "NODE_REPL_TRUSTED_CODE_PATHS": os.pathsep.join(
                        ["", str(home), str(shared_home), str(home), ""]
                    ),
                }}}}
            target_bits = target_config
        if "node_repl" in target_bits.get("mcp_servers", {}) and target_bits["mcp_servers"]["node_repl"].get("command") == "/different/node":
            shared_config["mcp_servers"]["node_repl"]["command"] = str(node_path)
        target_toml = ""
        if "features" in target_bits:
            target_toml += "[features]\nplugins = false\n"
        if "plugins" in target_bits:
            target_toml += '[plugins."' + codex_browser.PLUGIN + '"]\nenabled = false\n'
        target_node = target_bits.get("mcp_servers", {}).get("node_repl")
        if target_node is not None:
            target_toml += "[mcp_servers.node_repl]\n"
            for key, value in target_node.items():
                if key == "env" and isinstance(value, dict):
                    target_toml += "\n[mcp_servers.node_repl.env]\n"
                    for env_key, env_value in value.items():
                        target_toml += env_key + " = " + json.dumps(env_value) + "\n"
                    continue
                target_toml += key + " = " + json.dumps(value) + "\n"
        (home / "config.toml").write_text(target_toml)
        (shared_home / "config.toml").write_text(
            "[mcp_servers.node_repl]\ncommand = " + json.dumps(shared_config["mcp_servers"]["node_repl"]["command"]) +
            "\nargs = [\"node-repl.mjs\"]\nstartup_timeout_sec = 5\n\n[mcp_servers.node_repl.env]\n" +
            "NODE_REPL_TRUSTED_SERVICES = " + json.dumps(services) + "\n" +
            "NODE_REPL_TRUSTED_CODE_PATHS = " + json.dumps(str(base / "trusted")) + "\n" +
            "NODE_REPL_NODE_PATH = " + json.dumps(str(base / "modules")) + "\nSECRET = \"omit-me\"\n")
        overrides, reason = codex_browser.browser_status(home, shared_home)
        cases.append({"name": name, "target": codex_browser.read_config(home),
                      "shared": codex_browser.read_config(shared_home),
                      "home": str(home.resolve()), "sharedHome": str(shared_home.resolve()),
                      "assets": {"controlChromeSkill": str((plugin / "SKILL.md").resolve()) if skill else None,
                                 "nodeRuntime": str(node_path.resolve()) if runtime else None,
                                 "browserService": str(service_path.resolve()) if service and valid_services else None},
                      "python": {"overrides": overrides, "reason": reason,
                                 "developerInstructions": codex_browser.GUIDANCE if overrides else None},
                      "expectedReason": expected_reason})
    return cases


def paired_exchange_cases():
    calls = []
    class Service:
        class Transport:
            @staticmethod
            def servers():
                return [{"id": "peer"}]

        transport = Transport()

        def queue(self, db, server, action, payload, key):
            calls.append({"phase": "queue", "server": server, "action": action,
                          "payload": payload, "requestId": key})
        def deliver(self, key):
            calls.append({"phase": "deliver", "requestId": key})
            return {"outcome": "unknown"}
    class Runtime:
        lock = threading.RLock()
        def multi_server(self):
            return Service()
        @contextlib.contextmanager
        def db(self):
            yield object()
    ids = [SimpleNamespace(hex="01" * 16), SimpleNamespace(hex="02" * 16)]
    results = []
    with patch.object(bridge.uuid, "uuid4", side_effect=ids):
        for _ in range(2):
            try:
                bridge.paired_exchange(Runtime())("peer", "browser_callback",
                    {"bridgeId": "0123456789abcdef0123456789abcdef", "token": "0123456789abcdef",
                     "target": "/oauth/callback?code=example-code", "headers": {"accept": "text/html"}})
            except Exception as error:
                results.append({"errorType": type(error).__name__})
    return {"results": results, "calls": calls}


def listener_lifecycle_witnesses():
    redirect = {"host": "127.0.0.1", "port": 43210, "path": "/oauth/callback"}
    expired_event = threading.Event()
    expired = bridge.Forward("expiry", redirect, lambda *_: {},
                             lambda *_: expired_event.set(), seconds=-1, listen_port=0)
    expired.bind()
    expired.start()
    expired_closed = expired_event.wait(2)

    runtime = SimpleNamespace(lock=threading.RLock())
    data = {"server": "peer", "bridgeId": "2123456789abcdef0123456789abcdef",
            "token": "0123456789abcdef", **redirect, "expiresAt": 2000.0}
    accepted = threading.Barrier(9)
    original_connection = bridge.Forward.connection

    def barrier_connection(forward, connection):
        accepted.wait(2)
        return original_connection(forward, connection)

    with patch.object(bridge.Forward, "connection", barrier_connection), \
            patch.object(bridge.time, "time", return_value=1000.0):
        bridge.open_remote_forward(runtime, data, lambda *_: {}, listen_port=0)
        forward = runtime.__dict__["_browser_remote_forwards"][data["bridgeId"]]
        clients = [socket.create_connection(("127.0.0.1", forward.bound_port), timeout=2)
                   for _ in range(8)]
        accepted.wait(2)
        ninth = socket.create_connection(("127.0.0.1", forward.bound_port), timeout=2)
        ninth.settimeout(2)
        ninth_closed = ninth.recv(1) == b""
        ninth.close()
        forward.close("oracle-cleanup")
        for client in clients:
            client.close()

    entered, release = threading.Event(), threading.Event()
    runtime = SimpleNamespace(lock=threading.RLock())
    data["bridgeId"] = "3123456789abcdef0123456789abcdef"

    def held_exchange(*_):
        entered.set()
        release.wait(2)
        return {"status": 200, "body": base64.b64encode(b"ok").decode(), "done": False}

    with patch.object(bridge.time, "time", return_value=1000.0):
        bridge.open_remote_forward(runtime, data, held_exchange, listen_port=0)
        forward = runtime.__dict__["_browser_remote_forwards"][data["bridgeId"]]
        result = []

        def client_call():
            client = socket.create_connection(("127.0.0.1", forward.bound_port), timeout=2)
            client.sendall(b"GET /oauth/callback?code=x HTTP/1.1\r\n\r\n")
            result.append(response_record(client.recv(4096)))
            client.close()

        client_thread = threading.Thread(target=client_call)
        client_thread.start()
        exchange_entered = entered.wait(2)
        forward.close("oracle-close")
        release.set()
        client_thread.join(2)
    return {"expiredClosed": expired_closed, "eightAdmitted": len(clients),
            "ninthClosed": ninth_closed, "exchangeEntered": exchange_entered,
            "responseAfterClose": bool(result and result[0] and result[0]["status"] == 200)}


def main():
    cases = [
        {"name": "allowed-headers", "request": b"GET /oauth/callback?code=example-code HTTP/1.1\r\nUser-Agent: test-agent\r\nAccept: text/html\r\nAccept-Language: en\r\nCookie: secret-cookie\r\nAuthorization: secret-auth\r\n\r\n"},
        {"name": "path-refusal", "request": b"GET /wrong?code=do-not-forward HTTP/1.1\r\nUser-Agent: test\r\n\r\n"},
        {"name": "method-refusal", "request": b"POST /oauth/callback HTTP/1.1\r\nUser-Agent: test\r\n\r\n"},
        {"name": "header-bound", "request": b"GET /oauth/callback HTTP/1.1\r\nUser-Agent: " + b"x" * 1025 + b"\r\nAccept: text/html\r\n\r\n"},
        {"name": "head-bound", "request": b"GET /oauth/callback HTTP/1.1\r\nX-Test: " + b"x" * (16 * 1024) + b"\r\n\r\n"},
        {"name": "redirect-response", "reply": {"status": 302, "location": "https://private.invalid/"}},
        {"name": "body-bound", "reply": {"status": 200, "body": base64.b64encode(b"x" * (256 * 1024 + 1)).decode("ascii")}},
        {"name": "encoded-body-bound", "reply": {"status": 200, "body": "A" * (256 * 1024 * 4 // 3 + 1)}},
        {"name": "unknown", "unknown": True},
        {"name": "duplicate-user-agent", "request": b"GET /oauth/callback?code=example-code HTTP/1.1\r\nUser-Agent: " + b"x" * 1025 + b"\r\nUser-Agent: short\r\n\r\n"},
        {"name": "user-agent-latin1-bound", "request": b"GET /oauth/callback?code=example-code HTTP/1.1\r\nUser-Agent: " + b"\xe9" * 1024 + b"\r\n\r\n"},
        {"name": "body-type-refusal", "reply": {"status": 200, "body": {}}},
        {"name": "charset-contenttype", "reply": {"status": 200,
            "contentType": "text/plain; charset=iso-8859-1 extra",
            "body": base64.b64encode(b"accepted").decode("ascii"), "done": True}},
        {"name": "incomplete-then-valid", "requests": [
            b"GET /oauth/callback HTTP/1.1\r\nUser-Agent: incomplete",
            b"GET /oauth/callback?code=example-code HTTP/1.1\r\n\r\n"]},
        {"name": "two-valid-relays", "requests": [
            b"GET /oauth/callback?code=first HTTP/1.1\r\n\r\n",
            b"GET /oauth/callback?code=second HTTP/1.1\r\n\r\n"],
         "replies": [{"status": 200, "body": base64.b64encode(b"first").decode(), "done": False},
                     {"status": 200, "body": base64.b64encode(b"second").decode(), "done": True}]},
        {"name": "redirect-done", "reply": {"status": 302, "done": True}},
        {"name": "fragmented-complete-head", "request": [
            b"GET /oauth/callback?code=fragmented HTTP/1.1\r\nUser-Agent: frag",
            b"mented-agent\r\nAccept: text/html\r\n\r\n"]},
        {"name": "duplicate-content-length", "request":
            b"GET /oauth/callback?code=duplicate HTTP/1.1\r\nContent-Length: 0\r\nContent-Length: 0\r\n\r\n"},
        {"name": "get-body", "request":
            b"GET /oauth/callback?code=body HTTP/1.1\r\nContent-Length: 3\r\n\r\nabc"},
        {"name": "fifth-relay", "requests": [
            b"GET /oauth/callback?code=relay HTTP/1.1\r\n\r\n" for _ in range(5)],
         "replies": [{"status": 200, "body": base64.b64encode(b"ok").decode(), "done": False}
                     for _ in range(5)]},
    ]
    forward = [{"name": case["name"], "python": run_forward(case)} for case in cases]
    # Expiry is a pre-bind refusal and must not create an active forward.
    expired = {"server": "peer", "bridgeId": "0123456789abcdef0123456789abcdef", "token": "0123456789abcdef",
               "host": "127.0.0.1", "port": 43210, "path": "/oauth/callback", "expiresAt": 999.0}
    with patch.object(bridge.time, "time", return_value=1000.0):
        try:
            bridge.open_remote_forward(SimpleNamespace(lock=threading.RLock()), expired, lambda *args: {}, listen_port=43210)
        except Exception as error:
            expiry = {"type": type(error).__name__, "message": str(error)}
        else:
            raise AssertionError("expired forward unexpectedly opened")
    # Replay and port conflict use the production listener on loopback ephemeral ports.
    replay_case = {"name": "replay", "replay": True}
    conflict_runtime = SimpleNamespace(lock=threading.RLock())
    conflict_data = {"server": "peer", "bridgeId": "0123456789abcdef0123456789abcdef", "token": "0123456789abcdef",
                     "host": "127.0.0.1", "port": 43210, "path": "/oauth/callback", "expiresAt": 2000.0}
    conflicts = []
    with patch.object(bridge.time, "time", return_value=1000.0):
        replay = run_forward(replay_case)
        first = bridge.open_remote_forward(conflict_runtime, conflict_data, lambda *args: {}, listen_port=0)
        second_data = dict(conflict_data, bridgeId="1123456789abcdef0123456789abcdef")
        try:
            bridge.open_remote_forward(conflict_runtime, second_data, lambda *args: {}, listen_port=0)
        except Exception as error:
            conflicts.append({"type": type(error).__name__, "message": str(error)})
        for value in list(conflict_runtime.__dict__.get("_browser_remote_forwards", {}).values()):
            value.close("oracle-cleanup")
    root = Path(tempfile.mkdtemp(prefix="browser-oracle-"))
    output = {"forward": forward, "expired": expiry, "replay": replay,
              "conflict": conflicts, "pairedExchange": paired_exchange_cases(),
              "lifecycle": listener_lifecycle_witnesses(),
              "config": config_corpus(root), "fixtureRoot": str(root)}
    print(json.dumps(output, sort_keys=True, separators=(",", ":")))


if __name__ == "__main__":
    main()
