#!/usr/bin/env python3
"""Opt-in live check of sign-in links (docs/vm-browser-bridge.md, section 7). Uses real models.

A local OAuth and OpenID Connect provider serves a device code flow and an authorization code
flow with PKCE and a loopback redirect. For each selected provider (Codex with the configured
Codex account, Claude with the local Claude sign-in) and each flow, a lead in a throwaway Studio
state runs a sign-in command that only prints its link. The prompt asks the model to give the
link to the person and does not name a tool. The check passes when:

- a sign-in card (browser/open) for the lead shows the provider link,
- every sign-in link in the lead's messages has a card, so a pasted link is never the only path,
- after the test approves the card and acts as the browser, the command prints LOGIN_OK.

Run it alone, as an announced manual run: `just test-sign-in-live`. Each scenario has a
300-second budget and makes a few small model requests.
"""

# Support direct execution without runner-provided PYTHONPATH.
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))

import ast
import os
import tempfile


def _isolate():
    """Keep the throwaway Runtime away from the running Studio: no supervisor routing (the names
    of `SUPERVISOR_ENV` in test_isolation.py, read without its home guard, because this check
    uses the real provider sign-ins) and a private image workspace store."""
    source = ast.parse((Path(__file__).with_name("test_isolation.py")).read_text())
    names = next(ast.literal_eval(node.value) for node in source.body if isinstance(node, ast.Assign)
                 and any(getattr(target, "id", None) == "SUPERVISOR_ENV" for target in node.targets))
    for name in names:
        os.environ.pop(name, None)
    os.environ["CODEX_WORKSPACE_STORE"] = tempfile.mkdtemp(prefix="sil-live-store-")


_isolate()

import argparse
import base64
import hashlib
import http.server
import json
import secrets
import shutil
import threading
import time
import urllib.error
import urllib.parse
import urllib.request

sys.dont_write_bytecode = True
from codex_runtime import Runtime  # noqa: E402
import codex_browser_bridge  # noqa: E402
from codex_browser_bridge import Bridge, card_id  # noqa: E402
from codex_browser_host import HostGuest  # noqa: E402
from browser_policy import sign_in_links  # noqa: E402
from studio_api.testing import read_runtime_records, read_runtime_state  # noqa: E402

CLI = Path(__file__).resolve().parent / "fixtures" / "sign_in_live_cli.py"
SCENARIO_SECONDS = 300


class Provider:
    """The identity provider: it signs the person in at once and checks PKCE and device codes."""

    def __init__(self):
        self.codes, self.devices, self.tokens = {}, {}, 0
        provider = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def send(self, status, body, content_type="application/json", location=None):
                data = body if isinstance(body, bytes) else json.dumps(body).encode()
                self.send_response(status)
                self.send_header("Content-Type", content_type)
                if location:
                    self.send_header("Location", location)
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            def do_GET(self):
                parts = urllib.parse.urlsplit(self.path)
                query = dict(urllib.parse.parse_qsl(parts.query))
                if parts.path == "/authorize":
                    code = secrets.token_urlsafe(12)
                    provider.codes[code] = (query["redirect_uri"], query["code_challenge"])
                    location = query["redirect_uri"] + "?" + urllib.parse.urlencode(
                        {"code": code, "state": query["state"]})
                    self.send(302, b"", "text/plain", location)
                elif parts.path == "/device" and query.get("user_code") in {
                        entry["user_code"] for entry in provider.devices.values()}:
                    for entry in provider.devices.values():
                        if entry["user_code"] == query["user_code"]:
                            entry["approved"] = True
                    self.send(200, b"Device approved", "text/plain")
                else:
                    self.send(404, {"error": "not_found"})

            def do_POST(self):
                length = int(self.headers.get("Content-Length") or 0)
                form = dict(urllib.parse.parse_qsl(self.rfile.read(length).decode()))
                if self.path == "/device/code":
                    device, user = secrets.token_urlsafe(12), secrets.token_hex(2).upper() + "-" + secrets.token_hex(2).upper()
                    provider.devices[device] = {"user_code": user, "approved": False}
                    origin = f"http://127.0.0.1:{provider.port}"
                    self.send(200, {"device_code": device, "user_code": user, "verification_uri": origin + "/device",
                                    "verification_uri_complete": origin + "/device?user_code=" + user,
                                    "interval": 1, "expires_in": 600})
                elif self.path == "/token" and form.get("grant_type") == "authorization_code":
                    redirect, challenge = provider.codes.pop(form.get("code"), (None, None))
                    digest = base64.urlsafe_b64encode(hashlib.sha256(
                        form.get("code_verifier", "").encode()).digest()).rstrip(b"=").decode()
                    if redirect != form.get("redirect_uri") or digest != challenge:
                        self.send(400, {"error": "invalid_grant"})
                        return
                    provider.tokens += 1
                    self.send(200, {"access_token": "live-token", "token_type": "Bearer"})
                elif self.path == "/token":
                    entry = provider.devices.get(form.get("device_code"))
                    if entry is None:
                        self.send(400, {"error": "invalid_grant"})
                    elif not entry["approved"]:
                        self.send(400, {"error": "authorization_pending"})
                    else:
                        provider.tokens += 1
                        self.send(200, {"access_token": "live-token", "token_type": "Bearer"})
                else:
                    self.send(404, {"error": "not_found"})

            def log_message(self, *_args):
                pass

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.port = self.server.server_address[1]
        self.origin = f"127.0.0.1:{self.port}"
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def close(self):
        self.server.shutdown()
        self.server.server_close()


def browser(url):
    """The person's browser: one page, and the loopback redirect of an authorization."""
    class Stop(urllib.request.HTTPRedirectHandler):
        def redirect_request(self, *_args, **_kwargs):
            return None
    try:
        with urllib.request.build_opener(Stop).open(url, timeout=10) as response:
            return response.status
    except urllib.error.HTTPError as error:
        with error:
            location = error.headers.get("Location")
            if error.code == 302 and location:
                with urllib.request.urlopen(location, timeout=10) as callback:
                    return callback.status
            return error.code


PROMPT = """This is a sign-in test. Do not inspect files.
Start this command once with orchestration_monitor, with output_patterns ["sign in"] and timeout_ms 300000:
python3 {cli} {flow} {issuer}
When its output shows a sign-in link, the person must open that link to sign in. Give the link to the person.
Then finish your turn and wait for the command's exit event; do not poll.
After the exit event, reply SIGNED_IN if the output has LOGIN_OK, otherwise reply SIGN_IN_FAILED."""


def scenario(runtime, provider, account, flow, model):
    # A native chat in a plain folder: never the Linux VM or an image workspace.
    settings = {"cwd": str(runtime.root), "yolo_mode": True, "workspaceMode": "worktree"}
    if account != "default":
        settings["account_key"] = account
    if model:
        settings["model"] = model
    lead = runtime.new_lead(settings)
    command = f"python3 {CLI} {flow} http://{provider.origin}"
    runtime.send(lead["id"], PROMPT.format(cli=CLI, flow=flow, issuer=f"http://{provider.origin}"))
    opened, end = None, time.monotonic() + SCENARIO_SECONDS
    try:
        while time.monotonic() < end:
            state = read_runtime_state(runtime)
            for request in state["requests"]:
                if request.get("agent") != lead["id"] or request.get("status") != "pending":
                    continue
                if request["method"] == "monitor/approve" and str(CLI) in request["params"].get("command", ""):
                    runtime.answer(request["id"], {"decision": "accept"})
                elif request["method"] == "browser/open" and opened is None:
                    opened = request
                    answer = runtime.answer(request["id"], {"decision": "accept"}, local_window=True)
                    if browser(answer["openUrl"]) != 200:
                        raise AssertionError("The browser could not finish the sign-in page")
                elif request["method"] != "browser/open":
                    raise AssertionError("Unexpected request: " + request["method"])
            current = runtime.agent(lead["id"])
            if current["status"] in ("failed", "paused", "interrupted"):
                raise AssertionError(json.dumps(current.get("error")))
            if current["status"] == "completed" and opened is not None and \
                    "SIGN" in current.get("lastAnswer", ""):
                break
            time.sleep(0.5)
        else:
            raise AssertionError(f"{account}/{flow}: timed out; card={'yes' if opened else 'no'}")
        monitors = [m for m in state["monitors"] if m.get("agent") == lead["id"]]
        texts = [item.get("text", "") for item in read_runtime_records(runtime, "items")
                 if item.get("id", "").startswith(lead["id"] + ":") and item.get("role") == "assistant"]
        cards = [c for c in read_runtime_records(runtime, "requests")
                 if c.get("agent") == lead["id"] and c.get("method") == "browser/open"]
        card_urls = {c["params"]["url"] for c in cards}
        pasted = [link for text in texts for link in sign_in_links(text)]
        tool_calls = [r for r in read_runtime_records(runtime, "tool_requests")
                      if r.get("agent") == lead["id"] and r.get("tool") == codex_browser_bridge.SIGN_IN_TOOL]
        assert opened["params"]["url"].startswith(f"http://{provider.origin}/"), opened["params"]["url"]
        assert all(link in card_urls for link in pasted), (pasted, card_urls)
        assert any("LOGIN_OK" in (m.get("tail") or "") and m.get("exitCode") == 0 for m in monitors), monitors
        assert "SIGNED_IN" in runtime.agent(lead["id"]).get("lastAnswer", "")
        return {"account": account, "flow": flow, "result": "PASS", "card": card_id(opened["params"]["bridgeId"]),
                "path": "tool" if tool_calls else "message", "toolCalls": len(tool_calls),
                "linksInMessages": len(pasted), "cards": len(cards), "command": command}
    finally:
        runtime.stop(lead["id"])


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--run", action="store_true", required=True)
    parser.add_argument("--providers", default="codex,claude", help="codex, claude or both")
    parser.add_argument("--flows", default="device,loopback", help="device, loopback or both")
    parser.add_argument("--codex-model", default="")
    parser.add_argument("--claude-model", default="")
    options = parser.parse_args()
    # A short folder: a macOS Unix socket path has at most 103 bytes.
    root = Path(tempfile.mkdtemp(prefix="sil-live-", dir="/tmp"))
    # Every state path of the Runtime and its helpers is this throwaway folder, never the
    # state of the running Studio (codex_state.state_dir).
    os.environ["CODEX_AGENTS_STATE_DIR"] = str(root)
    print("Evidence:", root, flush=True)
    provider = Provider()
    runtime = Runtime(root)
    guest = HostGuest(runtime, http_providers=frozenset({provider.origin}))
    bridge = Bridge(runtime, lambda: guest, source="host", http_providers=frozenset({provider.origin}))
    bridge.host_guest = guest
    runtime._browser_host_bridge = bridge
    results, failures = [], []
    try:
        accounts = {"codex": ("default", options.codex_model), "claude": ("claude-local", options.claude_model)}
        for name in options.providers.split(","):
            account, model = accounts[name.strip()]
            for flow in options.flows.split(","):
                try:
                    results.append(scenario(runtime, provider, account, flow.strip(), model))
                except AssertionError as error:
                    failures.append({"account": account, "flow": flow, "result": "FAIL", "error": str(error)[:2000]})
                print(json.dumps((results + failures)[-1]), flush=True)
        evidence = {"result": "FAIL" if failures else "PASS", "scenarios": results + failures,
                    "tokens": provider.tokens}
        (root / "result.json").write_text(json.dumps(evidence, indent=2))
        print(json.dumps(evidence, indent=2), flush=True)
        if failures:
            raise SystemExit(1)
    finally:
        runtime.close()
        provider.close()
        shutil.rmtree(os.environ["CODEX_WORKSPACE_STORE"], ignore_errors=True)
        if not failures:
            shutil.rmtree(root, ignore_errors=True)


if __name__ == "__main__":
    main()
