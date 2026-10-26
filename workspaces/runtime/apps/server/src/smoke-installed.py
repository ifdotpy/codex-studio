#!/usr/bin/env python3
"""Post-install smoke check of an installed Codex Studio app bundle.

Part 1 reads the live server and never changes it: it sends no session token and
no mutation. It checks the installed build, the backend process, the account
limit readings, the provider handles, and the supervisor acknowledgements.

Part 2 starts the installed supervisor and server from the bundle against a new
private state folder and a free port, with the production launcher code of the
bundle (recover_backend.py). It runs these flows with real, small model requests:
a Codex sign-in start and cancel (device code, no browser), one agent turn,
orchestration_send to a room and a broadcast, and a thread resume after a
provider restart. It never opens a window. It stops only the processes that it
started and deletes only the folder that it created.

Run it from a checkout with `just smoke-installed`, or from the bundle:
  python3 "/Applications/Codex Studio.app/Contents/Resources/workspace/scripts/smoke-installed.py"
See docs/testing.md, section "Installed build smoke".
"""
from __future__ import annotations

import argparse
import datetime
import importlib.util
import json
import os
from pathlib import Path
import re
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
from types import ModuleType
from typing import Any, Callable
import urllib.parse
import uuid

DEFAULT_APP = "/Applications/Codex Studio.app"
DEFAULT_URL = "http://127.0.0.1:4620"
# Prefix of the private folder that part 2 creates and deletes. A macOS Unix socket
# path has at most 103 bytes, so the folder is short and in /tmp.
ROOT_PREFIX = "studio-smoke-"
ROOT_MARKER = "studio-smoke-owner.json"
# Names the production launcher sets for the backend it starts (recover_backend.py
# launch_environment); the smoke backend must not inherit them from this shell.
ROUTING_ENVIRONMENT = ("CODEX_AGENTS_SUPERVISOR_MODE", "CODEX_AGENTS_STATE_DIR",
                       "CODEX_AGENTS_SUPERVISOR_FALLBACK", "CODEX_WORKSPACE_STORE")
# Seconds between the two supervisor samples of the acknowledgement check.
ACK_SAMPLE_SECONDS = 3.0
# Seconds that each flow may take. Flows run one after another.
STACK_START_SECONDS = 120.0
MODELS_SECONDS = 90.0
SIGN_IN_SECONDS = 90.0
TURN_SECONDS = 120.0
TEAM_SECONDS = 240.0
RESUME_SECONDS = 150.0
# Seconds that a smoke process gets between SIGTERM and SIGKILL; the server's own
# shutdown watchdog (SERVER_SHUTDOWN_GRACE in runtime-server) is 20 seconds.
STOP_SECONDS = 25.0
POLL_SECONDS = 0.5

TURN_TOKEN = "SMOKE_TURN_OK"
ROOM_TOKEN = "SMOKE_ROOM_OK"
BROADCAST_TOKEN = "SMOKE_BROADCAST_OK"
CHILD_NAME = "Smoke Child"
# The smoke tests host flows: the lead asks for a host worktree, so it never needs the
# Linux VM, which a fresh smoke state does not have. These are the record fields that
# the backend stores for that request.
HOST_WORKSPACE = {"workspaceMode": "worktree"}
HOST_RECORD = {"workspaceMode": "worktree", "executionMode": "native"}
FAILED_STATUSES = ("failed", "paused", "interrupted")


class CheckFailed(Exception):
    """A check found a defect; the message is the reason that the report prints."""


class Report:
    def __init__(self) -> None:
        self.results: list[dict[str, Any]] = []

    def run(self, name: str, check: Callable[[], str]) -> bool:
        started = time.monotonic()
        try:
            detail, ok = check(), True
        except CheckFailed as error:
            detail, ok = str(error), False
        except Exception as error:  # A crash in one check must not hide the others.
            detail, ok = f"{type(error).__name__}: {error}", False
        seconds = time.monotonic() - started
        self.results.append({"check": name, "result": "PASS" if ok else "FAIL",
                             "seconds": round(seconds, 1), "detail": detail})
        print(f"{'PASS' if ok else 'FAIL'}  {name:<14} {seconds:6.1f} s  {detail}", flush=True)
        return ok

    def skip(self, name: str, reason: str) -> None:
        self.results.append({"check": name, "result": "FAIL", "seconds": 0.0, "detail": reason})
        print(f"FAIL  {name:<14} {0.0:6.1f} s  {reason}", flush=True)

    @property
    def failed(self) -> list[dict[str, Any]]:
        return [result for result in self.results if result["result"] != "PASS"]


class Bundle:
    """The installed app bundle and the production modules that it ships."""

    def __init__(self, app: Path) -> None:
        self.app = app.resolve()
        self.resources = self.app / "Contents" / "Resources"
        self.workspace = self.resources / "workspace"
        self.scripts = self.workspace / "scripts"
        for path in (self.resources / "recover_backend.py", self.scripts / "codex_api_client.py"):
            if not path.is_file():
                raise SystemExit(f"smoke-installed: {path} is missing; is {self.app} a Codex Studio app?")
        # Use the bundle's own client and launcher, not the copies of this checkout.
        sys.path.insert(0, str(self.scripts))
        spec = importlib.util.spec_from_file_location("installed_recover_backend",
                                                      self.resources / "recover_backend.py")
        if spec is None or spec.loader is None:
            raise SystemExit("smoke-installed: cannot load the installed recover_backend.py")
        self.recovery: ModuleType = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(self.recovery)
        import codex_api_client
        self.client: ModuleType = codex_api_client

    def build(self) -> dict[str, Any]:
        stamp = self.resources / "studio-build.json"
        value = json.loads(stamp.read_text(encoding="utf-8")) if stamp.is_file() else {}
        return value if isinstance(value, dict) else {}

    def contains(self, path: str) -> bool:
        return Path(path).resolve().is_relative_to(self.app)


def get_json(client: ModuleType, url: str, path: str, *, token: str = "", timeout: float = 30) -> Any:
    return client.studio_json(url, path, token, timeout=timeout)


def ps_rows() -> list[tuple[int, int, str]]:
    output = subprocess.run(["/bin/ps", "-axww", "-o", "pid=,ppid=,command="], check=True,
                            capture_output=True, text=True, timeout=10).stdout
    rows = []
    for line in output.splitlines():
        parts = line.split(None, 2)
        if len(parts) == 3 and parts[0].isdigit() and parts[1].isdigit():
            rows.append((int(parts[0]), int(parts[1]), parts[2]))
    return rows


def process_alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True


def process_age(pid: int) -> str:
    result = subprocess.run(["/bin/ps", "-p", str(pid), "-o", "etime="], capture_output=True,
                            text=True, timeout=5)
    return result.stdout.strip() or "?"


# Part 1: read-only checks of the live server.

class LiveChecks:
    def __init__(self, bundle: Bundle, url: str) -> None:
        self.bundle = bundle
        self.url = url.rstrip("/")
        self.desktop: dict[str, Any] = {}
        self.config: dict[str, Any] = {}
        self.state: Path | None = None

    def get(self, path: str) -> Any:
        # No session token: the live server refuses every mutation without one.
        return get_json(self.bundle.client, self.url, path)

    def build(self) -> str:
        stamp = self.bundle.build()
        commit = stamp.get("commit")
        if not isinstance(commit, str) or not re.fullmatch(r"[0-9a-f]{40}", commit):
            raise CheckFailed(f"{self.bundle.resources / 'studio-build.json'} has no build commit")
        built = stamp.get("builtAt")
        when = (datetime.datetime.fromtimestamp(built).strftime("%Y-%m-%d %H:%M")
                if isinstance(built, (int, float)) else "unknown time")
        return f"commit {commit[:12]} built {when} ({self.bundle.app})"

    def backend(self) -> str:
        rb = self.bundle.recovery
        state = Path(self.bundle.client.desktop_state_dir(self.url, timeout=10)).resolve()
        port = urllib.parse.urlsplit(self.url).port or 80
        data = rb.identity(port, state, timeout=10)
        if not data:
            raise CheckFailed(f"no backend answers at {self.url}")
        config = rb.load_config(state / "background-recovery.json", state)
        if config.get("port") != port:
            raise CheckFailed(f"the recovery configuration names port {config.get('port')}, not {port}")
        for name in ("resources", "runtimeServer", "processSupervisor"):
            if isinstance(config.get(name), str) and not self.bundle.contains(config[name]):
                raise CheckFailed(f"the recovery configuration {name} {config[name]} is outside {self.bundle.app}")
        # The installed launcher's own proof: the PID runs the configured backend of this
        # bundle (for Rust, backendBuild is the SHA-256 of that file) and owns the listener.
        rb.verify_backend_process(data, config)
        selected = rb.backend_selection(config)
        kind = data.get("backendKind") or "python"
        if kind != selected:
            raise CheckFailed(f"the {kind} backend runs, but {selected} is selected")
        if config.get("supervisorEnabled") is True and data.get("supervisorMode") is not True:
            raise CheckFailed("the supervisor is enabled, but the backend is not in supervisor mode")
        if data.get("supervisorFallback") or data.get("supervisorError"):
            raise CheckFailed(f"supervisor fallback or error: {data.get('supervisorError') or 'fallback mode'}")
        self.desktop, self.config, self.state = data, config, state
        build = str(data.get("backendBuild") or "")[:12]
        return f"{kind} pid {data['pid']} up {process_age(data['pid'])}, build {build}, state {state}"

    def accounts(self) -> str:
        accounts = self.get("/api/accounts").get("accounts")
        if not isinstance(accounts, list) or not accounts:
            raise CheckFailed("/api/accounts returned no accounts")
        problems, notes = [], []
        for account in accounts:
            key, status = account.get("id"), account.get("status")
            label = f"{key} ({account.get('provider', 'codex')}, {status})"
            query = urllib.parse.urlencode({"account_key": key, "cached": "1"})
            try:
                limits = self.get("/api/limits?" + query)
            except ValueError as failure:
                problems.append(f"{label}: limits request failed: {failure}")
                continue
            data, error = limits.get("data"), limits.get("error")
            if isinstance(data, dict) and data.get("rateLimits") is not None:
                at = limits.get("readAt") or limits.get("at")
                age = f"{(time.time() - at) / 60:.0f} min" if isinstance(at, (int, float)) else "unknown age"
                notes.append(f"{key}: reading {age} old")
            elif isinstance(error, str) and error:
                notes.append(f"{key}: unavailable ({error[:80]})")
            elif status != "ready":
                notes.append(f"{key}: {status}, no reading")
            else:
                problems.append(f"{label}: no limit reading and no unavailable reason")
        if problems:
            raise CheckFailed("; ".join(problems))
        return f"{len(accounts)} accounts: " + "; ".join(notes)

    def providers(self) -> str:
        supervisor = self.desktop.get("supervisor") or {}
        if self.config.get("supervisorEnabled") is not True:
            return "supervisor mode is off; no supervised provider handles"
        recovery = supervisor.get("recovery") or {}
        if recovery.get("blocked") or recovery.get("degraded"):
            raise CheckFailed(f"supervisor recovery: {json.dumps(recovery)}")
        handles = supervisor.get("handles")
        if not isinstance(handles, list):
            raise CheckFailed("/api/desktop has no supervisor handle list")
        problems = []
        for handle in handles:
            name, pid = handle.get("id"), handle.get("pid")
            if not isinstance(pid, int) or not process_alive(pid):
                problems.append(f"{name}: provider PID {pid} is not running")
            if handle.get("stdoutReaderAlive") is not True or handle.get("stdoutReaderError"):
                problems.append(f"{name}: output reader stopped ({handle.get('stdoutReaderError')})")
            if handle.get("backpressure") or handle.get("persistenceErrors"):
                problems.append(f"{name}: backpressure or journal errors")
        if problems:
            raise CheckFailed("; ".join(problems))
        return f"{len(handles)} handles open: " + ", ".join(
            f"{h.get('id', '').removeprefix('account:')} pid {h.get('pid')}" for h in handles)

    def acknowledgements(self) -> str:
        if self.config.get("supervisorEnabled") is not True:
            return "supervisor mode is off"
        first = {h["id"]: h for h in (self.desktop.get("supervisor") or {}).get("handles", [])}
        time.sleep(ACK_SAMPLE_SECONDS)
        later = {h["id"]: h for h in (self.get("/api/desktop").get("supervisor") or {}).get("handles", [])}
        stalled = []
        for name, handle in later.items():
            pending = handle["sequence"] - handle["acknowledged"]
            before = first.get(name)
            # Pending output is normal while the backend works; it is stalled when the
            # acknowledged cursor did not reach the earlier sample's sequence.
            if pending > 0 and before is not None and before["sequence"] > before["acknowledged"] \
                    and handle["acknowledged"] < before["sequence"]:
                stalled.append(f"{name}: {pending} events unacknowledged for {ACK_SAMPLE_SECONDS:.0f} s")
        if stalled:
            raise CheckFailed("; ".join(stalled))
        total = sum(h["sequence"] - h["acknowledged"] for h in later.values())
        return f"no stalled acknowledgements ({total} pending now, {len(later)} handles)"

    def renderer(self) -> str:
        opener = self.bundle.client._DIRECT_OPENER
        with opener.open(self.url + "/", timeout=10) as response:
            page = response.read().decode("utf-8", "replace")
        installed = (self.bundle.workspace / "web" / "dist" / "index.html").read_text(encoding="utf-8")
        build = re.compile(r'<meta name="studio-build" content="([^"]+)"')
        served, packaged = build.search(page), build.search(installed)
        if not served or not packaged or served.group(1) != packaged.group(1):
            raise CheckFailed(f"the served renderer build {served and served.group(1)} is not the installed "
                              f"build {packaged and packaged.group(1)}")
        script = re.search(r'<script type="module"[^>]* src="([^"]+\.js)"', page)
        if not script:
            raise CheckFailed("the server page has no renderer module")
        asset = urllib.parse.urljoin(self.url + "/", script.group(1))
        with opener.open(asset, timeout=10) as response:
            if response.status != 200:
                raise CheckFailed(f"{script.group(1)} returned HTTP {response.status}")
        return f"renderer build {served.group(1)} and {script.group(1)} served"


# Part 2: flows on a private backend that runs the installed binaries.

class SmokeBackend:
    """The installed supervisor and server on a new private state and a free port."""

    def __init__(self, bundle: Bundle, live: dict[str, Any], live_state: Path, live_port: int,
                 codex_home: str | None) -> None:
        self.bundle = bundle
        self.live_state = live_state
        self.root = Path(tempfile.mkdtemp(prefix=ROOT_PREFIX, dir="/tmp")).resolve()
        (self.root / ROOT_MARKER).write_text(json.dumps({"pid": os.getpid(), "created": time.time()}))
        self.state = self.root / "state"
        self.work = self.root / "work"
        self.port = free_port(exclude=live_port)
        if self.state == live_state or self.state.is_relative_to(live_state) \
                or live_state.is_relative_to(self.root):
            raise SystemExit("smoke-installed: the private state overlaps the live state")
        environment = dict(live.get("environment") or {})
        if codex_home:
            environment["CODEX_HOME"] = str(Path(codex_home).expanduser().resolve())
        self.config: dict[str, Any] = {**live, "stateDir": str(self.state), "port": self.port,
                                       "environment": environment}
        self.url = f"http://127.0.0.1:{self.port}"
        self.token = ""
        self.supervisor: subprocess.Popen[bytes] | None = None
        self.server: subprocess.Popen[bytes] | None = None

    def start(self) -> str:
        rb = self.bundle.recovery
        self.state.mkdir(mode=0o700)
        self.work.mkdir()
        for name in ROUTING_ENVIRONMENT:
            os.environ.pop(name, None)
        os.environ["CODEX_WORKSPACE_STORE"] = str(self.root / "workspaces")
        # The Rust server accepts only private files in a fresh state (fresh_state.rs),
        # and the launcher of older bundles opens canvas.log with the process umask.
        os.umask(0o077)
        deadline = time.monotonic() + STACK_START_SECONDS
        if self.config.get("supervisorEnabled") is True:
            sys.path.insert(0, str(self.bundle.scripts))
            import codex_supervisor_service
            command = codex_supervisor_service._macos_jobs(
                Path(self.config["resources"]), self.state, self.config["python"], self.config)["supervisor"]
            with (self.root / "supervisor.log").open("ab") as log:
                self.supervisor = subprocess.Popen(
                    command, cwd=self.root, env=rb.launch_environment(self.config, self.state),
                    stdin=subprocess.DEVNULL, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
            wait_until(deadline, "the supervisor did not start", lambda: self.supervisor_ready(rb))
        # The production launcher: it refuses an occupied state and starts the selected backend.
        self.server, status = rb.tick(self.config, self.state)
        if status != "started" or self.server is None:
            raise CheckFailed(f"the launcher did not start the backend: {status}")
        desktop = wait_until(deadline, "the backend did not answer", lambda: self.identity(rb))
        if Path(desktop["stateDir"]).resolve() != self.state:
            raise CheckFailed("the smoke backend reports another state folder")
        self.token = self.bundle.client.session_token(self.url, timeout=10)
        supervisor = f", supervisor pid {self.supervisor.pid}" if self.supervisor else ""
        return f"{desktop.get('backendKind') or 'python'} pid {desktop['pid']}{supervisor}, port {self.port}, state {self.state}"

    def supervisor_ready(self, rb: ModuleType) -> bool:
        if self.supervisor is not None and self.supervisor.poll() is not None:
            raise CheckFailed(f"the supervisor exited with {self.supervisor.returncode}; see {self.root}/supervisor.log")
        try:
            if rb.backend_selection(self.config) == "rust":
                return bool(rb.supervisor_serves_protocol_v2(self.state))
            return (self.state / "supervisor.sock").exists()
        except (OSError, ValueError):
            return False

    def identity(self, rb: ModuleType) -> dict[str, Any] | None:
        if self.server is not None and self.server.poll() is not None:
            raise CheckFailed(f"the backend exited with {self.server.returncode}; see {self.state}/canvas.log")
        try:
            data = rb.identity(self.port, self.state, timeout=2)
        except OSError:
            return None
        return data if isinstance(data, dict) else None

    def get(self, path: str, timeout: float = 30) -> Any:
        return get_json(self.bundle.client, self.url, path, token=self.token, timeout=timeout)

    def post(self, path: str, body: dict[str, Any], timeout: float = 60) -> Any:
        try:
            return self.bundle.client.request_json(self.url, path, body, self.token, timeout=timeout)
        except Exception as error:
            detail = ""
            read = getattr(error, "read", None)
            if callable(read):
                detail = read().decode("utf-8", "replace")[:300]
            raise CheckFailed(f"POST {path} failed: {error} {detail}".strip()) from error

    def entities(self) -> list[dict[str, Any]]:
        values: list[dict[str, Any]] = self.bundle.client.pull_entities(self.url, self.token, timeout=30)
        return values

    def provider_children(self) -> list[tuple[int, str]]:
        """Processes of this smoke's supervisor; each must run from this smoke's state."""
        if self.supervisor is None:
            return []
        return [(pid, command) for pid, parent, command in ps_rows() if parent == self.supervisor.pid]

    def stop(self) -> list[str]:
        """Stop only the processes that this smoke started. Return what is left."""
        for process in (self.server, None, self.supervisor):
            if process is None:
                # Providers stop after the server and before their supervisor.
                for pid, command in self.provider_children():
                    if command.startswith(str(self.state) + "/") or str(self.root) in command:
                        stop_process(pid, gone(pid))
            else:
                stop_process(process.pid, ended(process))
        left = [f"pid {pid}: {command[:120]}" for pid, _, command in ps_rows()
                if str(self.root) in command and pid != os.getpid()]
        return left

    def remove(self) -> str | None:
        """Delete the private folder only when it is ours and holds no mount."""
        marker = self.root / ROOT_MARKER
        mounts = subprocess.run(["/sbin/mount"], capture_output=True, text=True, timeout=10).stdout
        if (self.root.parent != Path("/tmp").resolve() or not self.root.name.startswith(ROOT_PREFIX)
                or not marker.is_file() or str(self.root) in mounts):
            return f"kept {self.root}: it is not an unmounted smoke folder"
        shutil.rmtree(self.root)
        return None


def free_port(exclude: int) -> int:
    while True:
        with socket.socket() as probe:
            probe.bind(("127.0.0.1", 0))
            port: int = probe.getsockname()[1]
        if port != exclude:
            return port


def gone(pid: int) -> Callable[[], bool]:
    return lambda: not process_alive(pid)


def ended(process: subprocess.Popen[bytes]) -> Callable[[], bool]:
    return lambda: process.poll() is not None


def stop_process(pid: int, stopped: Callable[[], bool]) -> None:
    if stopped():
        return
    try:
        os.kill(pid, signal.SIGTERM)
    except ProcessLookupError:
        return
    deadline = time.monotonic() + STOP_SECONDS
    while time.monotonic() < deadline:
        if stopped():
            return
        time.sleep(0.2)
    try:
        os.kill(pid, signal.SIGKILL)
    except ProcessLookupError:
        return
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline and not stopped():
        time.sleep(0.2)


def wait_until(deadline: float, message: str, check: Callable[[], Any]) -> Any:
    while time.monotonic() < deadline:
        value = check()
        if value:
            return value
        time.sleep(POLL_SECONDS)
    raise CheckFailed(message)


class Flows:
    def __init__(self, backend: SmokeBackend, model: str, effort: str) -> None:
        self.backend = backend
        self.model = model
        self.effort = effort
        self.lead = ""
        self.lead_answered = False
        self.thread = ""

    def agent(self, agent_id: str) -> dict[str, Any]:
        for entity in self.backend.entities():
            if entity["collection"] == "agent" and entity["id"] == agent_id:
                value: dict[str, Any] = entity["value"]
                return value
        raise CheckFailed(f"agent {agent_id} is missing")

    def answers(self, agent_id: str) -> tuple[str, list[str]]:
        transcript = self.backend.get("/api/transcript?" + urllib.parse.urlencode({"id": agent_id}))
        texts = [item.get("text") or "" for item in transcript.get("items", [])
                 if item.get("role") == "assistant"]
        return str((transcript.get("agent") or {}).get("status")), texts

    def unexpected_requests(self) -> None:
        pending = [entity["value"] for entity in self.backend.entities()
                   if entity["collection"] == "request" and entity["value"].get("status") == "pending"]
        if pending:
            raise CheckFailed("unexpected pending request: " + ", ".join(
                str(request.get("method")) for request in pending))

    def wait_answer(self, agent_id: str, seconds: float, done: Callable[[list[str]], bool]) -> list[str]:
        deadline = time.monotonic() + seconds
        status, texts = "unknown", list[str]()
        while time.monotonic() < deadline:
            status, texts = self.answers(agent_id)
            if status in FAILED_STATUSES:
                error = self.agent(agent_id).get("error")
                raise CheckFailed(f"agent {status}: {json.dumps(error)[:300]}")
            if status == "completed" and done(texts):
                return texts
            self.unexpected_requests()
            time.sleep(POLL_SECONDS)
        last = texts[-1][:120] if texts else "no answer"
        raise CheckFailed(f"timed out after {seconds:.0f} s; status {status}, last answer: {last}")

    def models(self) -> str:
        # A fresh backend answers 400 "Model catalog is pending" until its first
        # native read finishes; the Python backend reads it in the background.
        deadline = time.monotonic() + MODELS_SECONDS
        while True:
            try:
                listing = self.backend.get("/api/models?account_key=default", timeout=60)
                break
            except Exception as error:
                if (getattr(error, "status_code", None) != 400
                        or "Model catalog is pending" not in str(error)
                        or time.monotonic() >= deadline):
                    raise
                time.sleep(POLL_SECONDS)
        models = [item.get("id") for item in listing.get("data", []) if isinstance(item, dict)]
        if self.model not in models:
            raise CheckFailed(f"model {self.model} is not offered; offered: {', '.join(map(str, models))}")
        account = next(a for a in self.backend.get("/api/accounts")["accounts"] if a["id"] == "default")
        if account.get("status") != "ready":
            raise CheckFailed(f"the default Codex account of {account.get('home')} is {account.get('status')}")
        return f"default account {account.get('home')} ready, model {self.model} effort {self.effort}"

    def sign_in(self) -> str:
        request_id = str(uuid.uuid4())
        started = self.backend.post("/api/accounts/login", {"login_id": request_id}, timeout=SIGN_IN_SECONDS)
        try:
            if started.get("status") != "pending":
                raise CheckFailed(f"sign-in start returned {started.get('status')}: {started.get('error')}")
            link = urllib.parse.urlsplit(str(started.get("verificationUrl") or ""))
            if link.scheme != "https" or not (link.hostname or "").endswith("openai.com") \
                    or not started.get("userCode"):
                raise CheckFailed("sign-in start returned no OpenAI device link and code")
        finally:
            # Cancel also after a failed start; the device code is never printed or opened.
            cancelled = self.backend.post("/api/accounts/login/cancel", {"login_id": request_id})
        if cancelled.get("status") != "cancelled":
            raise CheckFailed(f"sign-in cancel returned {cancelled.get('status')}")
        accounts = self.backend.get("/api/accounts")
        login = [entry for entry in accounts.get("logins", []) if entry.get("requestId") == request_id]
        if [entry.get("status") for entry in login] != ["cancelled"]:
            raise CheckFailed(f"the account list shows the sign-in as {[e.get('status') for e in login]}")
        if any(a.get("id") == started.get("accountKey") and a.get("status") == "ready"
               for a in accounts.get("accounts", [])):
            raise CheckFailed("a cancelled sign-in left a ready account")
        return f"device link {link.hostname}{link.path}, then cancelled"

    def turn(self) -> str:
        self.lead = str(uuid.uuid4())
        created = self.backend.post("/api/agents", {
            "id": self.lead, "name": "Smoke lead", "cwd": str(self.backend.work),
            "prompt": f"This is an automated smoke check. Do not use tools. Reply with exactly {TURN_TOKEN}.",
            "model": self.model, "effort": self.effort, **HOST_WORKSPACE})
        modes = {key: created.get(key) for key in HOST_RECORD}
        if modes != HOST_RECORD:
            raise CheckFailed(f"the lead was created with {modes}, not the requested host flow {HOST_RECORD}")
        texts = self.wait_answer(self.lead, TURN_SECONDS, lambda texts: any(TURN_TOKEN in t for t in texts))
        self.thread = str(self.agent(self.lead).get("threadId") or "")
        if not self.thread:
            raise CheckFailed("the completed agent has no provider thread")
        self.lead_answered = True
        return f"lead {self.lead[:8]} answered {texts[-1][:40]!r} on thread {self.thread[:13]}"

    def lead_failed(self) -> None:
        """Stop a wait at once when the lead can no longer spawn a child or take its result."""
        value = self.agent(self.lead)
        if value.get("status") in FAILED_STATUSES:
            raise CheckFailed(f"the lead became {value.get('status')}: {json.dumps(value.get('error'))[:300]}")

    def team(self) -> str:
        if not self.lead_answered:
            raise CheckFailed("not run: the agent turn did not pass")
        child_prompt = ("This is a smoke check of the orchestration_send tool. Do not inspect files. Use the "
                        "orchestration_send tool for both calls below; do not use orchestration_message. "
                        f'First call orchestration_send with agent_id "parent" and text "{ROOM_TOKEN}". '
                        f'Then call orchestration_send with agent_id "broadcast" and text "{BROADCAST_TOKEN}". '
                        "Then reply CHILD_DONE.")
        self.backend.post("/api/messages", {"id": str(uuid.uuid4()), "room": self.lead, "text": (
            "This is an automated smoke check. Do not inspect files and do not run shell commands. "
            f'Call orchestration_spawn once with exactly one agent: name "{CHILD_NAME}", role "reviewer", '
            f'model "{self.model}", effort "{self.effort}", and this exact prompt: {json.dumps(child_prompt)} '
            "Then finish this turn. When the child result arrives, reply SMOKE_TEAM_OK.")})
        deadline = time.monotonic() + TEAM_SECONDS
        def spawned() -> dict[str, Any] | None:
            child = next((e["value"] for e in self.backend.entities() if e["collection"] == "agent"
                          and e["value"].get("parentId") == self.lead), None)
            if child is None:
                self.lead_failed()
            return child

        child = wait_until(deadline, f"no {CHILD_NAME} was spawned", spawned)
        child_id = child["id"]
        tokens = {"private": ROOM_TOKEN, "broadcast": BROADCAST_TOKEN}
        missing = ["no room yet"]

        def delivered() -> bool:
            # The rooms as the renderer reads them: the private room of the lead and the
            # child, and the broadcast room of the lead's team.
            rooms = {}
            for entity in self.backend.entities():
                room = entity["value"]
                if entity["collection"] != "room":
                    continue
                if room.get("kind") == "private" and set(room.get("members") or []) == {self.lead, child_id}:
                    rooms["private"] = room["id"]
                elif room.get("kind") == "broadcast" and room.get("rootId") == self.lead:
                    rooms["broadcast"] = room["id"]
            missing[:] = [f"no {kind} room" for kind in tokens if kind not in rooms]
            for kind, room_id in rooms.items():
                messages = self.backend.get("/api/agent-chat?" + urllib.parse.urlencode(
                    {"room": room_id})).get("messages", [])
                sent = [m for m in messages if m.get("sender") == child_id and tokens[kind] in (m.get("text") or "")]
                if not sent:
                    missing.append(f"no {tokens[kind]} from {CHILD_NAME} in {room_id}")
                elif sent[0].get("deliveries", {}).get(self.lead) != "delivered":
                    missing.append(f"{tokens[kind]} delivery to the lead is {sent[0].get('deliveries')}")
            return not missing

        def child_settled() -> bool:
            value = self.agent(child_id)
            if value.get("status") in FAILED_STATUSES:
                raise CheckFailed(f"{CHILD_NAME} {value.get('status')}: {json.dumps(value.get('error'))[:300]}")
            self.lead_failed()
            self.unexpected_requests()
            return value.get("status") == "completed"

        wait_until(deadline, f"{CHILD_NAME} did not finish", child_settled)
        try:
            wait_until(min(deadline, time.monotonic() + 30), "", delivered)
        except CheckFailed:
            raise CheckFailed(f"{CHILD_NAME} finished, but: " + "; ".join(missing)) from None
        # Tool outputs in the transcript carry the call arguments. Among the messaging
        # tools, only orchestration_send takes agent_id with text (orchestration_message
        # takes target), so these arguments identify its calls.
        transcript = self.backend.get("/api/transcript?" + urllib.parse.urlencode({"id": child_id}))
        calls = []
        for item in transcript.get("items", []):
            if item.get("role") != "output":
                continue
            try:
                calls.append(json.loads(item.get("text") or "").get("arguments") or {})
            except (ValueError, AttributeError):
                continue
        sends = {(c.get("agent_id"), c.get("text")) for c in calls if isinstance(c, dict)}
        expected = {("parent", ROOM_TOKEN), ("broadcast", BROADCAST_TOKEN)}
        if not expected <= sends:
            raise CheckFailed(f"{CHILD_NAME} delivered the messages without orchestration_send; its calls: "
                              + json.dumps(calls)[:300])

        def lead_has_result() -> bool:
            events = [e["value"] for e in self.backend.entities() if e["collection"] == "event"
                      and e["value"].get("agent") == self.lead and e["value"].get("kind") == "child_result"]
            status, _ = self.answers(self.lead)
            if status in FAILED_STATUSES:
                raise CheckFailed(f"the lead became {status} after the child result")
            return bool(events) and all(e.get("status") == "delivered" for e in events) and status == "completed"

        wait_until(deadline, "the child result did not reach the lead", lead_has_result)
        return f"{CHILD_NAME} {child_id[:8]}: room and broadcast delivered, child result delivered to the lead"

    def resume(self) -> str:
        if not self.lead_answered:
            raise CheckFailed("not run: the agent turn did not pass")
        handles = self.backend.get("/api/desktop").get("supervisor", {}).get("handles", [])
        old = next((h.get("pid") for h in handles if h.get("id") == "account:default"), None)
        children = dict(self.backend.provider_children())
        if not isinstance(old, int) or old not in children or not children[old].startswith(str(self.backend.state) + "/"):
            raise CheckFailed(f"the default provider PID {old} is not a child of this smoke's supervisor")
        stop_process(old, gone(old))
        if process_alive(old):
            raise CheckFailed(f"the provider PID {old} did not stop")
        self.backend.post("/api/messages", {"id": str(uuid.uuid4()), "room": self.lead, "text": (
            "Smoke check after a provider restart. Do not use tools. Reply RESUMED: followed by the exact "
            "word that you replied in your first turn of this chat.")})
        texts = self.wait_answer(self.lead, RESUME_SECONDS, lambda texts: any("RESUMED" in t for t in texts))
        answer = next(t for t in reversed(texts) if "RESUMED" in t)
        thread = str(self.agent(self.lead).get("threadId") or "")
        handles = self.backend.get("/api/desktop").get("supervisor", {}).get("handles", [])
        new = next((h.get("pid") for h in handles if h.get("id") == "account:default"), None)
        if thread != self.thread:
            raise CheckFailed(f"the lead moved to another thread ({self.thread[:13]} to {thread[:13]})")
        if new == old:
            raise CheckFailed("the provider did not restart")
        if TURN_TOKEN not in answer:
            raise CheckFailed(f"the resumed thread lost its history; answer: {answer[:120]!r}")
        return f"provider pid {old} to {new}, same thread, answer {answer[:40]!r}"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--app", default=DEFAULT_APP, help=f"installed app bundle (default: {DEFAULT_APP})")
    parser.add_argument("--url", default=DEFAULT_URL, help=f"live server (default: {DEFAULT_URL})")
    parser.add_argument("--read-only", action="store_true", help="run only the live read-only checks")
    parser.add_argument("--model", default="gpt-6-luna", help="model of the smoke agents (default: gpt-6-luna)")
    parser.add_argument("--effort", default="low", help="reasoning effort of the smoke agents (default: low)")
    parser.add_argument("--codex-home", help="Codex home of the smoke default account (default: the production "
                                             "default, CODEX_HOME unset, so ~/.codex)")
    parser.add_argument("--keep", action="store_true", help="keep the private state folder after a pass")
    options = parser.parse_args()
    # A stop request must still reach the cleanup below, which stops the smoke backend.
    for stop in (signal.SIGTERM, signal.SIGHUP):
        signal.signal(stop, lambda number, _frame: sys.exit(128 + number))
    started = time.monotonic()
    bundle = Bundle(Path(options.app))
    report = Report()
    print(f"Codex Studio installed smoke: {bundle.app}", flush=True)

    live = LiveChecks(bundle, options.url)
    report.run("build", live.build)
    backend_ok = report.run("live backend", live.backend)
    if backend_ok:
        report.run("live accounts", live.accounts)
        report.run("live providers", live.providers)
        report.run("live acks", live.acknowledgements)
        report.run("live renderer", live.renderer)
    else:
        for name in ("live accounts", "live providers", "live acks", "live renderer"):
            report.skip(name, "not run: the live backend check failed")

    flows = ("sign-in", "agent turn", "orchestration", "resume")
    if not options.read_only:
        if not backend_ok or live.state is None:
            for name in ("smoke backend", "models", *flows):
                report.skip(name, "not run: the live backend check failed")
        else:
            smoke = SmokeBackend(bundle, live.config, live.state,
                                 urllib.parse.urlsplit(options.url).port or 80, options.codex_home)
            try:
                runner = Flows(smoke, options.model, options.effort)
                if report.run("smoke backend", smoke.start) and report.run("models", runner.models):
                    report.run("sign-in", runner.sign_in)
                    report.run("agent turn", runner.turn)
                    report.run("orchestration", runner.team)
                    report.run("resume", runner.resume)
                else:
                    for name in flows:
                        report.skip(name, "not run: the smoke backend or model check failed")
            finally:
                left = smoke.stop()
                if left:
                    report.skip("cleanup", "processes left: " + "; ".join(left))
                elif report.failed or options.keep:
                    print(f"Evidence: {smoke.root}", flush=True)
                else:
                    kept = smoke.remove()
                    if kept:
                        report.skip("cleanup", kept)

    failed = report.failed
    total = time.monotonic() - started
    print(f"Result: {'FAIL' if failed else 'PASS'} ({len(report.results)} checks, {len(failed)} failed) "
          f"in {total:.1f} s", flush=True)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
