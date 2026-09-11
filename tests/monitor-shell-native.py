#!/usr/bin/env python3
"""Compare native exec_command with monitor argv using a loopback provider.

The provider emits fixed tool calls. No cloud model requests or user state.
"""

import importlib.util
import json
import os
from pathlib import Path
import shlex
import sys
import tempfile
import threading
from unittest.mock import patch

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
from codex_shell import monitor_command

spec = importlib.util.spec_from_file_location("native_fixture", Path(__file__).with_name("workspace-native-turn.py"))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)


class ShellHandler(f.ResponsesHandler):
    def do_POST(self):
        if self.path != "/v1/responses":
            return self.reject()
        request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        self.server.requests.append(request)
        number = len(self.server.requests)
        item = ({"id": "fc_shell", "call_id": "call_shell", "status": "completed",
                 "type": "function_call", "name": "exec_command",
                 "arguments": json.dumps({"cmd": self.server.command, "max_output_tokens": 1000})}
                if number == 1 else
                {"id": "msg_shell", "type": "message", "role": "assistant", "status": "completed",
                 "content": [{"type": "output_text", "text": "Complete.", "annotations": []}]})
        response = {"id": f"resp_shell_{number}", "object": "response", "status": "completed",
                    "model": request["model"], "output": [item],
                    "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}}
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Connection", "close")
        self.end_headers()
        self.event("response.created", response={**response, "status": "in_progress", "output": []})
        self.event("response.output_item.added", output_index=0, item={**item, "status": "in_progress"})
        self.event("response.output_item.done", output_index=0, item=item)
        self.event("response.completed", response=response)
        self.close_connection = True


def run_case(root, *, login=True, snapshot=True):
    home, project, shell_home = root / "codex", root / "project", root / "shell"
    for path in (home, project, shell_home):
        path.mkdir(parents=True)
    # Deliberately start with Xcode Python first. Codex's rc setup must restore
    # the same Homebrew preference for both execution paths.
    python_bin = str(Path(sys.executable).parent)
    (shell_home / ".zshrc").write_text(
        f"export PATH={shlex.quote(python_bin)}:$PATH\n"
        "export STUDIO_RC=loaded\nexport STUDIO_POLICY=wrong\n"
    )
    result_file = project / "result.json"
    script = (
        "import importlib.util,json,os,sys;from pathlib import Path;"
        "result={'python':sys.executable,'tomllib':importlib.util.find_spec('tomllib') is not None,"
        "'rc':os.getenv('STUDIO_RC'),'policy':os.getenv('STUDIO_POLICY'),"
        "'excluded':os.getenv('STUDIO_EXCLUDED')};"
        f"Path({str(result_file)!r}).write_text(json.dumps(result));print(json.dumps(result))"
    )
    command = "python3 -c " + shlex.quote(script)
    provider = f.Provider()
    provider.RequestHandlerClass = ShellHandler
    provider.command = command
    threading.Thread(target=provider.serve_forever, daemon=True).start()
    endpoint = f"http://127.0.0.1:{provider.server_port}"
    (home / "config.toml").write_text(f'''model = "gpt-5.6-sol"
model_provider = "local-shell"
allow_login_shell = {str(login).lower()}
[features]
plugins = false
remote_plugin = false
apps = false
skip_host_skill_discovery = true
shell_snapshot = {str(snapshot).lower()}
[shell_environment_policy]
exclude = ["STUDIO_EXCLUDED"]
[shell_environment_policy.set]
STUDIO_POLICY = "configured"
[model_providers.local-shell]
name = "Local shell fixture"
base_url = "{endpoint}/v1"
wire_api = "responses"
requires_openai_auth = false
request_max_retries = 0
stream_max_retries = 0
''')
    environment = {"CODEX_HOME": str(home), "ZDOTDIR": str(shell_home),
                   "PATH": "/usr/bin:/bin:" + python_bin, "CODEX_BIN": os.environ.get("CODEX_BIN", "/opt/homebrew/bin/codex"),
                   "OPENAI_API_KEY": "", "CODEX_API_KEY": "", "STUDIO_EXCLUDED": "must-not-reach-command",
                   "NO_PROXY": "127.0.0.1,localhost", "no_proxy": "127.0.0.1,localhost"}
    environment.update({key: endpoint for key in ("HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "http_proxy", "https_proxy", "all_proxy")})
    notifications = []
    server = None
    try:
        with patch.dict(os.environ, environment):
            os.environ.pop("SDKROOT", None)
            server = f.AppServer(root, notifications.append, lambda _: None, lambda: None)
            started = server.call("thread/start", {"cwd": str(project), "model": "gpt-5.6-sol",
                "config": {**f.THREAD_CONFIG, "model_provider": "local-shell"},
                "approvalPolicy": "never", "sandbox": "danger-full-access"})
            turn = server.call("turn/start", {"threadId": started["thread"]["id"],
                "input": [{"type": "text", "text": "Execute the fixed environment probe."}]})["turn"]["id"]
            completed = f.until(lambda: next((n for n in notifications if n.get("method") == "turn/completed"
                and n["params"]["turn"]["id"] == turn), None), "native shell turn completion")
            assert completed["params"]["turn"]["status"] == "completed", completed
            native = json.loads(result_file.read_text())
            result_file.unlink()
            managed_command = command
            if sys.platform == "darwin":
                (project / "main.c").write_text("int main(void) { return 0; }\n")
                managed_command += ' && /usr/bin/cc main.c -o main && ./main && printf "%s" "$SDKROOT" > sdk.txt'
            result = server.call("command/exec", {"command": monitor_command(server, managed_command, str(project)),
                "cwd": str(project), "timeoutMs": 10000, "sandboxPolicy": {"type": "dangerFullAccess"}})
            assert result["exitCode"] == 0, result
            if sys.platform == "darwin":
                sdk = (project / "sdk.txt").read_text()
                selected = server.call("command/exec", {"command": ["/usr/bin/xcrun", "--sdk", "macosx", "--show-sdk-path"],
                    "cwd": str(project), "timeoutMs": 10000, "sandboxPolicy": {"type": "dangerFullAccess"}})
                assert selected["exitCode"] == 0 and sdk == selected["stdout"].strip(), {"sdk": sdk, "selected": selected}
            managed = json.loads(result_file.read_text())
            # Native snapshots restore ambient exports even if the native
            # command policy excludes them. Keep command/exec's filter intact.
            assert managed.pop("excluded") is None, managed
            native.pop("excluded")
            assert managed == native, {"native": native, "monitor": managed}
            assert native["policy"] == "configured", native
            if login and snapshot:
                assert native["tomllib"] and native["rc"] == "loaded", native
            else:
                assert native["rc"] is None, native
            print(json.dumps({"login": login, "snapshot": snapshot, "native_and_monitor": native,
                              "monitor_sdk": sdk if sys.platform == "darwin" else None}))
    finally:
        if server:
            server.close()
        provider.shutdown()
        provider.server_close()


if __name__ == "__main__":
    if not Path("/bin/zsh").is_file():
        raise SystemExit("This native fixture requires zsh")
    with tempfile.TemporaryDirectory(prefix="codex-monitor-shell-") as directory:
        root = Path(directory)
        run_case(root / "default")
        run_case(root / "no-login", login=False)
        run_case(root / "no-snapshot", snapshot=False)
