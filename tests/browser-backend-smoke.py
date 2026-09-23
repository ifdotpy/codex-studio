#!/usr/bin/env python3
"""Start an isolated Canvas backend and exercise real browser setup paths."""
import json
import os
from pathlib import Path
import select
import subprocess
import sys
import tempfile
import threading
import urllib.request
from unittest.mock import patch

REPO = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(REPO / "scripts"))


def fake_server():
    for line in sys.stdin:
        request = json.loads(line)
        if "id" not in request:
            continue
        method = request.get("method")
        with Path(os.environ["CODEX_FAKE_LOG"]).open("a", encoding="utf-8") as log:
            log.write(method + "\n")
        if method == "model/list":
            result = {"data": [{"model": "gpt-6-astra", "defaultReasoningEffort": "medium",
                                "supportedReasoningEfforts": [{"reasoningEffort": "low"},
                                                               {"reasoningEffort": "medium"}]}]}
        elif method in {"thread/start", "thread/resume"}:
            result = {"thread": {"id": request.get("params", {}).get("threadId", "fake-browser-thread")}}
        else:
            result = {}
        print(json.dumps({"id": request["id"], "result": result}), flush=True)


def api(origin, path):
    with urllib.request.urlopen(origin + path, timeout=4) as response:
        return json.loads(response.read())


def run():
    from codex_runtime import Runtime

    with tempfile.TemporaryDirectory(prefix="codex-browser-backend-") as tmp:
        root = Path(tmp)
        home = root / "codex-home"
        home.mkdir()
        project = root / "project"
        project.mkdir()
        node = root / "node_repl"
        node.write_text("#!/bin/sh\nexit 0\n")
        node.chmod(0o755)
        service = home / "browser-service.mjs"
        service.write_text("export {};\n")
        market = root / "marketplace"
        plugin = market / "plugins/chrome"
        (plugin / "skills/control-chrome").mkdir(parents=True)
        (plugin / "skills/control-chrome/SKILL.md").write_text("---\nname: control-chrome\n---\n")
        (plugin / "scripts").mkdir()
        (plugin / "scripts/browser-client.mjs").write_text("export {};\n")
        (plugin / ".codex-plugin").mkdir()
        (plugin / ".codex-plugin/plugin.json").write_text(json.dumps({"name": "chrome", "version": "1.0.0"}))
        installed = home / "plugins/cache/openai-bundled/chrome/latest"
        (installed / ".codex-plugin").mkdir(parents=True)
        (installed / ".codex-plugin/plugin.json").write_text(json.dumps({"name": "chrome", "version": "1.0.0"}))
        config = (
            '[marketplaces.openai-bundled]\nsource_type="local"\nsource=' + json.dumps(str(market)) + '\n'
            '[mcp_servers.node_repl]\ncommand=' + json.dumps(str(node)) + '\nargs=[]\n'
            '[mcp_servers.node_repl.env]\nNODE_REPL_TRUSTED_SERVICES=' +
            json.dumps(json.dumps({"browser": str(service)})) + '\n'
        )
        (home / "config.toml").write_text(config)
        wrapper = root / "fake-codex"
        wrapper.write_text(f"#!/bin/sh\nexec {json.dumps(sys.executable)} {json.dumps(str(Path(__file__).resolve()))} --fake-app-server\n")
        wrapper.chmod(0o755)
        env = {
            **os.environ,
            "CODEX_AGENTS_STATE_DIR": str(root / "canvas-state"),
            "CODEX_HOME": str(home),
            "CODEX_BIN": str(wrapper),
            "CODEX_CANVAS_CWD": str(project),
            "CODEX_FAKE_LOG": str(root / "fake-app-server.log"),
            "PYTHONDONTWRITEBYTECODE": "1",
        }
        backend = subprocess.Popen(
            [sys.executable, str(REPO / "scripts/codex-canvas"), "--port", "0"],
            cwd=REPO, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        )
        try:
            ready, _, _ = select.select([backend.stdout], [], [], 10)
            if not ready:
                raise AssertionError("Temporary codex-canvas did not start within 10 seconds")
            line = backend.stdout.readline().strip()
            if not line.startswith("Codex Canvas: http://"):
                error = backend.stderr.read(2000)
                raise AssertionError(f"Temporary codex-canvas failed: {line} {error}")
            origin = line.removeprefix("Codex Canvas: ")
            assert api(origin, "/api/desktop")["browser"]["enabled"] is True
            backend.terminate()
            backend.wait(timeout=4)

            # Reopen Canvas's exact temporary state through its Runtime after the
            # server exits, so the two processes never own one state directory.
            from codex_runtime import AppServer
            factory = lambda app_root, notification, request, died: AppServer(
                app_root, notification, request, died, executable=str(wrapper)
            )
            with patch.dict(os.environ, {key: env[key] for key in (
                    "CODEX_AGENTS_STATE_DIR", "CODEX_HOME", "CODEX_BIN", "CODEX_CANVAS_CWD", "CODEX_FAKE_LOG")}):
                runtime = Runtime(root / "canvas-state", server_factory=factory)
                try:
                    fresh = runtime.catalog("default")
                    assert fresh["data"][0]["model"] == "gpt-6-astra"
                    agent = runtime.new_lead({"cwd": str(project), "model": "gpt-6-astra"})
                    prepared = runtime.prepare(agent)
                    assert prepared["threadId"] == "fake-browser-thread", prepared
                    with runtime.lock:
                        runtime.loaded.discard(agent["id"])
                    resumed = runtime.prepare(runtime.agent(agent["id"]))
                    assert resumed["threadId"] == "fake-browser-thread", resumed

                    # Native catalog refresh builds params while holding start_lock.
                    # The old configure_browser reentered connect() and blocked here.
                    completed = threading.Event()
                    errors = []

                    def params_during_refresh():
                        try:
                            runtime.new_thread_params(runtime.agent(agent["id"]))
                        except BaseException as error:
                            errors.append(error)
                        finally:
                            completed.set()

                    runtime.start_lock.acquire()
                    worker = threading.Thread(target=params_during_refresh, daemon=True)
                    worker.start()
                    passed_while_locked = completed.wait(2)
                    runtime.start_lock.release()
                    worker.join(3)
                    assert passed_while_locked, "Browser configuration deadlocked while native catalog refresh held start_lock"
                    assert errors == [], errors
                    methods = Path(env["CODEX_FAKE_LOG"]).read_text().splitlines()
                    for required in ("model/list", "thread/start", "thread/resume", "skills/extraRoots/set"):
                        assert required in methods, {"missing": required, "methods": methods}
                finally:
                    runtime.close()
            print(json.dumps({"pass": True, "backend": "temporary codex-canvas",
                              "catalog": "model/list", "threadPreparation": ["thread/start", "thread/resume"],
                              "browserSkill": "skills/extraRoots/set", "heldStartLock": "passed"}))
        finally:
            if backend.poll() is None:
                backend.terminate()
                try:
                    backend.wait(timeout=4)
                except subprocess.TimeoutExpired:
                    backend.kill()
                    backend.wait(timeout=2)


if __name__ == "__main__":
    if sys.argv[1:] == ["--fake-app-server"]:
        fake_server()
    else:
        # Keep an accidental lock regression a bounded test failure.
        import signal
        signal.alarm(45)
        run()
