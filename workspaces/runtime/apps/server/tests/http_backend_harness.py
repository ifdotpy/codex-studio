"""Own an isolated backend process and expose only its HTTP boundary."""

from __future__ import annotations

from dataclasses import dataclass
import json
import os
from pathlib import Path
import queue
import shlex
import signal
import shutil
import statistics
import sqlite3
import subprocess
import sys
import threading
import time
import tempfile
from contextlib import closing
from typing import Any, Callable, TextIO
from urllib.error import HTTPError, URLError
from urllib.request import Request, urlopen
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from server_test_waits import (
    HANG_GUARD_SECONDS,
    POLL_INTERVAL_SECONDS,
    current_condition_deadline,
    wait_for_condition,
)


REPOSITORY_ROOT = Path(__file__).resolve().parents[5]
SERVER_SCRIPT = REPOSITORY_ROOT / "workspaces/runtime/apps/server/src/codex-canvas"
SUPERVISOR_SCRIPT = REPOSITORY_ROOT / "workspaces/runtime/apps/server/src/codex_process_supervisor.py"
PROVIDER_REPLAY = Path(__file__).with_name("provider-replay-server.py")
PROVIDER_FIXTURES = Path(__file__).parent / "fixtures/provider-replay"
# One generous guard for a genuinely hung backend/provider interaction.
HTTP_WAIT_GUARD_SECONDS = HANG_GUARD_SECONDS
HTTP_WAIT_POLL_SECONDS = POLL_INTERVAL_SECONDS
STARTUP_TIMEOUT_SECONDS = HTTP_WAIT_GUARD_SECONDS
STOP_TIMEOUT_SECONDS = 4.0
PROVIDER_SELECTION_ENVIRONMENT = frozenset({
    "CODEX_BIN",
    "CODEX_AGENTS_NATIVE_RUNTIME_PIN",
    "CLAUDE_BIN",
    "STUDIO_CLAUDE_BIN",
    "STUDIO_NODE_BIN",
})
PROVIDER_CREDENTIAL_ENVIRONMENT = frozenset({
    "CODEX_API_KEY",
    "OPENAI_API_KEY",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "AWS_BEARER_TOKEN_BEDROCK",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_ACCESS_TOKEN",
    "CLAUDE_CODE_OAUTH_REFRESH_TOKEN",
})

# Dispatcher- and scheduler-owned fields that can change between an HTTP
# response and the test's post-request read. Values are paths within each row's
# JSON record (or, for plain columns, the column name). Keep this as the sole
# authority; docs/testing.md points here.
BACKGROUND_OWNED_FIELDS: dict[str, frozenset[str]] = {
    "runtime_agents": frozenset({
        # Dispatcher records which required complaints it presented at codex_runtime.py:7063.
        "complaintsPresented",
        # Dispatcher reconciles normalized settings into the active attempt at codex_runtime.py:6943.
        "startAttempt.settingsFixed",
    }),
    "runtime_agent_record_generation": frozenset({
        # Scheduler roster invalidation counter is maintained by agent triggers at codex_runtime.py:1607-1620.
        "value",
    }),
    "runtime_scheduler_agent_changes": frozenset({
        # Scheduler journal rows are maintained by agent triggers at codex_runtime.py:2855-2868.
        "generation",
    }),
    "runtime_scheduler_agent_change_state": frozenset({
        # Scheduler journal cursor is maintained by agent triggers at codex_runtime.py:2860-2871.
        "floor", "generation", "entries", "total",
    }),
    "runtime_event_meta": frozenset({
        # Dispatcher timings/projections are written in codex_runtime.py:4302,
        # 4354, 4368, 6571-6871, 6854, 6879, 7138-7144, 7264 and codex_efficiency.py:715.
        "timing.dispatchPickedAt", "timing.startBeganAt", "timing.nativeSubmitBeganAt",
        "timing.submittedAt", "timing.acceptedAt",
        "timing.fastQueuedAt", "timing.fastSkipReason", "timing.fastScheduledAt",
        "timing.fastEnteredAt", "timing.fastLockedAt", "timing.fastLockOwners",
        "timing.fastIndexesReadyAt", "timing.fastTeamCheckedAt", "timing.fastAgentLoadedAt",
        "timing.fastActiveScanAt", "timing.fastCandidatesAt", "timing.fastCapacityCheckedAt",
        "timing.fastBudgetCheckedAt", "timing.fastAccountCheckedAt", "timing.fastRadioCheckedAt",
        "timing.fastWorkspaceScanAt", "timing.fastToolGateAt", "timing.fastBatchLoadedAt",
        "timing.fastActorReloadedAt", "timing.fastDispatchDoneAt", "modelEventProjection",
        "contextManifest", "transcriptItemId",
    }),
    # The dispatcher sets this in codex_runtime.py:6943; reconcile_agent copies
    # it into this projection in codex_execution.py:255-259.
    "runtime_execution_attempts": frozenset({"settingsFixed"}),
    "sync_entities": frozenset({
        # Same dispatcher-only field is copied into the agent entity at codex_runtime.py:3302,3372.
        "payload.value.complaintsPresented",
        # Agent projection carries dispatcher-normalized attempts via codex_runtime.py:3302,3372.
        "payload.value.startAttempt.settingsFixed",
    }),
    # Rule tick updates its scheduling and liveness projection in codex_rules.py:420-483.
    "runtime_rules": frozenset({
        "status", "inFlight", "activeWorkers", "nextAt", "lastAt", "checks", "wakes",
        "lowSince", "alerted", "error", "previousOutcome", "lastExitCode",
        "lastStallExitCode", "lastStallError", "lastOutput", "lastFinished", "lastEvent",
        "eventText", "fingerprint", "fileActivityAt", "fileGeneration",
        "stallWakeGeneration", "stallProbe", "stallEventKey", "stallText",
    }),
    # Monitor workers, output callbacks and stall checks advance these fields in codex_runtime.py:9895-10192.
    "runtime_monitors": frozenset({
        "status", "tail", "bytes", "activityAt", "activityGeneration",
        "stallWakeGeneration", "stallProbeGeneration", "stallProbeStarted", "lastStallAt",
        "lastLivenessResult", "exitCode", "error", "finished", "configurationPending",
        "operation", "reattachRecovery",
    }),
}


def no_write_snapshot(state: Path) -> tuple[object, ...]:
    """Snapshot request-visible databases, omitting only named background fields.

    A no-write proof compares what a request could write. Fields owned by the
    background dispatcher or scheduler are excluded by their table and field
    names; whole tables are never excluded, except the separate analytics.sqlite3
    database. Python writes that database only through Runtime startup and the
    background analytics capture/rollout collectors: codex_runtime.py:1811-1812,
    2407-2425, 2545-2592, 4139-4145, 7779, 7917-7921, 9052, 9209;
    codex_analytics.py:265-299, 301-324, 350-468, 493-768;
    codex_cost_usage.py:64-120; codex_streaming.py:154-189;
    codex_native_errors.py:287; native_notifications/dispatch.py:177;
    codex_analytics_history.py:348-351, 354-404, 468-829. HTTP analytics/cost
    handlers read it (studio_api/insights/router.py:143-155); no HTTP handler
    writes it. An absent table and a present empty table compare equally, but
    any row in another newly created table remains visible.
    """
    database_snapshots = []
    for database_path in sorted(state.glob("*.sqlite3")):
        if database_path.name == "analytics.sqlite3":
            continue
        with closing(sqlite3.connect(f"file:{database_path}?mode=ro", uri=True)) as database:
            tables = [
                row[0] for row in database.execute(
                    "SELECT name FROM sqlite_master WHERE type='table' "
                    "AND (name NOT LIKE 'sqlite_%' OR name='sqlite_sequence') ORDER BY name"
                )
            ]
            snapshots = []
            for table in tables:
                columns = tuple(row[1] for row in database.execute(f'PRAGMA table_info("{table}")'))
                excluded = BACKGROUND_OWNED_FIELDS.get(table, frozenset())
                retained_columns = tuple(column for column in columns if column not in excluded)
                selected_columns = ",".join(f'"{column}"' for column in columns)
                rows = database.execute(f'SELECT {selected_columns} FROM "{table}"').fetchall()
                if not rows:
                    continue
                retained_rows = []
                for row in rows:
                    values = dict(zip(columns, row, strict=True))
                    for json_column in ("record", "payload"):
                        encoded = values.get(json_column)
                        if not isinstance(encoded, (str, bytes, bytearray)):
                            continue
                        try:
                            parsed = json.loads(encoded)
                        except (TypeError, ValueError):
                            continue
                        if not isinstance(parsed, dict):
                            continue
                        for field in excluded:
                            prefix = f"{json_column}."
                            if json_column == "record" and not field.startswith(("record.", "payload.")):
                                path = field
                            elif field.startswith(prefix):
                                path = field[len(prefix):]
                            else:
                                continue
                            parts = path.split(".")
                            current = parsed
                            for part in parts[:-1]:
                                current = current.get(part) if isinstance(current, dict) else None
                            if isinstance(current, dict):
                                current.pop(parts[-1], None)
                        values[json_column] = json.dumps(parsed, sort_keys=True, separators=(",", ":"))
                    retained_rows.append(tuple(values[column] for column in retained_columns))
                retained_rows.sort(key=repr)
                snapshots.append((table, retained_columns, tuple(retained_rows)))
            database_snapshots.append((database_path.name, tuple(snapshots)))
    return tuple(database_snapshots)


@dataclass(frozen=True)
class HttpResult:
    status: int
    headers: dict[str, str]
    body: bytes

    def json(self) -> Any:
        return json.loads(self.body)


class ProviderSelectionError(RuntimeError):
    """The backend selected a native provider other than the replay fixture."""


class HTTPTransportError(RuntimeError):
    """An HTTP connection failed and may succeed on the next condition poll."""


class BackendProcess:
    """Start the selected backend under isolated homes and a runner scratch root."""

    def __init__(
        self,
        scratch_root: Path,
        *,
        claude_bin: str | None = "/usr/bin/false",
        initialize_workspace: bool = True,
    ):
        self.scratch_root = scratch_root.resolve(strict=True)
        self.root = Path(tempfile.mkdtemp(prefix="http-boundary-", dir=self.scratch_root)).resolve(strict=True)
        if self.scratch_root not in self.root.parents:
            raise RuntimeError("Backend state escaped the runner scratch root")
        self.state = self._directory("state")
        self.home = self._directory("home")
        self.codex_home = self._directory("codex-home")
        self.claude_home = self._directory("claude-home")
        self.xdg_state = self._directory("xdg-state")
        # Runtime schema identity is content-addressed and safe to reuse between
        # sequential backends in this one runner-owned scratch root.
        self.xdg_cache = self.scratch_root / "http-boundary-xdg-cache"
        self.xdg_cache.mkdir(exist_ok=True)
        self.xdg_config = self._directory("xdg-config")
        self.cwd = self._directory("workspace")
        if initialize_workspace:
            subprocess.run(["git", "init", "--quiet", "--initial-branch=main"],
                           cwd=self.cwd, env={"PATH": os.defpath, "HOME": str(self.home),
                                              "GIT_CONFIG_NOSYSTEM": "1"},
                           check=True, timeout=HANG_GUARD_SECONDS)
        self.origin: str | None = None
        self.ready_seconds: float | None = None
        self.process_start_times: list[float] = []
        self.class_setup_seconds: float | None = None
        self.class_setup_phases: dict[str, float] = {}
        self._process: subprocess.Popen[str] | None = None
        self._supervisor_process: subprocess.Popen[str] | None = None
        self._supervisor_log: TextIO | None = None
        self._backend_python: Path | None = None
        self._claude_bin = claude_bin
        self._output: queue.Queue[str] = queue.Queue()
        self._reader: threading.Thread | None = None
        self._stderr: list[str] = []
        self._stderr_lock = threading.Lock()
        self._codex_log: Path | None = None
        self.session: dict[str, str] = {}
        self.identity: dict[str, Any] = {}
        self.schema_hash = ""
        self.ready_samples: list[float] = []
        self.start_phases: list[dict[str, float]] = []
        self.stop_phases: list[dict[str, float]] = []
        self.cleanup_seconds: float | None = None
        self._provider_start_log: Path | None = None
        self._provider_hold_method: str | None = None
        self._provider_hold_occurrence = 1
        self.lead_id = ""
        self.room_id = ""
        self.workspace_id = ""
        self.draft_id = ""
        self.work_body: dict[str, Any] = {}
        self.work_result: dict[str, Any] = {}
        self.provider_message: dict[str, str] = {}
        self.supervisor_protocol = False

    def _directory(self, leaf: str) -> Path:
        value = self.root / leaf
        value.mkdir(parents=True, exist_ok=True)
        return value.resolve(strict=True)

    def _command(self) -> list[str]:
        configured = os.environ.get("STUDIO_HTTP_BACKEND_COMMAND")
        if configured:
            command = shlex.split(configured)
        else:
            python = os.environ.get("STUDIO_HTTP_BACKEND_PYTHON", sys.executable)
            self._backend_python = Path(os.path.abspath(os.path.expanduser(python)))
            command = [python, "-B", str(SERVER_SCRIPT)]
        if not command:
            raise RuntimeError("STUDIO_HTTP_BACKEND_COMMAND is empty")
        port_arguments = [
            (index, value.split("=", 1)[1] if value.startswith("--port=") else
             command[index + 1] if value == "--port" and index + 1 < len(command) else None)
            for index, value in enumerate(command)
            if value == "--port" or value.startswith("--port=")
        ]
        if not port_arguments:
            command.extend(("--port", "0"))
        elif len(port_arguments) != 1 or port_arguments[0][1] != "0":
            raise RuntimeError("The backend harness requires exactly one ephemeral --port 0")
        return command

    def _environment(self) -> dict[str, str]:
        # A positive allow-list also strips unknown future provider selectors and
        # credentials. The caller environment is never a test Runtime boundary.
        environment = {
            key: value for key, value in os.environ.items()
            if key in {"LANG", "LC_ALL", "SYSTEMROOT", "WINDIR",
                       # Preserve Python subprocess text encoding independently of LANG.
                       "LC_CTYPE",
                       # setup-python needs its shared-library directory on Linux.
                       "LD_LIBRARY_PATH",
                       "STUDIO_TEST_FORBIDDEN_ROOTS", "STUDIO_TEST_GUARD_REPORT"}
            or key.startswith("GIT_CONFIG_")
        }
        # These settings select the Python test default. Never leak them to a
        # caller-supplied backend command.
        environment.pop("CODEX_AGENTS_PYTHON", None)
        environment.pop("STUDIO_HTTP_BACKEND_COMMAND", None)
        environment.pop("STUDIO_HTTP_BACKEND_PYTHON", None)
        for key in tuple(environment):
            if key in PROVIDER_SELECTION_ENVIRONMENT | PROVIDER_CREDENTIAL_ENVIRONMENT:
                environment.pop(key, None)
        values = {
            "HOME": str(self.home),
            "CODEX_HOME": str(self.codex_home),
            "CLAUDE_CONFIG_DIR": str(self.claude_home),
            "CLAUDE_SECURESTORAGE_CONFIG_DIR": str(self.claude_home),
            "CODEX_AGENTS_STATE_DIR": str(self.state),
            "CODEX_CANVAS_CWD": str(self.cwd),
            "XDG_CONFIG_HOME": str(self.xdg_config),
            "XDG_DATA_HOME": str(self._directory("xdg-data")),
            "XDG_CACHE_HOME": str(self.xdg_cache),
            "XDG_STATE_HOME": str(self.xdg_state),
            "XDG_RUNTIME_DIR": str(self._directory("xdg-runtime")),
            "TMPDIR": str(self.scratch_root),
            "TMP": str(self.scratch_root),
            "TEMP": str(self.scratch_root),
            "PYTHONDONTWRITEBYTECODE": "1",
            "HTTP_PROXY": "http://127.0.0.1:9",
            "HTTPS_PROXY": "http://127.0.0.1:9",
            "ALL_PROXY": "http://127.0.0.1:9",
            "NO_PROXY": "127.0.0.1,localhost",
        }
        # Match Python provider tests by default. Discovery tests can opt out to
        # exercise the no-Claude path without exposing a host executable.
        if self._claude_bin is not None:
            values["STUDIO_CLAUDE_BIN"] = self._claude_bin
        if self.supervisor_protocol:
            values["CODEX_AGENTS_SUPERVISOR_MODE"] = "1"
            values["CODEX_AGENTS_BACKEND_ID"] = "provider-fixture-backend"
        # Keep the equipped interpreter when this factory is called from system Python.
        if not os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"):
            values["CODEX_AGENTS_PYTHON"] = os.environ.get(
                "STUDIO_HTTP_BACKEND_PYTHON", os.environ.get("CODEX_AGENTS_PYTHON", sys.executable)
            )
        executable = getattr(self, "_provider_executable", None)
        # Never expose caller-installed provider candidates to the test backend.
        # Native discovery honors this explicit test pin before PATH or its
        # platform-specific ChatGPT location.
        path_entries = [str(executable.parent)] if executable is not None else []
        if self._backend_python is not None:
            path_entries.append(str(self._backend_python.parent))
        path_entries.extend(os.defpath.split(os.pathsep))
        values["PATH"] = os.pathsep.join(path_entries)
        # The replay executable is the only Codex candidate visible to the
        # child. Installed Codex and Claude commands must stay unreachable.
        selected_codex = shutil.which("codex", path=values["PATH"])
        if executable is None or selected_codex != str(executable):
            raise RuntimeError("The backend child PATH does not resolve only to the replay Codex")
        if shutil.which("claude", path=values["PATH"]) is not None:
            raise RuntimeError("The backend child PATH exposes a Claude executable")
        # The backend starts Node helpers (the Ed25519 service). Name the caller's
        # Node explicitly instead of exposing the caller's PATH.
        node = os.environ.get("CODEX_NODE") or shutil.which("node")
        if node:
            values["CODEX_NODE"] = node
        environment.update(values)
        if executable is not None:
            environment["CODEX_BIN"] = str(executable)
            environment["CODEX_AGENTS_NATIVE_RUNTIME_PIN"] = str(executable)
            environment["PROVIDER_REPLAY_PHASE"] = str(self.root / "provider-phase")
            environment["PROVIDER_REPLAY_RELEASE"] = str(self.root / "provider-release")
            environment["PROVIDER_REPLAY_GATE_PHASE"] = str(self.root / "provider-gate-phase")
            environment["PROVIDER_REPLAY_GATE_RELEASE"] = str(self.root / "provider-gate-release")
            environment["PROVIDER_REPLAY_WAIT_GUARD_SECONDS"] = str(
                HTTP_WAIT_GUARD_SECONDS
            )
            if self._provider_hold_method is not None:
                environment["PROVIDER_REPLAY_HOLD_METHOD"] = self._provider_hold_method
                environment["PROVIDER_REPLAY_HOLD_OCCURRENCE"] = str(
                    self._provider_hold_occurrence
                )
        return environment

    def start(self, *, allow_sync_unavailable: bool = False) -> float:
        if self._process is not None:
            raise RuntimeError("Backend is already running")
        if getattr(self, "_provider_executable", None) is None:
            raise RuntimeError("HTTP backend harness requires an explicit replay provider")
        command = self._command()
        environment = self._environment()
        if os.environ.get("STUDIO_HTTP_BACKEND_COMMAND") and not (self.state / "canvas.sqlite3").is_file():
            self._bootstrap_python_state(environment)
        if self.supervisor_protocol and self._supervisor_process is None:
            self._supervisor_log = (self.root / "supervisor.log").open("w", encoding="utf-8")
            self._supervisor_process = subprocess.Popen(
                [sys.executable, "-B", str(SUPERVISOR_SCRIPT), "--state", str(self.state)],
                cwd=self.cwd,
                env=environment,
                stdout=self._supervisor_log,
                stderr=subprocess.STDOUT,
                start_new_session=(os.name != "nt"),
            )
            wait_for_condition(
                lambda: (self.state / "supervisor.sock").exists(),
                "disposable supervisor socket did not appear",
                retry_exceptions=(),
                timeout_exception=TimeoutError,
            )
        if self._codex_log is not None:
            environment["CODEX_FAKE_RECEIVED"] = str(self._codex_log)
        started = time.monotonic()
        self.process_start_times.append(started)
        self._process = subprocess.Popen(
            command,
            cwd=self.cwd,
            env=environment,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            start_new_session=(os.name != "nt"),
        )
        assert self._process.stdout is not None
        assert self._process.stderr is not None
        self._reader = threading.Thread(
            target=self._read_stdout, args=(self._process.stdout,), daemon=True
        )
        self._reader.start()
        threading.Thread(
            target=self._read_stderr, args=(self._process.stderr,), daemon=True
        ).start()
        deadline = started + STARTUP_TIMEOUT_SECONDS
        ready_line_at: float
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                self._raise_backend_not_ready("startup guard expired", self._process.poll())
            try:
                line = self._output.get(timeout=min(0.1, remaining))
                break
            except queue.Empty:
                status = self._process.poll()
                if status is not None:
                    self._raise_backend_not_ready("process exited before the ready line", status)
        ready_line_at = time.monotonic()
        prefix = "Codex Canvas: "
        if not line.startswith(prefix):
            self.stop()
            raise RuntimeError(f"Backend did not emit its ready origin: {line!r}; {self.stderr}")
        self.origin = line.removeprefix(prefix).strip()
        port = int(self.origin.rsplit(":", 1)[1])
        if port == 4620:
            self.stop()
            raise RuntimeError("Refusing to connect to the live backend port 4620")
        if not self.origin.startswith("http://127.0.0.1:"):
            self.stop()
            raise RuntimeError(f"Backend origin is not loopback: {self.origin!r}")
        last_health_error: Exception | None = None
        health_started = time.monotonic()
        while time.monotonic() < deadline:
            try:
                health = self.request("GET", "/api/session", timeout=max(0.1, deadline-time.monotonic()))
                if health.status == 200:
                    self.session = health.json()
                    break
                last_health_error = RuntimeError(f"health returned HTTP {health.status}")
                time.sleep(min(HTTP_WAIT_POLL_SECONDS, max(0.0, deadline-time.monotonic())))
            except (RuntimeError, URLError, OSError) as error:
                last_health_error = error
                time.sleep(min(HTTP_WAIT_POLL_SECONDS, max(0.0, deadline-time.monotonic())))
        else:
            self.stop()
            raise RuntimeError(f"Backend health request exceeded its bound: {last_health_error}; {self.stderr}")
        health_ready_at = time.monotonic()
        last_identity_error: Exception | None = None
        while time.monotonic() < deadline:
            try:
                identity = self.request(
                    "GET", "/api/sync/identity",
                    headers={"X-Studio-API-Schema": "probe"},
                    timeout=max(0.1, deadline-time.monotonic()),
                )
                if identity.status == 200:
                    self.identity = identity.json()
                    self.schema_hash = identity.headers["x-studio-api-schema"]
                    break
                if identity.status == 501 and allow_sync_unavailable:
                    self.identity = {}
                    self.schema_hash = identity.headers["x-studio-api-schema"]
                    break
                last_identity_error = RuntimeError(
                    f"contract identity returned HTTP {identity.status}"
                )
                time.sleep(min(HTTP_WAIT_POLL_SECONDS, max(0.0, deadline-time.monotonic())))
            except (RuntimeError, URLError, OSError) as error:
                last_identity_error = error
                time.sleep(min(HTTP_WAIT_POLL_SECONDS, max(0.0, deadline-time.monotonic())))
        else:
            self.stop()
            raise RuntimeError(
                f"Backend contract identity exceeded its readiness bound: "
                f"{last_identity_error}; {self.stderr}"
            )
        identity_ready_at = time.monotonic()
        if self._provider_executable is None:
            self.stop()
            raise RuntimeError("HTTP backend harness requires an explicit replay provider")
        expected_provider = self._provider_executable.resolve()
        last_provider_status: object = None
        while time.monotonic() < deadline:
            try:
                desktop = self.request(
                    "GET", "/api/desktop",
                    timeout=max(0.1, deadline - time.monotonic()),
                ).json()
                native = desktop.get("nativeRuntime")
                selected = native.get("selected") if isinstance(native, dict) else None
                if isinstance(selected, dict):
                    source_path = selected.get("sourcePath")
                    if not source_path:
                        raise ProviderSelectionError(
                            "HTTP backend reported native runtime selection without sourcePath"
                        )
                    selected_path = Path(source_path).resolve()
                    if selected_path != expected_provider:
                        raise ProviderSelectionError(
                            "HTTP backend selected a non-replay native provider: "
                            f"{selected_path}; expected {expected_provider}"
                        )
                    break
                last_provider_status = native
                time.sleep(min(HTTP_WAIT_POLL_SECONDS, max(0.0, deadline - time.monotonic())))
            except ProviderSelectionError:
                self.stop()
                raise
            except (RuntimeError, URLError, OSError, ValueError) as error:
                last_provider_status = error
                time.sleep(min(HTTP_WAIT_POLL_SECONDS, max(0.0, deadline - time.monotonic())))
        else:
            self.stop()
            raise RuntimeError(
                "HTTP backend did not report its pinned replay provider before readiness: "
                f"{last_provider_status}; {self.stderr}"
            )
        self.ready_seconds = identity_ready_at - started
        self.start_phases.append({
            "spawnToReadyAnnouncementSeconds": round(ready_line_at - started, 4),
            "healthRequestSeconds": round(health_ready_at - health_started, 4),
            "identityRequestSeconds": round(identity_ready_at - health_ready_at, 4),
            "totalReadySeconds": round(self.ready_seconds, 4),
        })
        return self.ready_seconds

    def _bootstrap_python_state(self, environment: dict[str, str]) -> None:
        """Initialize a disposable state before a read-only Rust backend opens it."""
        python = os.environ.get("STUDIO_HTTP_BACKEND_PYTHON", sys.executable)
        bootstrap_environment = environment.copy()
        bootstrap_environment["CODEX_AGENTS_PYTHON"] = python
        bootstrap_environment["PYTHONUNBUFFERED"] = "1"
        log_path = self.root / "python-state-bootstrap.log"
        with log_path.open("w", encoding="utf-8") as log:
            process = subprocess.Popen(
                [python, "-B", str(SERVER_SCRIPT), "--port", "0"],
                cwd=self.cwd,
                env=bootstrap_environment,
                stdout=log,
                stderr=subprocess.STDOUT,
                start_new_session=(os.name != "nt"),
            )
            try:
                wait_for_condition(
                    lambda: "Codex Canvas: http://127.0.0.1:" in log_path.read_text(encoding="utf-8")
                    or process.poll() is not None,
                    "Python runtime did not initialize disposable state",
                    retry_exceptions=(OSError,),
                    timeout_exception=TimeoutError,
                )
                if process.poll() is not None:
                    raise RuntimeError(
                        "Python state bootstrap exited early: "
                        + log_path.read_text(encoding="utf-8")[-4000:]
                    )
            finally:
                if process.poll() is None:
                    process.terminate()
                try:
                    process.wait(timeout=STOP_TIMEOUT_SECONDS)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=2)

    def _raise_backend_not_ready(self, reason: str, exit_status: int | None) -> None:
        stderr_tail = self.stderr or "<empty>"
        message = (
            f"Backend did not emit its ready line: {reason}; "
            f"child exit status={exit_status}; stderr tail:\n{stderr_tail}"
        )
        if "studio_operations_native" in stderr_tail:
            message += "\n" + self._native_module_setup_hint()
        self.stop()
        raise RuntimeError(message)

    def _native_module_setup_hint(self) -> str:
        # The child environment is an allow-list without PYTHONPATH, so only the
        # backend interpreter's own site-packages can supply the native module.
        python = self._backend_python or "the configured backend interpreter"
        return (
            f"Test environment: the harness starts the backend with {python} and "
            "without the caller's PYTHONPATH, so a module copy on PYTHONPATH is not "
            "visible to it. Install the module in that interpreter's site-packages: "
            "run `cargo build --release -p studio-operations-python`, then "
            "`python3 workspaces/runtime/apps/server/src/install-cli.py --dev "
            "--bin-dir <folder>` under the XDG_CACHE_HOME that selected that "
            "interpreter. CI runs both steps "
            "in .github/actions/fast-tier-setup before the server suites."
        )

    @property
    def stderr(self) -> str:
        with self._stderr_lock:
            return "".join(self._stderr)[-4000:]

    def _read_stdout(self, stream: Any) -> None:
        for line in stream:
            self._output.put(line.rstrip("\r\n"))

    def _read_stderr(self, stream: Any) -> None:
        for line in stream:
            with self._stderr_lock:
                self._stderr.append(line)
                del self._stderr[:-80]

    def request(
        self,
        method: str,
        path: str,
        *,
        body: Any = None,
        headers: dict[str, str] | None = None,
        timeout: float = HTTP_WAIT_GUARD_SECONDS,
    ) -> HttpResult:
        if self.origin is None:
            raise RuntimeError("Backend is not ready")
        if self.origin.endswith(":4620"):
            raise RuntimeError("Refusing to connect to port 4620")
        payload = None if body is None else json.dumps(body, separators=(",", ":")).encode()
        request_headers = dict(headers or {})
        if payload is not None:
            request_headers.setdefault("Content-Type", "application/json")
        condition_deadline = current_condition_deadline()
        if condition_deadline is not None:
            remaining = condition_deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError(
                    f"HTTP condition exceeded its shared "
                    f"{HTTP_WAIT_GUARD_SECONDS:g}s guard"
                )
            timeout = min(timeout, remaining)
        try:
            with urlopen(Request(self.origin + path, data=payload, headers=request_headers,
                                 method=method), timeout=timeout) as response:
                return HttpResult(response.status,
                                  {key.lower(): value for key, value in response.headers.items()},
                                  response.read())
        except HTTPError as error:
            with error:
                return HttpResult(error.code,
                                  {key.lower(): value for key, value in error.headers.items()},
                                  error.read())
        except URLError as error:
            raise HTTPTransportError(f"HTTP request failed at {path}: {error}") from error

    def create_codex_replay(self, fixture_name: str) -> Path:
        (self.codex_home / "auth.json").write_text(
            json.dumps({"OPENAI_API_KEY": "fixture-only-replay-key"}),
            encoding="utf-8",
        )
        fixture = (PROVIDER_FIXTURES / fixture_name).resolve(strict=True)
        if PROVIDER_FIXTURES.resolve() not in fixture.parents:
            raise ValueError("Provider replay fixture escaped its fixture directory")
        replies = (PROVIDER_FIXTURES / "codex-rpc-replies.json").resolve(strict=True)
        self._codex_log = self.root / "codex-frames.jsonl"
        self._provider_start_log = self.root / "codex-starts.txt"
        provider_start_log = str(self._provider_start_log)
        (self.root / "provider-release").touch()
        executable = self.root / "bin" / "codex"
        executable.parent.mkdir(parents=True, exist_ok=True)
        executable.write_text(
            f"#!{sys.executable}\n"
            "import json, os, struct, sys, time\n"
            f"fixture = {str(fixture)!r}\n"
            f"replies = {str(replies)!r}\n"
            f"replay = {str(PROVIDER_REPLAY)!r}\n"
            "if sys.argv[1:] == ['--version']:\n"
            "    print('codex-cli 0.1.0')\n"
            "elif sys.argv[1:3] == ['app-server', 'generate-json-schema']:\n"
            "    output = sys.argv[sys.argv.index('--out') + 1]\n"
            "    os.makedirs(os.path.join(output, 'v2'), exist_ok=True)\n"
            "    requests = ['initialize', 'config/read', 'thread/loaded/list', 'turn/start',\n"
            "        'thread/metadata/update', 'thread/queue/list', 'thread/backgroundTerminals/list',\n"
            "        'thread/settings/update', 'model/list']\n"
            "    schema = {'oneOf': [{'properties': {'method': {'enum': requests}}}]}\n"
            "    turn = {'properties': {'cyberAccessProgram': {}}, 'definitions':\n"
            "        {'CyberAccessProgram': {'enum': ['standard', 'daybreakBlue', 'daybreakRed']}}}\n"
            "    models = {'definitions': {'Model': {'properties': {'availableAccessPrograms': {}}}}}\n"
            "    for name, value in [('ClientRequest.json', schema),\n"
            "        ('v2/TurnStartParams.json', turn), ('v2/ModelListResponse.json', models)]:\n"
            "        with open(os.path.join(output, name), 'w', encoding='utf-8') as stream:\n"
            "            json.dump(value, stream)\n"
            "elif sys.argv[1:2] == ['app-server']:\n"
            f"    with open({provider_start_log!r}, 'a', encoding='utf-8') as stream:\n"
            "        stream.write(str(time.monotonic()) + '\\n')\n"
            "    os.execv(sys.executable, [sys.executable, replay, fixture, replies])\n"
            "else:\n"
            "    raise SystemExit('unsupported fake Codex command')\n",
            encoding="utf-8",
        )
        executable.chmod(0o755)
        companion = executable.with_name("codex-code-mode-host")
        companion.write_text(
            f"#!{sys.executable}\n"
            "import json, struct, sys\n"
            "if sys.argv[1:] == ['--help']:\n"
            "    print('Usage: codex-code-mode-host --listen stdio')\n"
            "    raise SystemExit(0)\n"
            "header = sys.stdin.buffer.read(4)\n"
            "if len(header) != 4:\n"
            "    raise SystemExit(1)\n"
            "payload = sys.stdin.buffer.read(struct.unpack('<I', header)[0])\n"
            "json.loads(payload)\n"
            "response = json.dumps({'type': 'connection/ready', 'selectedVersion': 1,\n"
            "    'capabilities': []}).encode()\n"
            "sys.stdout.buffer.write(struct.pack('<I', len(response)) + response)\n",
            encoding="utf-8",
        )
        companion.chmod(0o755)
        # CODEX_BIN is set on the next start, after this explicit provider setup.
        self._provider_executable = executable
        return self._codex_log

    def hold_provider(self, method: str, occurrence: int = 1) -> None:
        """Hold one replay RPC until release_provider_gate is called."""
        if self._process is not None:
            raise RuntimeError("Provider gates must be configured before backend start")
        if not method or occurrence < 1:
            raise ValueError("Provider gate needs a method and positive occurrence")
        self._provider_hold_method = method
        self._provider_hold_occurrence = occurrence
        (self.root / "provider-gate-release").unlink(missing_ok=True)

    def provider_gate_started(self) -> bool:
        return (self.root / "provider-gate-phase").exists()

    def release_provider_gate(self) -> None:
        (self.root / "provider-gate-release").touch()

    def wait_until(self, condition: Callable[[], Any], description: str) -> Any:
        """Wait for an observable fixture condition with one shared hang guard."""
        return wait_for_condition(
            condition,
            f"HTTP condition did not settle: {description}",
            retry_exceptions=(HTTPTransportError,),
            timeout_exception=TimeoutError,
        )

    def received_provider_frames(self) -> list[dict[str, Any]]:
        if self._codex_log is None or not self._codex_log.exists():
            return []
        contents = self._codex_log.read_text(encoding="utf-8")
        lines = contents.splitlines()
        if contents and not contents.endswith("\n"):
            lines = lines[:-1]
        return [json.loads(line) for line in lines]

    def provider_start_times(self) -> list[float]:
        if self._provider_start_log is None or not self._provider_start_log.exists():
            return []
        return [float(line) for line in self._provider_start_log.read_text(encoding="utf-8").splitlines()]

    def stop(self) -> None:
        process = self._process
        if process is None:
            return
        self._process = None
        stop_started = time.monotonic()
        exit_wait_seconds = 0.0
        forced = False
        try:
            if os.name == "nt":
                if process.poll() is None:
                    process.terminate()
            else:
                try:
                    os.killpg(process.pid, signal.SIGTERM)
                except ProcessLookupError:
                    pass
            if process.poll() is None:
                wait_started = time.monotonic()
                process.wait(timeout=STOP_TIMEOUT_SECONDS)
                exit_wait_seconds = time.monotonic() - wait_started
        except subprocess.TimeoutExpired:
            forced = True
            if os.name == "nt":
                process.kill()
            else:
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
            process.wait(timeout=2)
        finally:
            group_cleanup_started = time.monotonic()
            self.origin = None
            if os.name != "nt":
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
            for stream in (process.stdout, process.stderr):
                if stream is not None:
                    stream.close()
            self.stop_phases.append({
                "exitWaitSeconds": round(exit_wait_seconds, 4),
                "forcedKill": float(forced),
                "groupAndPipeCleanupSeconds": round(time.monotonic() - group_cleanup_started, 4),
                "totalStopSeconds": round(time.monotonic() - stop_started, 4),
            })

    def close(self) -> None:
        self.stop()
        cleanup_started = time.monotonic()
        if self._supervisor_process is not None:
            supervisor = self._supervisor_process
            self._supervisor_process = None
            supervisor.terminate()
            try:
                supervisor.wait(timeout=STOP_TIMEOUT_SECONDS)
            except subprocess.TimeoutExpired:
                supervisor.kill()
                supervisor.wait(timeout=2)
        if self._supervisor_log is not None:
            self._supervisor_log.close()
            self._supervisor_log = None
        resolved = self.root.resolve()
        if self.scratch_root not in resolved.parents:
            raise RuntimeError("Refusing to remove state outside the runner scratch root")
        shutil.rmtree(resolved)
        self.cleanup_seconds = time.monotonic() - cleanup_started


def start_contract_backend(
    test_case: type[Any],
    fixture_name: str = "restart-mid-turn-supervisor-replay.json",
    *,
    hold_provider_method: str | None = None,
    hold_provider_occurrence: int = 1,
    supervisor_protocol: bool = False,
) -> BackendProcess:
    """Start one isolated backend owned by a unittest class cleanup."""
    scratch_value = os.environ.get("TMPDIR")
    if not scratch_value:
        raise RuntimeError("The server runner must provide a memory-backed TMPDIR")
    construction_started = time.monotonic()
    backend = BackendProcess(Path(scratch_value))
    backend.supervisor_protocol = supervisor_protocol
    if hold_provider_method is not None:
        backend.hold_provider(hold_provider_method, hold_provider_occurrence)
    backend.class_setup_phases["harnessConstructionSeconds"] = round(
        time.monotonic() - construction_started, 4,
    )
    test_case.addClassCleanup(_report_contract_backend_timing, backend)
    test_case.addClassCleanup(backend.close)
    provider_started = time.monotonic()
    backend.create_codex_replay(fixture_name)
    backend.class_setup_phases["fakeProviderSetupSeconds"] = round(
        time.monotonic() - provider_started, 4,
    )
    ready = backend.start()
    backend.ready_samples.append(ready)
    backend.class_setup_seconds = time.monotonic() - construction_started
    return backend


def _report_contract_backend_timing(backend: BackendProcess) -> None:
    print(json.dumps({
        "classSetupSeconds": round(backend.class_setup_seconds or 0, 4),
        "classSetupPhases": backend.class_setup_phases,
        "backendReadyMedianSeconds": round(statistics.median(backend.ready_samples), 4)
        if backend.ready_samples else None,
        "backendReadyWorstSeconds": round(max(backend.ready_samples), 4)
        if backend.ready_samples else None,
        "backendStarts": len(backend.ready_samples),
        "backendStartPhases": backend.start_phases,
        "backendStopPhases": backend.stop_phases,
        "stateCleanupSeconds": round(backend.cleanup_seconds or 0, 4),
    }, sort_keys=True), flush=True)


class TimedHTTPContractTest:
    """Print focused per-contract timings for the process-boundary suite."""

    def setUp(self) -> None:
        self._contract_started = time.monotonic()

    def tearDown(self) -> None:
        print(json.dumps({"contract": self.id(), "seconds": round(
            time.monotonic() - self._contract_started, 4)}, sort_keys=True), flush=True)
