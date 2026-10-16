"""Python auth-flow receipts from real writers and scripted provider boundaries."""
import json
import os
from pathlib import Path
import sys
import tempfile
import threading
from types import SimpleNamespace
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).parent / "turn_lifecycle"))
from oracle_guard import install_guard
install_guard(block_processes=True)
sys.path.insert(0, str(Path(__file__).resolve().parents[3] / "apps/server/src"))
from codex_accounts import AccountStore
from codex_claude_login import LoginManager

CODEX_ID = "4b154e4b-80bb-4f1f-b07f-dcaa15563190"
CLAUDE_ADD_ID = "e0166868-592a-4dca-8994-04d34a730af5"
CLAUDE_START_ID = "987edcdc-0e86-4fdf-a1c6-1513517d3ad0"
URL = "https://claude.com/cai/oauth/authorize?fixture=1"
LOGIN = {"type": "chatgptDeviceCode", "loginId": "fixture-login",
         "verificationUrl": "https://example.invalid/device", "userCode": "fixture-code"}


class FakeServer:
    def call(self, method, params, timeout):
        assert timeout in (10, 20)
        if method == "account/login/start":
            assert params == {"type": "chatgptDeviceCode"}
            return LOGIN
        assert method == "account/login/cancel"
        assert params == {"loginId": "fixture-login"}
        return {"status": "canceled"}


class FakeRuntime:
    def connect(self, _key, **_kwargs):
        return FakeServer()


class FakeInput:
    def __init__(self):
        self.writes = []

    def write(self, value):
        self.writes.append(value)

    def flush(self):
        pass


class FakeProcess:
    pid = 424242

    def __init__(self):
        self.stdin = FakeInput()

    def poll(self):
        return None


def codex_case(root):
    home = root / "codex-home"
    home.mkdir()
    os.environ.update(HOME=str(root / "user"), CODEX_HOME=str(home),
                      XDG_STATE_HOME=str(root / "xdg-state"),
                      STUDIO_CLAUDE_BIN=str(root / "fake-claude"))
    store = AccountStore(root / "codex-runtime")
    initial = json.loads(store.path.read_text())
    store.login_receipts = lambda: []
    with patch("codex_accounts.time.time", return_value=1_900_000_000.0):
        started = store.start_login(FakeRuntime(), CODEX_ID, label=None)
        replay = store.start_login(FakeRuntime(), CODEX_ID, label=None)
        cancelled = store.cancel_login(FakeRuntime(), CODEX_ID)
        cancel_replay = store.cancel_login(FakeRuntime(), CODEX_ID)
    return {"initial": initial, "responses": [started, replay, cancelled, cancel_replay],
            "stored": json.loads(store.path.read_text()),
            "accountsRoot": str(store.root), "root": str(root)}


class ClaudeAccounts:
    def __init__(self, profile):
        self.lock = threading.RLock()
        self.profile = profile

    def _row(self, key):
        assert key == "claude-profile"
        return self.profile

    def refresh(self, _key):
        return {"status": "signedOut"}


def claude_case(root, adding):
    request = CLAUDE_ADD_ID if adding else CLAUDE_START_ID
    config = root / ("profile-config" if not adding else "unused-add-config")
    config.mkdir()
    profile = {"id": "claude-profile", "provider": "claude", "accountId": "claude:fixture@example.test",
               "email": "fixture@example.test", "status": "ready", "claudeOptions": {
                   "binaryPath": str(root / "scripted-claude"), "configDir": str(config),
                   "customModels": [], "launchArgs": ""}}
    runtime = SimpleNamespace(root=str(root), accounts=ClaudeAccounts(profile))
    Path(runtime.root).mkdir(exist_ok=True)
    manager = LoginManager(runtime)
    with patch("codex_claude_login.threading.Thread.start", lambda _thread: None), \
         patch("codex_claude_login.codex_claude.installed", return_value="/fixture/scripted"), \
         patch("codex_claude_login.os.killpg", lambda *_args: None):
        if adding:
            start = manager.start_add("fixture@example.test", "Fixture Claude", request)
            replay = manager.start_add("fixture@example.test", "Fixture Claude", request)
        else:
            start = manager.start("claude-profile", request)
            replay = manager.start("claude-profile", request)
        job = manager.jobs[request]
        process = FakeProcess()
        job["process"] = process
        manager._publish_verification_url(job, URL)
        pending = manager.status(request)
        code = manager.code(request, "fixture-code#fixture-state") if adding else None
        cancelled = manager.cancel(request)
        # The scripted child has no background reader, so run the worker's
        # finally-path cleanup after the fake process boundary reports exit.
        if adding:
            manager._cleanup_add(job)
        final_replay = (manager.start_add("fixture@example.test", "Fixture Claude", request)
                        if adding else manager.start("claude-profile", request))
        record = json.loads((manager.directory / (request + ".json")).read_text())
    return {"responses": [start, replay, pending, code, cancelled, final_replay],
            "stored": record, "writes": [item.decode() for item in process.stdin.writes],
            "directory": str(manager.directory), "root": str(root)}


with tempfile.TemporaryDirectory(prefix="wave9-login-parity-") as temporary:
    root = Path(temporary)
    for name in ("codex", "claude-add", "claude-start"):
        (root / name).mkdir()
    codex = codex_case(root / "codex")
    claude_add = claude_case(root / "claude-add", True)
    claude_start = claude_case(root / "claude-start", False)
    print(json.dumps({"codex": codex, "claudeAdd": claude_add,
                      "claudeStart": claude_start}))
