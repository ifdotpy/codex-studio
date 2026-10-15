"""Run discovered HTTP consumers against Rust and record operation responses."""
from __future__ import annotations

import argparse
import ast
from contextlib import closing, contextmanager, nullcontext
import hashlib
import json
import os
from pathlib import Path
import re
import shlex
import sqlite3
import subprocess
import sys
import tempfile
import time
import uuid
import urllib.parse
import urllib.request
import xml.etree.ElementTree as ET
from urllib.error import HTTPError
from collections import defaultdict
from typing import Any, Iterator


sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from server_test_contracts import P20_OPERATION_MANIFEST_ENV
from runtime_test_environment import runtime_environment
from codex_python import resolve_python

ROOT = Path(__file__).resolve().parents[5]
TESTS = ROOT / "workspaces/runtime/apps/server/tests"
SPEC = ROOT / "workspaces/runtime/packages/runtime-http/openapi.json"
HARNESS = TESTS / "http_backend_harness.py"
JSON_METHODS = {"POST", "PUT", "PATCH"}
DEFAULT_CACHE = Path(os.environ.get("XDG_CACHE_HOME", Path.home() / ".cache"))
DEFAULT_LEDGER = DEFAULT_CACHE / "codex-studio" / "rust-runtime-http-conformance-ledger.json"
COPY_MARKER = ".codex-studio-rehearsal-copy"
COPY_MARKER_CONTENT = "sqlite-online-backup-copy-v1\n"

# These existing Rust router tests are the only valid positive fixtures for
# operations whose session or signed-peer ports cannot be installed in the
# process-backed Python HTTP fixture. Keep this map closed and exact.
ROUTER_POSITIVE_TESTS: dict[tuple[str, str], tuple[str, str]] = {
    ("POST", "/api/connection-recovery"): (
        "studio-runtime-http::recovery_http",
        "connection_recovery_http_returns_the_synchronous_worker_status",
    ),
    ("POST", "/api/servers/orchestration"): (
        "studio-runtime-http::multi_server_incoming",
        "signed_remote_event_crosses_http_boundary_and_persists_once",
    ),
}


def validate_copy_state_root(
    value: Path,
    *,
    live_state_root: Path | None = None,
    configured_state_root: str | None = None,
) -> Path:
    """Require a marked SQLite copy outside every known live state location."""
    root = Path(value).expanduser().resolve(strict=True)
    protected_roots = [
        (Path.home() / ".local/state/codex-agents").resolve(),
    ]
    if live_state_root is not None:
        protected_roots.append(Path(live_state_root).expanduser().resolve())
    if configured_state_root:
        protected_roots.append(Path(configured_state_root).expanduser().resolve())
    for protected in protected_roots:
        if root == protected or protected in root.parents:
            raise ValueError(f"refusing live state directory or descendant: {root}")
    if not root.is_dir():
        raise ValueError("copy state root must be a directory")
    marker = root / COPY_MARKER
    if (not marker.is_file() or marker.is_symlink()
            or marker.read_text(encoding="utf-8") != COPY_MARKER_CONTENT):
        raise ValueError(f"copy state root lacks the {COPY_MARKER} marker")
    copy_database_stats: dict[str, Any] = {}
    for name in ("canvas.sqlite3", "analytics.sqlite3"):
        database = root / name
        if not database.is_file() or database.is_symlink():
            raise ValueError(f"copy state root requires a regular {name}")
        stat = database.stat()
        if stat.st_nlink > 1:
            raise ValueError(f"copy state database must not have hard links: {name}")
        _validate_copy_database(database, name)
        copy_database_stats[name] = stat
    known_live_roots = [Path.home() / ".local/state/codex-agents"]
    if live_state_root is not None:
        known_live_roots.append(Path(live_state_root).expanduser())
    if configured_state_root:
        known_live_roots.append(Path(configured_state_root).expanduser())
    for live_root in known_live_roots:
        for name, copy_stat in copy_database_stats.items():
            try:
                live_stat = (live_root / name).stat()
            except OSError:
                continue
            if (copy_stat.st_dev, copy_stat.st_ino) == (live_stat.st_dev, live_stat.st_ino):
                raise ValueError(f"copy state database aliases a known live database: {name}")
    allowed = {COPY_MARKER, "canvas.sqlite3", "analytics.sqlite3"}
    unexpected = sorted(path.name for path in root.iterdir() if path.name not in allowed)
    if unexpected:
        raise ValueError(
            "copy state root must contain only its marker and SQLite databases; "
            f"unexpected entries: {', '.join(unexpected)}"
        )
    return root


def _validate_copy_database(database: Path, name: str) -> None:
    """Check a copied database's SQLite structure without opening it writable."""
    with database.open("rb") as source:
        if source.read(16) != b"SQLite format 3\x00":
            raise ValueError(f"copy state database has an invalid SQLite header: {name}")
    uri = "file:" + urllib.parse.quote(str(database), safe="/") + "?mode=ro&immutable=1"
    try:
        with closing(sqlite3.connect(uri, uri=True)) as connection:
            if connection.execute("PRAGMA quick_check").fetchone() != ("ok",):
                raise ValueError(f"copy state database failed SQLite integrity check: {name}")
            required = {
                "canvas.sqlite3": {"runtime_agents": {"id", "record"}},
                "analytics.sqlite3": {"analytics_usage": {"seq", "id", "agent", "record"}},
            }[name]
            for table, columns in required.items():
                rows = connection.execute(f'PRAGMA table_info("{table}")').fetchall()
                if not columns.issubset({row[1] for row in rows}):
                    raise ValueError(
                        f"copy state database has an incompatible schema: {name}/{table}"
                    )
    except sqlite3.DatabaseError as error:
        raise ValueError(f"copy state database is not a valid SQLite database: {name}") from error


def copied_agent_witness(root: Path) -> dict[str, str]:
    """Select a native agent already present in the copy for an HTTP read witness."""
    uri = "file:" + urllib.parse.quote(str(root / "canvas.sqlite3"), safe="/") + "?mode=ro&immutable=1"
    with closing(sqlite3.connect(uri, uri=True)) as connection:
        for identifier, raw in connection.execute(
            "SELECT id,record FROM runtime_agents ORDER BY id"
        ):
            try:
                record = json.loads(raw)
            except (TypeError, ValueError):
                continue
            if not isinstance(record, dict):
                continue
            cwd = record.get("cwd")
            if (
                record.get("id") == identifier
                and isinstance(cwd, str)
                and cwd
                and record.get("executionMode", "host") != "vm"
            ):
                return {"id": identifier, "cwd": cwd}
    raise ValueError("copy state database has no pre-existing native agent to verify")


def validate_ephemeral_backend_command(command: str) -> list[str]:
    """Reject fixed ports before the backend process can be spawned."""
    values = shlex.split(command)
    if not values:
        raise ValueError("Rust backend command must not be empty")
    ports: list[str] = []
    index = 0
    while index < len(values):
        value = values[index]
        if value == "--port":
            if index + 1 >= len(values):
                raise ValueError("--port requires a value")
            index += 1
            ports.append(values[index])
        elif value.startswith("--port="):
            ports.append(value.split("=", 1)[1])
        index += 1
    if any(port != "0" for port in ports):
        raise ValueError("copy-state ledger requires an OS-assigned ephemeral port; port 4620 is forbidden")
    if len(ports) > 1:
        raise ValueError("copy-state ledger accepts at most one --port 0")
    return values

# These tests stop at an operation whose manifest still names the missing
# service. Each exclusion is tied to that operation so it becomes stale when
# the corresponding dependency is implemented.
MISSING_SERVICE_TESTS: dict[str, dict[str, dict[str, str]]] = {
}

# This HTTP suite proves Python's explicit-null refusal. Its paired Rust
# evidence is the native `wave7_p04_callback_http` integration test.
PYTHON_BASELINE_ONLY_SUITES = frozenset({
    "workspaces/runtime/apps/server/tests/wave7-p04-native-http-contract.py",
})

# These tests need provider turn execution, but no current route declares that
# dependency. Keep them under one explicit unresolved-service group and fail
# if a route starts advertising the dependency before these exclusions are
# re-keyed to that operation.
UNDECLARED_PROVIDER_TURN_SERVICE = "turn-driver/provider-turn"
UNDECLARED_PROVIDER_TURN_TESTS: dict[str, dict[str, str]] = {
    "workspaces/runtime/apps/server/tests/tool-request-http-contract.py": {
        "__main__.ToolRequestHTTPContract.test_provider_tool_request_is_recoverable_and_scoped_by_actor": "Recovering the tool request requires a provider turn; no operation declares this dependency yet.",
    },
    "workspaces/runtime/apps/server/tests/http-boundary-contract.py": {
        "__main__.BoundaryContractTests.test_codex_provider_turn_uses_replay_executable": "The Rust HTTP contract fixture starts no persistent supervisor; provider-session/supervisor-transport is required to start a turn.",
        "__main__.RestartContractTests.test_restart_serves_sync_receipt_and_provider_facts": "The Rust HTTP contract fixture starts no persistent supervisor; provider-session/supervisor-transport is required before the restart scenario can reach a turn.",
    },
    "workspaces/runtime/apps/server/tests/protocol-fixtures-contract.py": {
        "__main__.ProtocolFixturesContract.test_supervisor_and_provider_fixtures_match_http_observations": "The supervisor-enabled fixture still times out before provider output reaches the transcript; the cause is not yet known and remains an open provider-session finding.",
    },
    "workspaces/runtime/apps/server/tests/yolo-contract.py": {
        "__main__.YoloHTTPContract.test_turns_after_yolo_is_disabled_use_restricted_provider_permissions": "The test executes provider turns; no operation declares this dependency yet.",
        "__main__.YoloHTTPContract.test_yolo_default_inherits_to_worker_and_selects_provider_permissions": "The test executes a worker provider turn; no operation declares this dependency yet.",
        "__main__.YoloWorkerMessageOrderingHTTPContract.test_message_ordering_before_and_after_worker_first_turn_is_deterministic": "The worker first turn requires provider turn execution; no operation declares this dependency yet.",
        "__main__.YoloRestartHTTPContract.test_disabled_yolo_mode_is_served_after_backend_restart": "The post-restart message waits for a provider turn; no operation declares this dependency yet.",
    },
}


def missing_service_tests_for_suite(relative: str) -> dict[str, dict[str, str]]:
    return dict(MISSING_SERVICE_TESTS.get(relative, {})) | {
        test_id: {
            "service": UNDECLARED_PROVIDER_TURN_SERVICE,
            "reason": reason,
            "operation": "requires a provider turn; no operation declares it yet",
        }
        for test_id, reason in UNDECLARED_PROVIDER_TURN_TESTS.get(relative, {}).items()
    }


def validate_missing_service_exclusions(
    manifest: dict[tuple[str, str], dict[str, Any]],
) -> None:
    for suite, tests in MISSING_SERVICE_TESTS.items():
        for test_id, details in tests.items():
            method, path = details["operation"].split(" ", 1)
            entry = manifest.get((method, path))
            if (
                entry is None
                or entry.get("status") != "partial"
                or details["service"] not in entry.get("dependencies", [])
            ):
                raise RuntimeError(
                    "stale missing-service exclusion: "
                    f"{suite}::{test_id} expects {details['service']} on {details['operation']}"
                )
    if any(
        UNDECLARED_PROVIDER_TURN_SERVICE in entry.get("dependencies", [])
        for entry in manifest.values()
    ):
        raise RuntimeError(
            "provider-turn exclusions are stale: a Rust operation now declares "
            f"{UNDECLARED_PROVIDER_TURN_SERVICE}; re-key the tests to that operation"
        )

# Probe inputs are keyed by the documented HTTP operation so a schema-valid
# sample can point at records created by the Python runtime seed. Each case may
# override query/header values and top-level body fields. A value prefixed with
# "$" names an identifier returned by seed_python_probe_state.
PROBE_CASES: dict[tuple[str, str], dict[str, Any]] = {
    ("POST", "/api/agents/account"): {
        "primary": {"body": {"id": "$agent", "account_key": "default"}, "replace_body": True,
                    "expected": "2xx", "assert_no_write": True},
    },
    ("POST", "/api/agents/account-transfer"): {
        "primary": {"body": {"id": "$agent", "account_key": "default", "scope": "team",
                             "request_id": "60606060-6060-4060-8060-606060606060"}, "replace_body": True,
                    "expected": "2xx"},
    },
    ("POST", "/api/connection-recovery"): {
        "primary": {"dispatch": False, "consumer_only": True,
                    "reason": "Positive evidence comes from the exact-candidate Rust router test named in ROUTER_POSITIVE_TESTS."},
    },
    ("GET", "/api/agent-chat"): {
        "primary": {"query": {"room": "$room"}, "query_omit": ["before", "after"], "expected": "2xx"},
    },
    ("GET", "/api/agents/finder-view"): {
        "primary": {
            "query": {"agent": "$agent"},
            "expected": "2xx",
        },
    },
    ("GET", "/api/analytics"): {
        "primary": {"query": {"agent": "$analyticsAgent"}, "query_omit": ["view", "detail", "item", "turn", "thread", "turns", "tool", "timing", "export"], "expected": "2xx"},
    },
    ("GET", "/api/capabilities"): {
        "primary": {"query": {"agent": "$agent"}, "expected": "2xx"},
    },
    ("GET", "/api/changes"): {
        "primary": {
            "query": {"agent": "$agent"}, "query_omit": ["scope"], "expected": "2xx",
        },
    },
    ("GET", "/api/federation"): {
        "primary": {
            "expected": "404",
            "reason": "The local federation management route has no GET counterpart.",
        },
    },
    ("GET", "/api/federation/v1/message"): {
        "primary": {
            "expected": "404",
            "reason": "Federation message delivery is POST-only.",
        },
    },
    ("GET", "/api/federation/v1/pair"): {
        "primary": {
            "expected": "404",
            "reason": "Federation pairing is POST-only.",
        },
    },
    ("GET", "/api/federation/v1/pull"): {
        "primary": {
            "expected": "404",
            "reason": "Federation state pull is POST-only.",
        },
    },
    ("GET", "/api/federation/v1/status"): {
        "primary": {
            "expected": "404",
            "reason": "Signed federation status is POST-only.",
        },
    },
    ("GET", "/api/models"): {
        "primary": {
            "query": {"account_key": "default"},
            "query_omit": ["workers", "retry"],
            "expected": "400",
        },
        "additional": [{
            "name": "unavailable Claude account",
            "query": {"account_key": "$claudeAccount"},
            "query_omit": ["workers", "retry"],
            "expected": "400",
        }],
    },
    ("GET", "/api/multi-server"): {
        "primary": {"expected": "2xx"},
        "additional": [{
            "name": "cached local identity",
            "fixture_mutations": {"remove_identity": True},
            "restore": ["identity_file"],
            "expected": "2xx",
        }],
    },
    ("GET", "/api/multi-server/v1/pair"): {
        "primary": {
            "expected": "404",
            "reason": "The pairing callback accepts POST only.",
        },
    },
    ("GET", "/api/checkpoints"): {
        "primary": {"query": {"agent": "$agent"}},
    },
    ("GET", "/api/diagnostics"): {
        "primary": {
            "expected": "2xx",
        },
    },
    ("GET", "/api/directories"): {
        "primary": {"query": {"server": "local", "path": "$projectPath"}, "expected": "2xx", "assert_no_write": True},
    },
    ("GET", "/api/import"): {
        "primary": {
            "query": {"account_key": "default"},
            "expected": "503",
            "assert_no_write": True,
        },
    },
    ("POST", "/api/import"): {
        "primary": {
            "body": {"threadId": "source-thread", "prompt": "Continue the synthetic fixture."},
            "expected": "503",
            "assert_no_write": True,
        },
    },
    ("GET", "/api/messages"): {
        "primary": {"query": {"room": "$room"}},
    },
    ("GET", "/api/messages/receipts"): {
        "primary": {"query": {"agent": "$agent", "ids": "[]"}, "expected": "2xx", "assert_no_write": True},
    },
    ("GET", "/api/panel"): {
        "primary": {"query": {"agent": "$agent"}},
    },
    ("GET", "/api/questions"): {
        "primary": {"query": {"agent": "$agent"}},
    },
    ("GET", "/api/rules"): {
        "primary": {"query": {"agent": "$agent"}},
    },
    ("GET", "/api/session-cost"): {
        "primary": {
            "query": {"agent": "$analyticsAgent"},
            "fixture_mutations": {"seed_pricing_catalog": True},
            "restore": ["pricing_file"],
            "expected": "2xx",
        },
        "additional": [{
            "name": "stale cost cache requires refresh",
            "query": {"agent": "$analyticsAgent"},
            "fixture_mutations": {"stale_session_cost_cache": True, "seed_pricing_catalog": True},
            "restore": ["session_cost_cache", "pricing_file"],
            "expected": "2xx",
        }],
    },
    ("GET", "/api/sync/stream"): {
        "primary": {"query": {"protocol": "3", "resources": "[{\"kind\":\"state\"}]", "apiSchema": "$schemaHash"}, "expected": "2xx"},
    },
    ("GET", "/api/skills"): {
        "primary": {"query": {"agent": "$agent"}},
    },
    ("GET", "/api/tool-requests"): {
        "primary": {"query": {"agent": "$agent", "request_id": "$toolReceipt"}, "expected": "2xx", "assert_no_write": True},
        "additional": [{"name": "list", "query": {"agent": "$agent"}, "query_omit": ["request_id"], "expected": "2xx", "assert_no_write": True}],
    },
    ("GET", "/api/transcript"): {
        "primary": {"query": {"id": "$agent"}, "expected": "2xx"},
    },
    ("GET", "/api/transcript/page"): {
        "primary": {
            "query": {"id": "$agent"},
            "query_omit": ["before", "around", "after"],
            "expected": "2xx",
        },
    },
    ("GET", "/api/accounts/claude/login"): {
        "primary": {"query": {"request_id": "88888888-8888-4888-8888-888888888888"}, "expected": "400"},
    },
    ("GET", "/api/multi-server/v1/identity"): {"primary": {"expected": "2xx"}},
    ("GET", "/api/multi-server/v1/status"): {"primary": {"expected": "403", "expected_error": "Status requires a signed paired-server request", "reason": "Status reads require a signed paired-server request; this probe has no signature headers.", "assert_no_write": True}},
    ("GET", "/api/project-locations"): {
        "primary": {"query": {"project": "ledger-missing-project", "server": "local"}, "expected": "400"},
    },
    ("GET", "/api/search/item"): {
        "primary": {"query": {"id": "$agent", "message_id": "$item"}, "expected": "2xx"},
    },
    ("GET", "/api/search"): {
        "primary": {"query": {"q": "ledger-probe"}, "expected": "2xx"},
    },
    ("GET", "/api/complaint"): {
        "primary": {"query": {"id": "$complaint"}},
    },
    ("GET", "/api/file"): {
        "primary": {
            "query": {"agent": "$agent", "path": "$filePath", "asset": ""},
            "expected": "2xx",
        },
    },
    ("GET", "/api/file-info"): {
        "primary": {
            "query": {"agent": "$agent", "path": "$filePath", "asset": ""},
            "expected": "2xx",
        },
    },
    ("POST", "/api/monitor/cancel"): {
        "primary": {"body": {"id": "$monitor"}, "expected": "2xx"},
    },
    ("POST", "/api/monitor/input"): {
        "primary": {"body": {"id": "$monitor", "text": ""}, "expected": "400"},
    },
    ("GET", "/api/monitor/log"): {
        "primary": {"query": {"id": "$monitor"}, "header": {"Range": "bytes=0-"}},
    },
    ("GET", "/api/terminals/output"): {
        "primary": {"query": {"id": "$terminal"}},
    },
    ("GET", "/api/transcript/item"): {
        "primary": {"query": {"id": "$agent", "message_id": "$item"}},
    },
    ("GET", "/api/transcript/search"): {
        "primary": {"query": {"id": "$agent", "q": "ledger-probe"}},
    },
    ("GET", "/api/workspace/tasks"): {
        "primary": {"query": {
            "agent": "$agent", "cursor": "", "before": "",
        }},
    },
    ("GET", "/api/workspace"): {
        "primary": {"query": {"agent": "$agent"}},
    },
    ("GET", "/api/limits"): {
        "primary": {"query": {"account_key": "default", "cached": "1"}, "expected": "2xx"},
        "additional": [{"name": "disconnected account limits snapshot", "query": {"account_key": "$disconnectedAccount"}, "expected": "2xx"}],
    },
    ("GET", "/api/task"): {
        "primary": {"query": {"id": "$task"}},
    },
    ("GET", "/api/plan"): {
        "primary": {"query": {"agent": "$agent"}},
    },
    ("GET", "/api/queue"): {
        "primary": {"query": {"agent": "$agent"}},
    },
    ("GET", "/api/work"): {
        "primary": {"query": {"agent": "$agent"}},
    },
    ("GET", "/api/costs"): {
        "primary": {"query": {"account_key": "default"}, "expected": "2xx", "assert_no_write": True},
    },
    ("GET", "/api/desktop"): {
        "primary": {"query": {"account_key": "default"}},
    },
    ("POST", "/api/annotation"): {
        "primary": {"body": {"agent": "$agent", "path": "$filePath"}},
    },
    ("POST", "/api/chats"): {
        "primary": {"body": {"id": "$chat", "name": "HTTP ledger probe chat", "members": ["$agent"]}},
    },
    ("POST", "/api/connections"): {
        "primary": {"body": {"source": "$agent", "target": "$chat"}},
    },
    ("POST", "/api/context-repair"): {
        "primary": {"body": {"id": "$agent"}, "expected": "2xx"},
    },
    ("POST", "/api/browser/forward"): {
        "primary": {"body": {}, "expected": "400", "assert_no_write": True},
    },
    ("POST", "/api/messages"): {
        "primary": {
            "body": {"id": "$message", "room": "$chat", "text": "Ledger message"},
            "expected": "2xx",
        },
        "additional": [{
            "name": "attachment IDs must be strings",
            "body": {
                "id": "77777777-7777-4777-8777-777777777777",
                "room": "$agent", "text": "", "assets": [{"name": "pending"}],
            },
            "expected": "400",
            "reason": "request schema rejects attachment objects; uploaded attachment IDs are strings",
        }],
    },
    ("POST", "/api/complaints"): {
        "primary": {
            "body": {
                "id": "ledger-probe-complaint-submit", "lead": "$agent",
                "text": "Please review the ledger probe.",
            },
            "expected": "2xx",
        },
    },
    ("POST", "/api/federation"): {
        "primary": {"body": {"action": "status"}, "replace_body": True, "expected": "2xx"},
        "additional": [{
            "name": "federation invitation dependency",
            "body": {"action": "create_invite", "label": "Ledger probe"},
            "replace_body": True,
            # The test state has federation disabled: codex_federation.py:316-318
            # raises ValueError and studio_api/federation/router.py:109-114 maps it to 400.
            "expected": "400",
            "assert_no_write": True,
        }],
    },
    ("POST", "/api/multi-server"): {
        "primary": {
            "body": {"action": "settings", "autoPair": False, "requestId": "ledger-probe-settings"},
            "replace_body": True,
            "expected": "2xx",
        },
        "additional": [{
            "name": "signed invite dependency",
            "body": {"action": "create_invite", "requestId": "ledger-probe-invite", "label": "Ledger probe"},
            "replace_body": True,
            "expected": "400",
            "expected_error": "Configure Tailscale Serve HTTPS before pairing",
            "reason": "The disposable backend has no Tailscale Serve HTTPS origin for invitation creation.",
            "assert_no_write": True,
        }],
    },
    ("POST", "/api/peer-teams"): {
        "primary": {
            "body": {
                "action": "delete", "path": "$projectPath", "expected_revision": 0,
                "team_id": "11111111-1111-4111-8111-111111111111",
                "request_id": "ledger-probe-peer-delete",
            },
            "replace_body": True,
            "expected": "2xx",
        },
        "additional": [{
            "name": "peer radio dependency",
            "body": {
                "action": "radio", "radio_action": "create", "path": "$projectPath",
                "request_id": "ledger-probe-peer-radio", "name": "Ledger probe",
                "participants": [
                    {"account_key": "default", "model": "fixture-model"},
                    {"account_key": "default", "model": "fixture-model-2"},
                ],
            },
            "replace_body": True,
            "expected": "400",
            "assert_no_write": True,
        }],
    },
    ("POST", "/api/projects"): {
        "primary": {
            "body": {"action": "register", "path": "$projectPath", "name": "Ledger project"},
            "replace_body": True,
            "expected": "2xx",
        },
        "additional": [{
            "name": "cross-server project registration dependency",
            "body": {
                "action": "register", "path": "$projectPath", "name": "Remote project",
                "server": "remote", "request_id": "ledger-cross-server-project",
            },
            "replace_body": True,
            "expected": "403",
            "expected_error": "The server is not paired or is revoked",
            "reason": "Remote project registration is refused because the referenced server is not paired.",
            "assert_no_write": True,
        }],
    },
    ("POST", "/api/questions/defer"): {
        "primary": {
            "body": {"id": "$question", "deferred": True},
            "expected": "2xx",
        },
    },
    ("POST", "/api/questions/delete"): {
        "primary": {"body": {"id": "$questionDelete"}, "expected": "2xx"},
        # A fabricated pending card without a live retained session is not a
        # meaningful HTTP success probe. The fake provider callback contract is
        # covered at its owner by callback_runtime's durable delete test.
    },
    ("POST", "/api/panel/layout"): {
        "primary": {"body": {
            "agent": "$agent", "client": "desktop", "sequence": 1,
            "renderer": "progress-markdown-v2",
            "revision": "$layoutRevision", "width": 300,
            "height": 100, "contentWidth": 200, "contentHeight": 50,
            "overflowX": 0, "overflowY": 0, "totalLines": 1,
            "visibleLines": 1, "lastVisibleLine": "# HTTP ledger probe",
            "lastVisibleHeading": "# HTTP ledger probe",
            "fits": True, "reason": None,
        }},
    },
    ("POST", "/api/plan"): {
        "primary": {"body": {"agent": "$agent"}},
    },
    ("POST", "/api/queue"): {
        "primary": {"refresh_queue": True, "body": {
            "agent": "$agent", "action": "reorder", "request_id": "ledger-probe-queue",
            "ordered_ids": "$queueIds",
            "expected_revision": "$emptyQueueRevision",
        }},
    },
    ("POST", "/api/rename"): {
        "primary": {"body": {"id": "$agent", "name": "HTTP ledger probe rename", "request_id": "ledger-rename"}, "fixture_mutations": {"agent_name": "HTTP ledger probe rename"}, "restore": ["agent_record"], "expected": "2xx"},
        # Python persists and returns this pending receipt before finishing in the background.
        # Source: codex_rename.py:195-199.
        "additional": [{"name": "provider generated title without a provider session", "body": {"id": "$agent", "name": None, "request_id": "ledger-smart-title"}, "expected": "2xx", "python_citation": "codex_rename.py:195-199"}],
    },
    ("POST", "/api/accounts/claude/add"): {
        "primary": {"body": {"login_id": "88888888-8888-4888-8888-888888888881", "email": "ledger@example.test"}, "replace_body": True, "expected": "400", "assert_no_write": True},
    },
    ("POST", "/api/accounts/claude/login"): {
        "primary": {"body": {"login_id": "88888888-8888-4888-8888-888888888882", "account_key": "$claudeAccount"}, "replace_body": True, "expected": "400", "assert_no_write": True},
    },
    ("POST", "/api/accounts/claude/login/cancel"): {
        "primary": {"body": {"login_id": "88888888-8888-4888-8888-888888888883"}, "replace_body": True, "expected": "400", "assert_no_write": True},
    },
    ("POST", "/api/accounts/claude/login/code"): {
        "primary": {"body": {"login_id": "88888888-8888-4888-8888-888888888884", "code": "123456"}, "replace_body": True, "expected": "400", "assert_no_write": True},
    },
    ("POST", "/api/accounts/login"): {
        "primary": {"body": {"login_id": "88888888-8888-4888-8888-888888888885"}, "replace_body": True, "expected": "2xx", "python_citation": "codex_accounts.py:688-700"},
    },
    ("POST", "/api/accounts/login/cancel"): {
        "primary": {"body": {"login_id": "$codexLogin"}, "replace_body": True, "expected": "2xx", "python_citation": "codex_accounts.py:772-800"},
    },
    ("POST", "/api/accounts/delete"): {
        "primary": {"body": {"account_key": "ledger-unknown-account", "request_id": "ledger-delete"}, "expected": "400", "assert_no_write": True},
    },
    ("POST", "/api/accounts/disconnect"): {
        "primary": {"body": {"account_key": "ledger-unknown-account"}, "expected": "400", "assert_no_write": True},
    },
    ("POST", "/api/accounts/name"): {
        "primary": {"body": {"account_key": "ledger-unknown-account", "label": "Probe account", "request_id": "ledger-name"}, "expected": "400", "assert_no_write": True},
    },
    ("POST", "/api/accounts/reconnect"): {
        "primary": {"body": {"account_key": "ledger-unknown-account"}, "expected": "400", "assert_no_write": True},
    },
    ("POST", "/api/accounts/register"): {
        "primary": {"body": {"home": "$projectPath"}, "expected": "400", "assert_no_write": True},
    },
    ("POST", "/api/accounts/discover"): {"primary": {"body": {}, "expected": "2xx"}},
    # Python routes this operation to Runtime.answer (studio_api/work/router.py:376-379;
    # codex_runtime.py:11215-11254), which handles async questions and schedules
    # their follow-up. This probe answers a dedicated async question so the
    # later defer/delete probes retain their independent pending fixture.
    ("POST", "/api/answer"): {"primary": {"body": {"id": "$answerQuestion", "answers": {"value": {"answers": ["Ledger answer"]}}}, "expected": "2xx"}},
    ("POST", "/api/branch"): {"primary": {"body": {"agent": "$agent", "id": "ledger-branch", "message_id": "ledger-message"}, "expected": "400", "assert_no_write": True}},
    ("POST", "/api/capacity-retry"): {
        "primary": {"body": {"id": "$agent", "retry_id": "ledger-capacity-cancel", "action": "cancel"}, "expected": "2xx"},
        "additional": [{"name": "provider turn retry", "body": {"id": "$agent", "retry_id": "ledger-capacity-retry", "action": "retry"}, "expected": "2xx"}],
    },
    ("POST", "/api/checkpoint/restore"): {"primary": {"body": {"agent": "$agent", "checkpoint": "$checkpoint", "expectedTree": "0000000000000000000000000000000000000000"}, "expected": "409", "assert_no_write": True}},
    ("POST", "/api/claude/profiles"): {"primary": {"body": {"account_key": "ledger-missing-claude-account"}, "replace_body": True, "expected": "400", "assert_no_write": True}},
    ("POST", "/api/claude/session"): {"primary": {"body": {"action": "state", "id": "$agent"}, "replace_body": True, "expected": "400", "assert_no_write": True}},
    ("POST", "/api/action"): {"primary": {"dispatch": False, "consumer_only": True, "reason": "Fake-provider review and compact consumer tests provide the named successful requests; the generic seeded agent has no completed provider thread."}},
    ("POST", "/api/stop"): {"primary": {"body": {"id": "$agent"}, "expected": "2xx"}},
    ("POST", "/api/conversation/delete"): {"primary": {"body": {"id": "$deleteAgent"}, "expected": "2xx"}},
    # Python checks FederationService.enabled() at the start of route() before
    # reading signed headers (codex_federation.py:527-529); the router maps its
    # PermissionError to 403 (studio_api/federation/router.py:200-201).
    ("POST", "/api/federation/v1/message"): {"primary": {"body": {"protocol": 1, "envelope": {"protocol": 1, "senderServer": "ledger-server", "id": "ledger-message", "kind": "chat-message", "room": "ledger-room", "payload": {}, "created": 1, "timestamp": 1, "nonce": "ledger-probe-nonce", "signature": "ledger-signature"}}, "expected": "403", "assert_no_write": True}},
    ("POST", "/api/federation/v1/pair"): {"primary": {"body": {"protocol": 1, "inviteId": "fixture-invite", "token": "fixture-token", "stateId": "fixture-state", "label": "Ledger", "origin": "https://ledger.example", "publicKey": "fixture-key"}, "expected": "403", "assert_no_write": True}},
    ("POST", "/api/federation/v1/pull"): {"primary": {"body": {"protocol": 1, "acks": []}, "expected": "403", "assert_no_write": True}},
    ("POST", "/api/federation/v1/status"): {"primary": {"body": {"protocol": 1}, "expected": "403", "assert_no_write": True}},
    ("POST", "/api/multi-server/v1/auto-pair"): {"primary": {"body": {"protocol": 1, "serverId": "ledger-server", "label": "Ledger", "origin": "https://ledger.example", "publicKey": "ledger-key", "requestId": "ledger-auto-pair"}, "expected": "403", "expected_error": "Auto pairing requires a signed paired-server request", "reason": "Auto-pair requires a signed paired-server request; this probe has no signature headers.", "assert_no_write": True}},
    ("POST", "/api/multi-server/v1/name"): {"primary": {"body": {"requestId": "ledger-name", "label": "Ledger"}, "expected": "403", "expected_error": "Renaming requires a signed paired-server request", "reason": "Remote name updates require a signed paired-server request; this probe has no signature headers.", "assert_no_write": True}},
    ("POST", "/api/multi-server/v1/pair"): {"primary": {"body": {"protocol": 1, "inviteId": "fixture-invite", "token": "fixture-token", "clientId": "fixture-client", "label": "Ledger", "kind": "ui", "publicKey": "fixture-key", "requestId": "ledger-pair"}, "expected": "403", "expected_error": "Pairing requires a signed paired-server request", "reason": "Pairing requires a signed request and peer invitation fixture not present in the child backend.", "assert_no_write": True}},
    ("POST", "/api/organization"): {"primary": {"body": {"id": "$agent", "pinned": True}, "expected": "2xx"}},
    ("POST", "/api/room/delete"): {"primary": {"body": {"id": "$room"}, "expected": "2xx"}},
    ("POST", "/api/servers/orchestration"): {"primary": {"dispatch": False, "consumer_only": True, "reason": "Positive evidence comes from the exact-candidate signed Rust router test named in ROUTER_POSITIVE_TESTS."}},
    ("POST", "/api/terminals/close"): {"primary": {"body": {"id": "$liveTerminal"}, "expected": "2xx"}},
    ("POST", "/api/terminals/create"): {"primary": {"body": {"id": "ledger-terminal", "agent": "$agent"}, "expected": "2xx"}},
    ("POST", "/api/terminals/input"): {"primary": {"body": {"id": "$liveTerminal", "text": "x", "request_id": "ledger-input"}, "expected": "2xx"}},
    ("POST", "/api/terminals/rename"): {"primary": {"body": {"id": "$liveTerminal", "title": "Ledger"}, "expected": "2xx"}},
    ("POST", "/api/terminals/resize"): {"primary": {"body": {"id": "$liveTerminal", "cols": 80, "rows": 24}, "expected": "2xx"}},
    # Python implements native command input for a seeded running command
    # (codex_workspace.py:2183-2217), and Rust now queues the same durable
    # message through the implemented route.
    ("POST", "/api/native-command"): {
        "primary": {
            "body": {"id": "$activeCommand", "action": "input", "text": "ledger input"},
            "fixture_mutations": {"activate_native_command": True},
            "restore": ["active_command_record"],
            "expected": "2xx",
        },
    },
    ("POST", "/api/sync/notify"): {
        "primary": {
            "body": {"requestId": "ledger-sync-notify", "resources": [{"kind": "state"}]},
            "expected": "2xx",
        },
    },
    ("POST", "/api/usage-resume"): {
        "primary": {"body": {"id": "$agent", "resume_id": "ledger-resume-disable", "enabled": False}, "expected": "2xx"},
        "additional": [{"name": "provider turn resume", "body": {"id": "$agent", "resume_id": "ledger-resume-enable", "enabled": True}, "expected": "2xx"}],
    },
    ("POST", "/api/voice/approve"): {"primary": {"body": {"agent": "$agent", "session_id": "$voiceActiveSession", "speech_id": "ledger-speech", "transcript_id": "ledger-transcript"}, "expected": "400", "assert_no_write": True}},
    ("POST", "/api/voice/audio"): {"primary": {"body": {"agent": "$agent", "session_id": "$voiceActiveSession", "chunk_id": "ledger-audio", "audio": "", "mime": "audio/webm"}, "expected": "2xx"}},
    # Python commits the connecting session before its asynchronous provider
    # setup and returns it immediately (codex_native_voice.py:51-77).
    ("POST", "/api/voice/start"): {"primary": {"body": {"agent": "$agent", "session_id": "ledger-voice-start", "sdp": "v=0\r\n"}, "expected": "2xx"}},
    ("POST", "/api/agents"): {
        "primary": {"body": {"id": "66666666-6666-4666-8666-666666666666", "prompt": "Ledger agent", "cwd": "$projectPath"}, "expected": "2xx"},
    },
    ("POST", "/api/assets"): {
        "primary": {"body": {"agent": "$agent", "name": "ledger.txt", "base64": "bGVkZ2Vy"}, "expected": "2xx"},
    },
    ("POST", "/api/checkpoint"): {
        "primary": {"name": "native workspace checkpoint", "body": {"agent": "$agent", "label": "ledger checkpoint alternate"}, "fixture_mutations": {"clear_workspace_monitors": True}, "restore": ["workspace_monitors"], "expected": "2xx"},
        "additional": [{"name": "image workspace checkpoint refusal", "body": {"agent": "$imageAgent", "label": "ledger checkpoint"}, "expected": "400", "expected_error": "Studio Git checkpoints are unavailable for image workspaces", "assert_no_write": True, "reason": "Image workspaces explicitly refuse Studio Git checkpoints without writing state."}],
    },
    ("POST", "/api/checkpoint/preview"): {
        "primary": {"body": {"agent": "$agent", "checkpoint": "$checkpoint"}, "expected": "2xx"},
    },
    ("POST", "/api/configure"): {
        "primary": {"body": {"id": "$agent", "maxAgents": 10}, "fixture_mutations": {"agent_name": "HTTP ledger probe owner"}, "restore": ["agent_record"], "expected": "2xx"},
        "additional": [{"name": "invalid agent mode settings", "body": {"id": "$agent", "concurrency": 0}, "expected": "400", "assert_no_write": True}],
    },
    ("POST", "/api/conversation"): {
        "primary": {"body": {"id": "$agent", "excluded_account_keys": ["other-account"]}, "fixture_mutations": {"agent_name": "HTTP ledger probe owner"}, "restore": ["agent_record"], "expected": "2xx"},
        "additional": [{"name": "invalid conversation mode", "body": {"id": "$agent", "agent_mode": "single"}, "expected": "400", "assert_no_write": True}],
    },
    ("POST", "/api/mac-sync/config"): {"primary": {"body": {"projectId": "ledger-project", "enabled": True}, "replace_body": True, "expected": "400", "assert_no_write": True}},
    ("POST", "/api/mac-sync/resolve"): {"primary": {"body": {"projectId": "ledger-project", "path": "ledger.txt", "choice": "mac"}, "replace_body": True, "expected": "400", "assert_no_write": True}},
    ("POST", "/api/project-secrets/list"): {"primary": {"body": {"projectId": "ledger-project"}, "replace_body": True, "expected": "503", "assert_no_write": True}},
    ("POST", "/api/project-secrets/set"): {"primary": {"body": {"projectId": "ledger-project", "name": "LEDGER_SECRET", "value": "fixture-value", "requestId": "ledger-set-secret"}, "replace_body": True, "expected": "503", "assert_no_write": True}},
    ("POST", "/api/project-secrets/remove"): {"primary": {"body": {"projectId": "ledger-project", "name": "LEDGER_SECRET", "requestId": "ledger-remove-secret"}, "replace_body": True, "expected": "503", "assert_no_write": True}},
    ("POST", "/api/vault/setup/facts"): {"primary": {"body": {"projectId": "ledger-project"}, "replace_body": True, "expected": "400", "assert_no_write": True}},
    ("POST", "/api/vault/setup"): {"primary": {"body": {"projectId": "ledger-project", "requestId": "ledger-vault-setup", "prefix": "ledger", "profile": "fixture", "region": "us-east-1"}, "replace_body": True, "expected": "400", "assert_no_write": True}},
    ("POST", "/api/leads"): {
        "primary": {"body": {"id": "77777777-7777-4777-8777-777777777777", "cwd": "$projectPath", "workspaceMode": "worktree"}, "replace_body": True, "expected": "2xx"},
    },
    ("POST", "/api/voice/approval_speech"): {
        "primary": {"body": {"agent": "$agent", "request_id": "$approvalRequest"}, "replace_body": True, "expected": "2xx"},
    },
    ("POST", "/api/voice/approvals"): {"primary": {"body": {"agent": "$agent"}, "expected": "2xx"}},
    ("POST", "/api/voice/record"): {
        "primary": {"body": {"agent": "$agent", "session_id": "$voiceEndedSession", "event_id": "ledger-event", "kind": "event", "text": "ledger"}, "replace_body": True, "expected": "2xx"},
    },
    ("POST", "/api/tool-requests/cancel"): {
        "primary": {"body": {"agent": "$agent", "request_id": "$toolCancel"}, "expected": "2xx"},
    },
    ("POST", "/api/voice/records"): {"primary": {"body": {"agent": "$agent"}, "expected": "2xx"}},
    ("POST", "/api/voice/status"): {"primary": {"body": {"agent": "$agent"}, "expected": "2xx"}},
    ("POST", "/api/voice/submit"): {
        "primary": {"body": {"agent": "$agent", "message_id": "ledger-message", "record_ids": ["$voiceRecord"]}, "expected": "2xx"},
        "additional": [{"name": "missing transcript records", "body": {"agent": "$agent", "message_id": "ledger-message-missing-records", "record_ids": ["ledger-missing-voice-record"]}, "expected": "400", "assert_no_write": True}],
    },
    ("POST", "/api/voice/end"): {
        "primary": {"body": {"agent": "$agent", "session_id": "$voiceEndedSession"}, "expected": "2xx"},
        "additional": [{"name": "active voice session", "body": {"agent": "$agent", "session_id": "$voiceActiveSession"}, "expected": "2xx"}],
    },
    ("POST", "/api/voice/speech"): {
        "primary": {"body": {"agent": "$agent", "record_id": "$voiceRecord", "session_id": "$voiceEndedSession"}, "expected": "2xx"},
        "additional": [{"name": "active voice speech without a saved receipt", "body": {"agent": "$agent", "record_id": "$voiceRecord", "session_id": "$voiceActiveSession"}, "expected": "400", "assert_no_write": True}],
    },
    ("POST", "/api/rules"): {
        "primary": {"body": {
            "agent": "$agent", "name": "HTTP ledger probe rule", "kind": "interval",
            "command": "echo ledger probe", "text": "Check the probe fixture.",
        }, "expected": "2xx", "python_citation": "codex_rules.py:337-339"},
        "additional": [{
            "name": "active rule check is cancelled on pause",
            "body": {"agent": "$agent", "action": "pause", "id": "$activeRule"},
            "expected": "2xx",
            "python_citation": "codex_rules.py:179-181,236-245",
            "fixture_mutations": {"activate_rule_check": True},
            "restore": ["rule_monitor"],
        }],
    },
    ("POST", "/api/work"): {
        "primary": {"body": {"agent": "$agent", "action": "list"}},
    },
    ("POST", "/api/sync/drafts"): {
        "primary": {"body": {"rows": [{
            "newDocumentState": {
                "id": "ledger-probe-device:ledger-probe-session",
                "payload": "{\"device\":\"ledger-probe-device\",\"session\":\"ledger-probe-session\",\"text\":\"probe draft\"}",
            },
        }]}, "expected": "2xx"},
        "additional": [{
            "name": "draft identity mismatch is refused",
            "body": {"rows": [{
                "newDocumentState": {
                    "id": "wrong-device:ledger-probe-session",
                    "payload": "{\"device\":\"ledger-probe-device\",\"session\":\"ledger-probe-session\",\"text\":\"probe draft\"}",
                },
            }]},
            "expected": "400",
            "assert_no_write": True,
        }],
    },
    ("GET", "/api/sync/pull"): {
        "primary": {"query": {"scope": "drafts"}, "expected": "2xx"},
        "additional": [{
            "name": "entity pull shape",
            "query": {"scope": "state:entities:v1"},
            "expected": "2xx",
        }],
    },
}

UNSAFE_PROBE_EXCLUSIONS = {
    ("POST", "/api/limits/reset"):
        "not dispatched by the generic probe because reset-credit redemption requires an explicit fake-provider binding",
}


def consumer_suites() -> list[Path]:
    return sorted(
        path
        for path in TESTS.glob("*.py")
        if path != HARNESS
        and path.name != Path(__file__).name
        and re.search(r"(?:from\s+http_backend_harness\s+import|import\s+http_backend_harness)",
                      path.read_text(encoding="utf-8"))
    )


def operations() -> list[tuple[str, str]]:
    document = json.loads(SPEC.read_text(encoding="utf-8"))
    methods = {"get", "put", "post", "delete", "options", "head", "patch", "trace"}
    return sorted(
        (method.upper(), path)
        for path, item in document.get("paths", {}).items()
        for method in item
        if method in methods
    )


def appendix_http_assertions(
    known_operations: set[str],
) -> dict[str, set[str]]:
    """Validate named appendix witnesses and return the HTTP witnesses."""
    plan = ROOT / "docs/plans/rust-runtime-work-breakdown.md"
    text = plan.read_text(encoding="utf-8").split("<!-- operation-map -->", 1)[1]
    witnesses: dict[str, set[str]] = defaultdict(set)
    for line in text.splitlines():
        if not line.startswith("| `"):
            continue
        cells = [cell.strip() for cell in line.strip("|").split("|")]
        if len(cells) < 8:
            continue
        operation = cells[0].strip("`")
        witness = cells[7].strip("`")
        if not witness.startswith("T/"):
            continue
        match = re.fullmatch(r"T/([^:]+)::([A-Za-z_][A-Za-z0-9_.]*)", witness)
        if match is None:
            raise ValueError(f"plan witness {witness} must name a test with file::test_name")
        relative, test_name = match.groups()
        source_path = TESTS / relative
        module = ast.parse(source_path.read_text(encoding="utf-8"), filename=str(source_path))
        source_tests: set[str] = set()

        def collect_test_ids(node: ast.AST, ancestors: list[str]) -> None:
            current = ancestors
            if isinstance(node, ast.ClassDef):
                current = [*ancestors, node.name]
            elif isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
                if node.name.startswith("test_"):
                    source_tests.add(".".join([*ancestors, node.name]))
                current = [*ancestors, node.name]
            for child in ast.iter_child_nodes(node):
                collect_test_ids(child, current)

        collect_test_ids(module, [])
        if test_name not in source_tests:
            raise ValueError(f"plan witness {witness} does not resolve to a named test")
        if cells[6] == "H" and operation in known_operations:
            witnesses[operation].add(
                f"workspaces/runtime/apps/server/tests/{relative}::__main__.{test_name}"
            )
    return witnesses


def parse_router_test_evidence(
    metadata: dict[str, Any], junit_bytes: bytes, current_revision: str,
    *, worktree_clean: bool = True,
) -> dict[str, Any]:
    """Accept only the fixed router witnesses from a hashed, exact-revision JUnit report."""
    revision = metadata.get("revision")
    expected_hash = metadata.get("junitSha256")
    observed_hash = hashlib.sha256(junit_bytes).hexdigest()
    entries: list[dict[str, Any]] = []

    def unresolved(reason: str) -> dict[str, Any]:
        return {
            "tier": "Rust HTTP router tests",
            "status": "unverified",
            "revision": revision if isinstance(revision, str) else None,
            "junitSha256": observed_hash,
            "assertions": [
                {"operation": f"{method} {path}", "test": f"{classname}::{name}",
                 "status": "unverified", "reason": reason}
                for (method, path), (classname, name) in ROUTER_POSITIVE_TESTS.items()
            ],
        }

    if not isinstance(revision, str) or revision != current_revision:
        return unresolved("Router test evidence revision does not match the current source revision.")
    if not worktree_clean:
        return unresolved("Router test evidence cannot cover a dirty source worktree.")
    if not isinstance(expected_hash, str) or expected_hash != observed_hash:
        return unresolved("Router test evidence does not match the recorded JUnit report hash.")
    try:
        root = ET.fromstring(junit_bytes)
    except ET.ParseError as error:
        return {
            "status": "failed",
            "revision": revision,
            "junitSha256": observed_hash,
            "assertions": [{
                "operation": f"{method} {path}",
                "test": f"{classname}::{name}",
                "status": "failed",
                "reason": f"Could not parse nextest JUnit XML: {error}",
            } for (method, path), (classname, name) in ROUTER_POSITIVE_TESTS.items()],
        }

    test_cases = list(root.iter("testcase"))
    for (method, path), (classname, name) in ROUTER_POSITIVE_TESTS.items():
        matches = [
            case for case in test_cases
            if case.attrib.get("classname") == classname and case.attrib.get("name") == name
        ]
        assertion = {"operation": f"{method} {path}", "test": f"{classname}::{name}"}
        if len(matches) != 1:
            assertion.update({
                "status": "unverified",
                "reason": "The exact named router test is missing or duplicated in the JUnit report.",
            })
        elif matches[0].find("failure") is not None or matches[0].find("error") is not None:
            assertion.update({"status": "failed", "reason": "The exact named router test failed."})
        elif matches[0].find("skipped") is not None:
            assertion.update({"status": "unverified", "reason": "The exact named router test was skipped."})
        else:
            assertion.update({"status": "passed"})
        entries.append(assertion)
    statuses = {entry["status"] for entry in entries}
    status = "failed" if "failed" in statuses else "pass" if statuses == {"passed"} else "unverified"
    return {
        "tier": "Rust HTTP router tests",
        "status": status,
        "revision": revision,
        "junitSha256": observed_hash,
        "assertions": entries,
    }


def load_router_test_evidence(path: Path | None) -> dict[str, Any]:
    """Load the one explicit sidecar format; absent or stale evidence stays unresolved."""
    if path is None:
        return {
            "tier": "Rust HTTP router tests",
            "status": "unverified",
            "reason": "No --router-evidence sidecar was supplied.",
            "assertions": [
                {"operation": f"{method} {route}", "test": f"{class_name}::{test_name}",
                 "status": "unverified", "reason": "No --router-evidence sidecar was supplied."}
                for (method, route), (class_name, test_name) in ROUTER_POSITIVE_TESTS.items()
            ],
        }
    try:
        metadata = json.loads(path.read_text(encoding="utf-8"))
        if not isinstance(metadata, dict):
            raise ValueError("router evidence sidecar must be a JSON object")
        junit_path_value = metadata.get("junitPath")
        if not isinstance(junit_path_value, str) or not junit_path_value:
            raise ValueError("router evidence sidecar requires junitPath")
        junit_path = Path(junit_path_value)
        if not junit_path.is_absolute():
            junit_path = path.parent / junit_path
        junit_bytes = junit_path.read_bytes()
    except (OSError, ValueError, json.JSONDecodeError) as error:
        return {
            "tier": "Rust HTTP router tests",
            "status": "unverified",
            "reason": f"Router test evidence could not be loaded: {error}",
            "assertions": [
                {"operation": f"{method} {route}", "test": f"{class_name}::{test_name}",
                 "status": "unverified", "reason": "Router evidence input is unavailable."}
                for (method, route), (class_name, test_name) in ROUTER_POSITIVE_TESTS.items()
            ],
        }
    try:
        current_revision = subprocess.run(
            ["git", "-C", str(ROOT), "rev-parse", "HEAD"],
            check=True, capture_output=True, text=True,
        ).stdout.strip()
        worktree_status = subprocess.run(
            ["git", "-C", str(ROOT), "status", "--porcelain", "--untracked-files=all"],
            check=True, capture_output=True, text=True,
        ).stdout
    except (OSError, subprocess.CalledProcessError) as error:
        current_revision = ""
        evidence = parse_router_test_evidence(metadata, junit_bytes, current_revision)
        evidence["status"] = "unverified"
        evidence["reason"] = f"Could not verify current source state: {error}"
        return evidence
    return parse_router_test_evidence(
        metadata, junit_bytes, current_revision, worktree_clean=not worktree_status,
    )


def rust_operation_manifest() -> dict[tuple[str, str], dict[str, Any]]:
    prebuilt_path = os.environ.get(P20_OPERATION_MANIFEST_ENV)
    if prebuilt_path:
        try:
            rows = json.loads(Path(prebuilt_path).read_text(encoding="utf-8"))
            return {(item["method"], item["path"]): item for item in rows}
        except (OSError, KeyError, TypeError, json.JSONDecodeError) as error:
            raise RuntimeError(f"invalid prebuilt Rust route manifest at {prebuilt_path}: {error}") from error
    process = subprocess.run(
        [
            "cargo",
            "test",
            "--locked",
            "--release",
            "-p",
            "studio-runtime-http",
            "--lib",
            "print_operation_manifest",
            "--",
            "--nocapture",
        ],
        cwd=ROOT,
        env=os.environ.copy(),
        text=True,
        capture_output=True,
        check=False,
    )
    if process.returncode:
        raise RuntimeError(
            "could not read the Rust route manifest:\n"
            + (process.stderr + process.stdout)[-4000:]
        )
    marker = "RUST_HTTP_OPERATION_MANIFEST="
    for line in process.stdout.splitlines():
        if line.startswith(marker):
            return {
                (item["method"], item["path"]): item
                for item in json.loads(line[len(marker):])
            }
    raise RuntimeError("Rust route manifest test did not print its operation manifest")


def resolve_schema(schema: dict[str, Any], document: dict[str, Any]) -> dict[str, Any]:
    reference = schema.get("$ref")
    if not reference:
        return schema
    if not reference.startswith("#/components/schemas/"):
        raise ValueError(f"unsupported OpenAPI reference in probe fixture: {reference}")
    value: Any = document
    for part in reference[2:].split("/"):
        value = value[part.replace("~1", "/").replace("~0", "~")]
    return value


def sample_value(schema: dict[str, Any], document: dict[str, Any], active: frozenset[str] = frozenset()) -> Any:
    reference = schema.get("$ref")
    if reference in active:
        return {}
    schema = resolve_schema(schema, document)
    active = active | ({reference} if reference else set())
    for union_key in ("anyOf", "oneOf"):
        if union_key in schema:
            for member in schema[union_key]:
                try:
                    return sample_value(member, document, active)
                except (ValueError, KeyError):
                    continue
    if "const" in schema:
        return schema["const"]
    if "enum" in schema:
        return schema["enum"][0]
    if "default" in schema:
        return schema["default"]
    kind = schema.get("type")
    if kind == "object" or "properties" in schema:
        properties = schema.get("properties", {})
        required = schema.get("required", list(properties))
        return {name: sample_value(properties[name], document, active) for name in required if name in properties}
    if kind == "array":
        minimum = schema.get("minItems", 0)
        return [sample_value(schema.get("items", {}), document, active) for _ in range(minimum)]
    if kind == "string":
        return "x" * schema.get("minLength", 1)
    if kind == "integer":
        return max(1, int(schema.get("minimum", schema.get("exclusiveMinimum", 0) + 1)))
    if kind == "number":
        return max(1.0, float(schema.get("minimum", 0)))
    if kind == "boolean":
        return True
    if kind == "null":
        return None
    return {}


def ensure_operation_probe_cases(
    manifest: dict[tuple[str, str], dict[str, Any]],
) -> None:
    """Keep refusal expectations tied to the current route census."""
    for key, entry in manifest.items():
        status = entry.get("status")
        spec = PROBE_CASES.setdefault(key, {})
        primary = spec.get("primary", {})
        if primary.get("dispatch") is False and primary.get("consumer_only"):
            if not primary.get("reason"):
                raise RuntimeError(f"consumer-only probe {key[0]} {key[1]} needs a reason")
            if status != "implemented":
                primary["unverified"] = True
            continue
        if status == "unimplemented":
            # The manifest state has one exact contract for schema-valid input.
            spec.setdefault("primary", {}).setdefault("expected", "501")
            continue
        if status not in {"implemented", "partial"}:
            continue
        primary = spec.setdefault("primary", {})
        if status == "implemented":
            primary.setdefault("expected", "2xx")
            for probe_case in [primary, *spec.get("additional", [])]:
                if probe_case.get("expected") == "501":
                    # A stale refusal does not describe the expected result after
                    # implementation. Require an explicit success or named
                    # precondition case instead of treating any non-501 as valid.
                    probe_case["expected"] = None
                    probe_case["unverified"] = True
                    probe_case["reason"] = (
                        "previous 501 expectation needs an explicit success or runtime-precondition case"
                    )
                    probe_case.pop("expected_error", None)
        elif not primary.get("expected"):
            primary["unverified"] = True


def account_fixture_namespace(backend: Any) -> str:
    """Match account fixture IDs between the registry and probe inputs."""
    return backend.fixture_namespace if backend.copy_mode else ""


def seed_python_probe_state(backend: Any, python: str) -> dict[str, str]:
    """Seed the disposable backend with records written by the Python runtime."""
    prefix = backend.fixture_namespace or "ledger-"
    agent_id = f"{prefix}probe-agent"
    delete_agent_id = f"{prefix}probe-delete-agent"
    image_agent_id = f"{prefix}probe-image-agent"
    task_id = f"{prefix}probe-task"
    active_command_id = f"{prefix}probe-active-command"
    work_id = f"{prefix}probe-work"
    complaint_id = f"{prefix}probe-complaint"
    room_id = f"{prefix}probe-room"
    checkpoint_id = f"{prefix}probe-checkpoint"
    item_id = f"{prefix}probe-item"
    monitor_id = f"{prefix}probe-monitor"
    terminal_id = f"{prefix}probe-terminal"
    active_rule_id = f"{prefix}probe-active-rule"
    active_monitor_id = f"rule:{active_rule_id}:1"
    chat_id = str(uuid.uuid4()) if backend.copy_mode else "55555555-5555-4555-8555-555555555555"
    message_id = str(uuid.uuid4()) if backend.copy_mode else "66666666-6666-4666-8666-666666666666"
    file_path = f"{prefix}probe.txt"
    source = Path(__file__).resolve().parents[1] / "src"
    seed = f"""
import os, subprocess, sys, time
from pathlib import Path
sys.path.insert(0, {str(source)!r})
sys.path.insert(0, {str(TESTS)!r})
from runtime_test_environment import make_runtime
from codex_runtime import Runtime
from codex_canvas import Canvas

runtime = make_runtime(Runtime, Path(os.environ["CODEX_AGENTS_STATE_DIR"]), environment=dict(os.environ))
try:
    agent = {{
        "id": {agent_id!r}, "rootId": {agent_id!r}, "epoch": 0,
        "status": "idle", "name": "HTTP ledger probe owner", "role": "orchestrator",
        "model": "fixture-model",
        "cwd": os.environ["CODEX_CANVAS_CWD"], "isLead": True,
        "autoWake": True, "approvalPolicy": "never", "worktreeReady": True,
        "accountKey": "default", "threadId": "ledger-probe-thread",
    }}
    image_agent = {{
        **agent, "id": {image_agent_id!r}, "rootId": {agent_id!r},
        "parentId": {agent_id!r}, "name": "HTTP ledger image probe",
        "isLead": False, "autoWake": False, "workspaceMode": "image",
        "imageWorkspace": True, "executionMode": "vm",
    }}
    delete_agent = {{
        **agent, "id": {delete_agent_id!r}, "rootId": {delete_agent_id!r},
        "name": "HTTP ledger delete probe", "threadId": None, "turnId": None,
        "autoWake": False,
    }}
    now = time.time()
    cancel_retry = {{
        "id": "ledger-capacity-cancel", "status": "scheduled", "dueAt": now + 600,
        "threadId": "ledger-probe-thread", "turnId": "ledger-probe-turn", "accountKey": "default",
        "epoch": 0, "cause": "serverOverloaded", "attempt": 1, "maxAttempts": 3,
        "cwd": os.environ["CODEX_CANVAS_CWD"], "settings": {{}}, "taskClaims": [],
    }}
    retry_retry = {{**cancel_retry, "id": "ledger-capacity-retry"}}
    disable_resume = {{
        "id": "ledger-resume-disable", "status": "scheduled", "dueAt": now + 600,
        "threadId": "ledger-probe-thread", "turnId": "ledger-probe-turn", "accountKey": "default",
        "epoch": 0, "cause": "usage_limit", "failedAt": now, "authAttempt": 0,
        "authRefreshMarker": None, "plannedAt": None, "resetAt": None, "reason": None,
    }}
    enable_resume = {{**disable_resume, "id": "ledger-resume-enable"}}
    with runtime.lock, runtime.db() as db:
        runtime.put(db, "agents", agent, sync_rooms=False)
        runtime.put(db, "agents", image_agent, sync_rooms=False)
        runtime.put(db, "agents", delete_agent, sync_rooms=False)
        runtime.put(db, "tool_requests", {{
            "id": "ledger-tool-receipt", "agent": {agent_id!r},
            "tool": "orchestration_peers", "stage": "completed", "outcome": "applied",
            "created": now, "updated": now, "finished": now,
            "result": {{"success": True, "contentItems": []}},
        }})
        runtime.put(db, "tool_requests", {{
            "id": "ledger-tool-cancel", "agent": {agent_id!r},
            "tool": "orchestration_peers", "stage": "queued", "outcome": "pending",
            "created": now, "updated": now,
        }})
        runtime.capacity_save(db, agent, cancel_retry)
        runtime.capacity_save(db, agent, retry_retry)
        runtime.usage_resume_save(db, agent, disable_resume)
        runtime.usage_resume_save(db, agent, enable_resume)
        runtime.put(db, "agents", agent, sync_rooms=False)
        runtime.put(db, "tasks", {{
            "id": {task_id!r}, "agent": {agent_id!r}, "kind": "command",
            "status": "completed",
            "created": 1.0, "finished": 2.0,
        }}, sync_rooms=False)
        runtime.put(db, "tasks", {{
            "id": {active_command_id!r}, "agent": {agent_id!r}, "kind": "command",
            "status": "completed", "processId": "44", "itemId": "ledger-probe-command-item",
            "created": 1.0, "finished": 2.0,
        }}, sync_rooms=False)
        runtime.put(db, "work", {{
            "id": {work_id!r}, "rootId": {agent_id!r}, "owner": {agent_id!r},
            "status": "ready", "title": "HTTP ledger probe work", "description": "",
            "created": 1.0, "updated": 1.0, "createdBy": {agent_id!r},
            "version": 1, "dependencies": [], "decisions": [], "results": [],
        }}, sync_rooms=False)
        runtime.put(db, "plans", {{
            "id": {agent_id!r}, "rootId": {agent_id!r}, "text": "HTTP ledger probe plan",
            "version": 1, "updated": 1.0, "steps": [],
        }}, sync_rooms=False)
        runtime.put(db, "requests", {{
            "id": "ledger-probe-question", "agent": {agent_id!r}, "epoch": 0,
            "method": "agent/asyncQuestion", "status": "pending", "createdAt": 1.0,
            "params": {{"questions": [{{"id": "value", "question": "Value?"}}]}},
        }}, sync_rooms=False)
        runtime.put(db, "requests", {{
            "id": "ledger-probe-answer-question", "agent": {agent_id!r}, "epoch": 0,
            "method": "agent/asyncQuestion", "status": "pending", "createdAt": 1.0,
            "params": {{"questions": [{{"id": "value", "question": "Value?"}}]}},
        }}, sync_rooms=False)
        runtime.put(db, "requests", {{
            "id": "ledger-probe-monitor-approval", "agent": {agent_id!r}, "epoch": 0,
            "method": "monitor/approve", "status": "pending", "createdAt": 1.0,
            "params": {{"monitorId": "ledger-probe-monitor", "command": "echo ledger", "cwd": os.environ["CODEX_CANVAS_CWD"]}},
        }}, sync_rooms=False)
        runtime.put(db, "complaints", {{
            "id": {complaint_id!r}, "leadId": {agent_id!r}, "author": "user",
            "text": "HTTP ledger probe complaint", "version": 1, "status": "open",
            "created": 1.0, "updated": 1.0, "readAt": None,
            "recipient": "lead", "responses": [],
        }}, sync_rooms=False)
        runtime.put(db, "rooms", {{
            "id": {room_id!r}, "kind": "private", "members": [{agent_id!r}],
            "updated": 1.0,
        }}, sync_rooms=False)
        runtime.item(db, {agent_id!r}, {item_id!r}, "assistant", "HTTP ledger probe transcript item")
    workspace = Path(os.environ["CODEX_CANVAS_CWD"])
    subprocess.run(["git", "init", "-q"], cwd=workspace, check=True)
    subprocess.run(["git", "config", "user.name", "HTTP Ledger Fixture"], cwd=workspace, check=True)
    subprocess.run(["git", "config", "user.email", "ledger@example.invalid"], cwd=workspace, check=True)
    (workspace / {file_path!r}).write_text("HTTP ledger probe file\\n", encoding="utf-8")
    subprocess.run(["git", "add", {file_path!r}], cwd=workspace, check=True)
    subprocess.run(["git", "commit", "-qm", "ledger seed"], cwd=workspace, check=True)
    checkpoint = runtime.capture_checkpoint({agent_id!r}, "HTTP ledger probe checkpoint")
    runtime.set_rate_limits("default", {{
        "data": {{"accountId": "fixture-account", "status": "ready", "signedIn": True,
                 "rateLimits": {{"primary": {{"usedPercent": 27}}}}}},
        "at": 1234.5, "readAt": 1234.5, "error": None,
    }})
    from codex_voice import VoiceStore
    voice = VoiceStore(runtime)
    voice._publish_voice = lambda _agent: None
    voice._start_native = lambda _agent, _session, _sdp: None
    voice.start({agent_id!r}, "ledger-probe-voice-ended", "v=0\\r\\n")
    voice._state("ledger-probe-voice-ended", "lost", "fixture ended", ended=True, publish=False)
    voice.record(
        {agent_id!r}, "ledger-probe-voice-ended", "ledger-probe-voice-record",
        "user", "Ledger voice transcript record.", payload={{"utterance_order": 1}}, _publish=False,
    )
    active_voice = VoiceStore(runtime)
    active_voice._publish_voice = lambda _agent: None
    active_voice._start_native = lambda _agent, _session, _sdp: None
    active_voice.start({agent_id!r}, "ledger-probe-voice-active", "v=0\\r\\n")
    active_voice._state("ledger-probe-voice-active", "ready", publish=False)
    active_rule = runtime.rules({{
        "agent": {agent_id!r}, "action": "save", "id": {active_rule_id!r},
        "name": "HTTP ledger active rule", "kind": "interval", "intervalSeconds": 60,
        "at": time.time(), "command": "echo ledger probe", "text": "Check the probe fixture.",
    }})
    class QuietPool:
        def submit(self, *_args, **_kwargs):
            return None
    real_pool = runtime.pool
    runtime.pool = QuietPool()
    try:
        runtime.rules_tick()
    finally:
        runtime.pool = real_pool
    active_rule = next(
        rule for rule in runtime.rules({{"agent": {agent_id!r}, "action": "list"}})["rules"]
        if rule["id"] == {active_rule_id!r}
    )
    active_monitor = runtime.monitor(
        {agent_id!r}, {{"command": "echo ledger probe", "stall_timeout_seconds": 0}},
        key={active_monitor_id!r}, rule=active_rule,
    )
    monitor = runtime.monitor(
        {agent_id!r}, {{"command": "echo ledger probe", "stall_timeout_seconds": 0}},
        key={monitor_id!r},
    )
    for row in (active_monitor, monitor):
        log = Path(row["log"])
        log.parent.mkdir(parents=True, exist_ok=True)
        log.write_bytes(b"probe log!\\n")
    with runtime.db() as db:
        db.execute(
            "INSERT INTO voice_speech_requests(id,session,record_id,state,error) VALUES(?,?,?,?,?)",
            ("ledger-probe-voice-ended:ledger-probe-voice-record", "ledger-probe-voice-ended", "ledger-probe-voice-record", "failed", "saved failure"),
        )
    file = Path(os.environ["CODEX_CANVAS_CWD"]) / {file_path!r}
    file.write_text("HTTP ledger probe file\\n", encoding="utf-8")
    monitor_log = Path(os.environ["CODEX_AGENTS_STATE_DIR"]) / "monitor-logs" / "ledger.log"
    monitor_log.parent.mkdir(parents=True, exist_ok=True)
    monitor_log.write_bytes(b"probe log!\\n")
    progress = Path(os.environ["CODEX_AGENTS_STATE_DIR"]) / "progress" / {agent_id!r} / "PROGRESS.md"
    progress.parent.mkdir(parents=True, exist_ok=True)
    progress.write_text("# HTTP ledger probe\\n", encoding="utf-8")
    canvas = Canvas(runtime.root)
    canvas.runtime = runtime
    canvas.create_chat("HTTP ledger probe chat", [{agent_id!r}], {chat_id!r})
    from codex_terminals import TerminalManager
    manager = TerminalManager(Path(os.environ["CODEX_AGENTS_STATE_DIR"]))
    try:
        terminal = manager.create(runtime, {{
            "id": "ledger-probe-terminal-request", "agent": {agent_id!r},
            "cols": 100, "rows": 28,
        }})
        terminal_id = terminal["id"]
        manager.append(terminal_id, "HTTP ledger probe terminal output")
        queue_revision = runtime.queue_action({agent_id!r})["revision"]
        Path(os.environ["CODEX_AGENTS_STATE_DIR"], "ledger-probe-identifiers.json").write_text(
            __import__("json").dumps({{
                "terminal": terminal_id, "emptyQueueRevision": queue_revision,
            "checkpoint": checkpoint["id"], "monitor": monitor["id"],
            "activeMonitor": active_monitor["id"],
            }}), encoding="utf-8"
        )
    finally:
        manager.close()
finally:
    runtime.close()
"""
    seed = seed.replace("ledger-", prefix).replace('"default"', repr(backend.account_key))
    environment = runtime_environment(backend)
    environment["CODEX_AGENTS_PYTHON"] = python
    subprocess.run(
        [python, "-B", "-c", seed],
        cwd=backend.cwd,
        env=environment,
        check=True,
        timeout=30,
    )
    progress_metadata = (
        backend.state / "progress" / agent_id / "PROGRESS.md"
    ).stat()
    layout_revision = (
        f"{progress_metadata.st_dev}:{progress_metadata.st_ino}:"
        f"{progress_metadata.st_mtime_ns}:{progress_metadata.st_ctime_ns}:"
        f"{progress_metadata.st_size}"
    )
    terminal_identifiers = json.loads(
        (backend.state / f"{prefix}probe-identifiers.json").read_text(encoding="utf-8")
    )
    terminal_id = terminal_identifiers["terminal"]
    return {
        "agent": agent_id,
        "deleteAgent": delete_agent_id,
        "imageAgent": image_agent_id,
        "task": task_id,
        "activeCommand": active_command_id,
        "complaint": complaint_id,
        "room": room_id,
        "checkpoint": terminal_identifiers["checkpoint"],
        "item": f"{agent_id}:{item_id}",
        "monitor": terminal_identifiers["monitor"],
        "activeMonitor": terminal_identifiers["activeMonitor"],
        "terminal": terminal_id,
        "queueIds": [],
        "activeRule": active_rule_id,
        "question": f"{prefix}probe-question",
        "questionDelete": f"{prefix}probe-question",
        "answerQuestion": f"{prefix}probe-answer-question",
        "voiceRecord": f"{prefix}probe-voice-record",
        "voiceEndedSession": f"{prefix}probe-voice-ended",
        "voiceActiveSession": f"{prefix}probe-voice-active",
        "approvalRequest": f"{prefix}probe-monitor-approval",
        "schemaHash": backend.schema_hash,
        "analyticsAgent": analytics_probe_agent_identifier(backend),
        "chat": chat_id,
        "message": message_id,
        "filePath": file_path,
        "layoutRevision": layout_revision,
        "emptyQueueRevision": terminal_identifiers["emptyQueueRevision"],
        "projectPath": str(backend.cwd),
        "claudeAccount": f"{prefix}claude-ledger",
        "disconnectedAccount": f"{account_fixture_namespace(backend)}disconnected",
        "toolReceipt": f"{prefix}tool-receipt",
        "toolCancel": f"{prefix}tool-cancel",
    }


def analytics_probe_agent_identifier(backend: Any) -> str:
    """Return the analytics seed ID, isolated from copied user data."""
    prefix = backend.fixture_namespace if backend.copy_mode else ""
    return f"{prefix}analytics-contract-agent"


def session_cost_cache_path(state: Path, root_id: str) -> Path:
    """Resolve the cache path through the session-cost reader's path owner."""
    from codex_session_costs import SessionCostReader

    reader = SessionCostReader(state / "analytics.sqlite3", None, state_root=state)
    return reader._cache_path(root_id)


def valid_probe(
    method: str,
    template: str,
    identifiers: dict[str, str] | None = None,
    probe_case: dict[str, Any] | None = None,
) -> tuple[str, Any | None, dict[str, str]]:
    document = json.loads(SPEC.read_text(encoding="utf-8"))
    operation = document["paths"][template][method.lower()]
    overrides = probe_case or {}
    query_overrides = overrides.get("query", {})
    header_overrides = overrides.get("header", {})
    path_values: dict[str, Any] = {}
    query_values: list[tuple[str, str]] = []
    for parameter in operation.get("parameters", []):
        schema = resolve_schema(parameter.get("schema", {}), document)
        value = sample_value(schema, document)
        name = parameter["name"]
        if parameter.get("in") == "query" and name in overrides.get("query_omit", []):
            continue
        if parameter.get("in") in {"path", "query"} and name in query_overrides:
            value = resolve_probe_value(query_overrides[name], identifiers)
        if parameter.get("in") == "path":
            path_values[name] = value
        elif parameter.get("in") == "query":
            query_values.append((name, str(value).lower() if isinstance(value, bool) else str(value)))
    path = re.sub(r"\{([^}:]+)(?::[^}]+)?\}", lambda match: urllib.parse.quote(str(path_values.get(match.group(1), "probe")), safe=""), template)
    if query_values:
        path += "?" + urllib.parse.urlencode(query_values)
    body = None
    body_schema = operation.get("requestBody", {}).get("content", {}).get("application/json", {}).get("schema")
    if body_schema:
        body_overrides = resolve_probe_value(overrides.get("body", {}), identifiers)
        body = body_overrides if overrides.get("replace_body") else sample_value(body_schema, document)
        if not overrides.get("replace_body") and isinstance(body, dict) and isinstance(body_overrides, dict):
            body.update(body_overrides)
        if isinstance(body, dict):
            for name in overrides.get("body_omit", []):
                body.pop(name, None)
    headers = {
        name: str(resolve_probe_value(value, identifiers))
        for name, value in header_overrides.items()
    }
    if identifiers is not None:
        path = namespace_probe_constants(path, identifiers)
        body = namespace_probe_constants(body, identifiers)
        headers = namespace_probe_constants(headers, identifiers)
    return path, body, headers


def resolve_probe_value(value: Any, identifiers: dict[str, str] | None) -> Any:
    if isinstance(value, str) and value.startswith("$"):
        if identifiers is None:
            raise ValueError("probe fixture values require seeded identifiers")
        return identifiers[value[1:]]
    if isinstance(value, list):
        return [resolve_probe_value(item, identifiers) for item in value]
    if isinstance(value, dict):
        return {key: resolve_probe_value(item, identifiers) for key, item in value.items()}
    return value


def namespace_probe_constants(value: Any, identifiers: dict[str, str]) -> Any:
    """Keep static request literals unique when probes target a real-data copy."""
    if "_fixturePrefix" not in identifiers:
        return value
    prefix = identifiers["_fixturePrefix"]
    account_key = identifiers["_accountKey"]
    literal_ids = identifiers["_literalIds"]
    if isinstance(value, str):
        if value == "default":
            return account_key
        if value in literal_ids:
            return literal_ids[value]
        if "ledger-" in value:
            return value.replace("ledger-", prefix)
        return value
    if isinstance(value, list):
        return [namespace_probe_constants(item, identifiers) for item in value]
    if isinstance(value, dict):
        return {key: namespace_probe_constants(item, identifiers) for key, item in value.items()}
    return value


@contextmanager
def probe_fixture_state(
    backend: Any,
    identifiers: dict[str, str],
    probe_case: dict[str, Any],
) -> Iterator[None]:
    """Apply a keyed probe's shared-state mutation and always restore it."""
    mutations = probe_case.get("fixture_mutations", {})
    restore = set(probe_case.get("restore", []))
    if (bool({"agent_execution_mode", "agent_name"} & mutations.keys())) != ("agent_record" in restore):
        raise ValueError("agent record probe mutations must declare agent_record restoration")
    if ("remove_identity" in mutations) != ("identity_file" in restore):
        raise ValueError("identity file probe mutations must declare identity_file restoration")
    if ("stale_session_cost_cache" in mutations) != ("session_cost_cache" in restore):
        raise ValueError("session cost cache mutations must declare cache restoration")
    if ("seed_pricing_catalog" in mutations) != ("pricing_file" in restore):
        raise ValueError("pricing catalog mutations must declare pricing-file restoration")
    if ("clear_workspace_monitors" in mutations) != ("workspace_monitors" in restore):
        raise ValueError("workspace monitor mutations must declare restoration")
    if ("activate_rule_check" in mutations) != ("rule_monitor" in restore):
        raise ValueError("active rule-check mutations must declare restoration")
    if ("activate_native_command" in mutations) != ("active_command_record" in restore):
        raise ValueError("active command mutations must declare task record restoration")

    agent_record: dict[str, Any] | None = None
    identity_file: tuple[Path, bytes, int] | None = None
    session_cost_cache: tuple[Path, bytes] | None = None
    pricing_file: tuple[Path, bytes | None, int | None, bool] | None = None
    workspace_monitors: dict[str, str] | None = None
    rule_monitor: tuple[str, str] | None = None
    active_command_record: tuple[str, str] | None = None
    try:
        if {"agent_execution_mode", "agent_name"} & mutations.keys():
            with sqlite3.connect(backend.state / "canvas.sqlite3") as connection:
                row = connection.execute(
                    "SELECT record FROM runtime_agents WHERE id=?", (identifiers["agent"],),
                ).fetchone()
                if row is None:
                    raise RuntimeError("seeded ledger agent is missing")
                agent_record = json.loads(row[0])
                changed = dict(agent_record)
                if "agent_execution_mode" in mutations:
                    changed["executionMode"] = mutations["agent_execution_mode"]
                if "agent_name" in mutations:
                    changed["name"] = mutations["agent_name"]
                connection.execute(
                    "UPDATE runtime_agents SET record=? WHERE id=?",
                    (json.dumps(changed), identifiers["agent"]),
                )
        if "remove_identity" in mutations:
            path = backend.state / "multi-server" / "identity.json"
            if not path.is_file():
                raise RuntimeError("seeded synthetic federation identity is missing")
            identity_file = (path, path.read_bytes(), path.stat().st_mode & 0o777)
            path.unlink()
        if mutations.get("activate_native_command"):
            command_id = identifiers["activeCommand"]
            with sqlite3.connect(backend.state / "canvas.sqlite3") as connection:
                row = connection.execute(
                    "SELECT record FROM runtime_tasks WHERE id=?", (command_id,),
                ).fetchone()
                if row is None:
                    raise RuntimeError("seeded native command task is missing")
                active_command_record = (command_id, row[0])
                changed = json.loads(row[0])
                changed["status"] = "running"
                changed.pop("finished", None)
                connection.execute(
                    "UPDATE runtime_tasks SET record=? WHERE id=?",
                    (json.dumps(changed), command_id),
                )
        if mutations.get("stale_session_cost_cache"):
            path = session_cost_cache_path(
                backend.state, identifiers["analyticsAgent"],
            )
            if not path.is_file() or path.is_symlink():
                raise RuntimeError("seeded session-cost cache is missing")
            session_cost_cache = (path, path.read_bytes())
            cache = json.loads(session_cost_cache[1])
            cache["source"]["usage"]["generation"] += 1
            path.write_text(json.dumps(cache), encoding="utf-8")
        if mutations.get("seed_pricing_catalog"):
            path = backend.state / "model-pricing" / "models-dev-v1.json"
            directory_existed = path.parent.is_dir()
            if path.is_symlink() or (path.exists() and not path.is_file()):
                raise RuntimeError("model-pricing fixture path is not a regular file")
            pricing_file = (
                path,
                path.read_bytes() if path.is_file() else None,
                path.stat().st_mode & 0o777 if path.is_file() else None,
                directory_existed,
            )
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(json.dumps({
                "version": 1,
                "catalog": {"providers": {
                    "openai": {"models": {"gpt-test": {"cost": {"input": 1, "output": 1}}}},
                    "anthropic": {"models": {"claude-sonnet-5-5": {"cost": {"input": 1, "output": 1}}}},
                }},
            }), encoding="utf-8")
        if mutations.get("clear_workspace_monitors") or mutations.get("activate_rule_check"):
            with sqlite3.connect(backend.state / "canvas.sqlite3") as connection:
                if mutations.get("clear_workspace_monitors"):
                    workspace_monitors = {
                        monitor_id: raw
                        for monitor_id, raw in connection.execute(
                            "SELECT id,record FROM runtime_monitors WHERE "
                            "id LIKE ? AND "
                            "json_extract(record,'$.status') IN ('running','starting','approval')",
                            (f"{backend.fixture_namespace}%" if backend.copy_mode else "%",),
                        )
                    }
                    for monitor_id, raw in workspace_monitors.items():
                        changed = json.loads(raw)
                        changed["status"] = "completed"
                        connection.execute(
                            "UPDATE runtime_monitors SET record=? WHERE id=?",
                            (json.dumps(changed), monitor_id),
                        )
                if mutations.get("activate_rule_check"):
                    monitor_id = identifiers["activeMonitor"]
                    row = connection.execute(
                        "SELECT record FROM runtime_monitors WHERE id=?", (monitor_id,)
                    ).fetchone()
                    if row is None:
                        raise RuntimeError("seeded rule-check monitor is missing")
                    rule_monitor = (monitor_id, row[0])
                    changed = json.loads(row[0])
                    changed["status"] = "approval"
                    changed["ruleId"] = identifiers["activeRule"]
                    connection.execute(
                        "UPDATE runtime_monitors SET record=? WHERE id=?",
                        (json.dumps(changed), monitor_id),
                    )
        yield
    finally:
        try:
            if agent_record is not None:
                with sqlite3.connect(backend.state / "canvas.sqlite3") as connection:
                    connection.execute(
                        "UPDATE runtime_agents SET record=? WHERE id=?",
                        (json.dumps(agent_record), identifiers["agent"]),
                    )
        finally:
            try:
                if identity_file is not None:
                    path, content, mode = identity_file
                    path.write_bytes(content)
                    path.chmod(mode)
            finally:
                if session_cost_cache is not None:
                    path, content = session_cost_cache
                    path.write_bytes(content)
                if pricing_file is not None:
                    path, content, mode, directory_existed = pricing_file
                    if content is None:
                        path.unlink(missing_ok=True)
                    else:
                        path.write_bytes(content)
                        assert mode is not None
                        path.chmod(mode)
                    if not directory_existed:
                        try:
                            path.parent.rmdir()
                        except OSError:
                            pass
                if (
                    workspace_monitors is not None
                    or rule_monitor is not None
                    or active_command_record is not None
                ):
                    with sqlite3.connect(backend.state / "canvas.sqlite3") as connection:
                        if workspace_monitors is not None:
                            for monitor_id, raw in workspace_monitors.items():
                                connection.execute(
                                    "UPDATE runtime_monitors SET record=? WHERE id=?",
                                    (raw, monitor_id),
                                )
                        if rule_monitor is not None:
                            monitor_id, raw = rule_monitor
                            connection.execute(
                                "UPDATE runtime_monitors SET record=? WHERE id=?",
                                (raw, monitor_id),
                            )
                        if active_command_record is not None:
                            command_id, raw = active_command_record
                            connection.execute(
                                "UPDATE runtime_tasks SET record=? WHERE id=?",
                                (raw, command_id),
                            )


def probe_operations(
    manifest: dict[tuple[str, str], dict[str, Any]],
    planned_assertions: dict[str, set[str]],
    successful_assertions: dict[str, set[str]],
    *,
    python: str,
) -> list[dict[str, Any]]:
    sys.path.insert(0, str(TESTS))
    from http_backend_harness import BackendProcess, HttpResult

    with tempfile.TemporaryDirectory(
        prefix="rust-http-ledger-"
    ) as scratch_name:
        scratch = Path(scratch_name).resolve(strict=True)
        backend = BackendProcess(scratch, claude_bin=None)
        try:
            backend.copy_mode = False
            backend.fixture_namespace = ""
            backend.account_key = "default"
            backend.fixture_uuid_values = {}
            backend.fixture_login_id = "88888888-8888-4888-8888-888888888880"
            backend.create_codex_replay("restart-mid-turn-supervisor-replay.json")
            seed_catalog_state(backend, python)
            codex_login_id = seed_codex_login(backend, python)
            import importlib.util
            analytics_path = TESTS / "rust-analytics-http-contract.py"
            analytics_spec = importlib.util.spec_from_file_location("ledger_analytics_seed", analytics_path)
            if analytics_spec is None or analytics_spec.loader is None:
                raise RuntimeError("could not load analytics contract seed")
            analytics_module = importlib.util.module_from_spec(analytics_spec)
            analytics_spec.loader.exec_module(analytics_module)
            analytics_module.seed_analytics(backend)
            identifiers = seed_python_probe_state(backend, python)
            identifiers["codexLogin"] = codex_login_id
            backend.start()
            token = backend.session["token"]
            rows = []
            for method, template in operations():
                operation_manifest = manifest.get((method, template))
                implementation = operation_manifest["status"] if operation_manifest else "unimplemented"
                dependencies = (
                    operation_manifest.get("dependencies", [])
                    if operation_manifest
                    else []
                )
                operation_key = f"{method} {template}"
                exclusion = unsafe_probe_exclusion(method, template)
                if exclusion is not None:
                    planned = sorted(planned_assertions.get(operation_key, set()))
                    observed = sorted(successful_assertions.get(operation_key, set()))
                    rows.append({
                        "method": method,
                        "path": template,
                        "status": None,
                        "implementation": implementation,
                        "observedStatus": None,
                        "classification": "unverified",
                        "covered": bool(observed),
                        "expectedStatus": "unverified",
                        "dependencies": dependencies,
                        "schemaHeader": None,
                        "schemaHeaderMatches": None,
                        "standard501Envelope": None,
                        "dependencyResponseMatches": None,
                        "probeCases": [{
                            "case": "primary",
                            "status": None,
                            "expected": None,
                            "expectedMatches": False,
                            "dispatched": False,
                            "body": None,
                            "elapsedSeconds": 0.0,
                            "standard501Envelope": False,
                            "reason": exclusion,
                        }],
                        "httpAssertions": planned,
                        "observedSuccessfulAssertions": observed,
                    })
                    continue
                probe_spec = PROBE_CASES.get((method, template), {})
                primary_case = probe_spec.get("primary", {})
                cases = [("primary", primary_case)] + [
                    (case.get("name", f"additional-{index}"), case)
                    for index, case in enumerate(probe_spec.get("additional", []), start=1)
                ]
                observations = []
                for case_name, probe_case in cases:
                    if probe_case.get("dispatch") is False:
                        if not probe_case.get("consumer_only") or not probe_case.get("reason"):
                            raise RuntimeError(
                                f"non-dispatched probe {method} {template}/{case_name} "
                                "must be an explicitly named consumer-only case with a reason"
                            )
                        observations.append({
                            "case": case_name,
                            "status": None,
                            "expected": None,
                            "expectedMatches": False,
                            "dispatched": False,
                            "body": None,
                            "elapsedSeconds": 0.0,
                            "standard501Envelope": False,
                            "noWrite": True,
                            "consumerOnly": True,
                            "reason": probe_case["reason"],
                        })
                        continue
                    if probe_case.get("refresh_queue"):
                        queue_snapshot = backend.request(
                            "GET",
                            f"/api/queue?agent={urllib.parse.quote(identifiers['agent'], safe='')}",
                            headers={
                                "X-Canvas-Token": token,
                                "X-Studio-API-Schema": backend.schema_hash,
                            },
                            timeout=2.0,
                        )
                        if queue_snapshot.status != 200:
                            raise RuntimeError(
                                "could not read seeded queue for conformance probe: "
                                f"{queue_snapshot.status}"
                            )
                        queue_value = json.loads(queue_snapshot.body)
                        identifiers["emptyQueueRevision"] = queue_value["revision"]
                        identifiers["queueIds"] = [item["id"] for item in queue_value["items"]]
                    if (
                        os.environ.get("STUDIO_HTTP_BACKEND_COMMAND")
                        and method == "POST"
                        and template in {
                            "/api/terminals/close",
                            "/api/terminals/input",
                            "/api/terminals/rename",
                            "/api/terminals/resize",
                        }
                    ):
                        live_terminal = backend.request(
                            "POST",
                            "/api/terminals/create",
                            body={
                                "id": f"ledger-live-{template.rsplit('/', 1)[-1]}",
                                "agent": identifiers["agent"],
                            },
                            headers={
                                "X-Canvas-Token": token,
                                "X-Canvas-Workspace": backend.identity["workspaceId"],
                                "X-Studio-API-Schema": backend.schema_hash,
                            },
                        )
                        if live_terminal.status != 200:
                            raise RuntimeError(
                                "could not create terminal for live-action ledger cases: "
                                f"{live_terminal.status} {live_terminal.body[:1000]!r}"
                            )
                        identifiers["liveTerminal"] = live_terminal.json()["id"]
                    path, body, header_overrides = valid_probe(
                        method, template, identifiers, probe_case
                    )
                    headers = {
                        "X-Canvas-Token": token,
                        "X-Studio-API-Schema": backend.schema_hash,
                        **header_overrides,
                    }
                    if method in JSON_METHODS or method == "DELETE":
                        headers["X-Canvas-Workspace"] = backend.identity["workspaceId"]
                    file_paths = probe_case.get("assert_no_file_write", [])
                    files_before = {
                        relative: (backend.state / relative).read_bytes()
                        if (backend.state / relative).is_file() else None
                        for relative in file_paths
                    }
                    request_started = time.monotonic()
                    with probe_fixture_state(backend, identifiers, probe_case), (
                        sqlite_snapshot(backend.state / "canvas.sqlite3")
                        if probe_case.get("assert_no_write") else nullcontext(None)
                    ) as database_before:
                        if method == "GET" and template == "/api/sync/stream":
                            request = urllib.request.Request(backend.origin + path, headers=headers)
                            try:
                                with urllib.request.urlopen(request, timeout=2.0) as response:
                                    chunk = response.read(1)
                                    result = HttpResult(
                                        response.status,
                                        {key.lower(): value for key, value in response.headers.items()},
                                        chunk,
                                    )
                            except HTTPError as error:
                                with error:
                                    result = HttpResult(
                                        error.code,
                                        {key.lower(): value for key, value in error.headers.items()},
                                        error.read(),
                                    )
                        else:
                            result = backend.request(
                                method,
                                path,
                                body=body if method in JSON_METHODS else None,
                                headers=headers,
                                timeout=2.0,
                            )
                        try:
                            response_body = json.loads(result.body)
                        except (ValueError, TypeError):
                            response_body = None
                        if probe_case.get("wait_for_success") and not 200 <= result.status < 300:
                            deadline = time.monotonic() + 5
                            while time.monotonic() < deadline and not 200 <= result.status < 300:
                                time.sleep(0.05)
                                result = backend.request(method, path, headers=headers, timeout=2.0)
                                try:
                                    response_body = json.loads(result.body)
                                except (ValueError, TypeError):
                                    response_body = None
                        database_unchanged = (
                            database_before is None or database_before.unchanged()
                        )
                    operation_key = f"{method} {template}"
                    probe_event = {
                        "operation": operation_key,
                        "body": response_body,
                        "expected_error": probe_case.get("expected_error"),
                    }
                    standard_501 = result.status != 501 or valid_501_envelope(probe_event, manifest)
                    no_write = database_unchanged
                    no_write = no_write and all(
                        ((backend.state / relative).read_bytes()
                         if (backend.state / relative).is_file() else None) == original
                        for relative, original in files_before.items()
                    )
                    expected = probe_case.get("expected")
                    expected_matches = probe_status_matches(
                        expected,
                        result.status,
                        standard_501=standard_501,
                        response_body=response_body,
                        expected_error=probe_case.get("expected_error"),
                    )
                    observations.append({
                        "case": case_name,
                        "status": result.status,
                        "expected": expected,
                        "expectedMatches": expected_matches,
                        "expectedError": probe_case.get("expected_error"),
                        "body": persisted_probe_body(
                            probe_case, response_body, copy_mode=backend.copy_mode
                        ),
                        "elapsedSeconds": round(time.monotonic() - request_started, 4),
                        "standard501Envelope": standard_501,
                        "noWrite": no_write,
                        **({"reason": probe_case["reason"]} if "reason" in probe_case else {}),
                    })
                primary = observations[0]
                status = primary["status"]
                operation_key = f"{method} {template}"
                dispatched_observations = [
                    observation for observation in observations
                    if observation.get("dispatched", True)
                ]
                if dispatched_observations:
                    # The final response may belong to an additional dispatched
                    # case; the schema header is invariant across an operation.
                    schema_header = result.headers.get("x-studio-api-schema")
                    schema_header_matches = schema_header == backend.schema_hash
                    standard_501_envelope = primary["standard501Envelope"]
                    dependency_response_matches = (
                        implementation != "partial" or status != 501 or standard_501_envelope
                    )
                    classification = classify_observation(
                        status,
                        implementation,
                        schema_header_matches,
                        standard_501_envelope,
                        dependency_response_matches,
                    )
                    if primary.get("expected") == "400" and primary.get("expectedMatches"):
                        classification = "implemented"
                    if primary.get("expected") == "404" and primary.get("expectedMatches") and primary.get("reason"):
                        classification = "implemented"
                    classification = apply_probe_expectations(
                        classification, cases, observations
                    )
                    if not all(observation["noWrite"] for observation in dispatched_observations):
                        classification = "unexpected"
                else:
                    status = None
                    schema_header = None
                    schema_header_matches = False
                    standard_501_envelope = False
                    dependency_response_matches = False
                    classification = "unverified"
                rows.append({
                    "method": method,
                    "path": template,
                    "status": status,
                    "implementation": implementation,
                    "observedStatus": status,
                    "classification": classification,
                    "covered": bool(successful_assertions.get(operation_key)),
                    "expectedStatus": (
                        "unverified"
                        if any(
                            case.get("dispatch") is False
                            or case.get("unverified") or not case.get("expected")
                            for _, case in cases
                        )
                        else cases[0][1]["expected"]
                    ),
                    "dependencies": dependencies,
                    "schemaHeader": schema_header,
                    "schemaHeaderMatches": schema_header_matches,
                    "standard501Envelope": standard_501_envelope,
                    "dependencyResponseMatches": dependency_response_matches,
                    "probeCases": observations,
                    "httpAssertions": sorted(planned_assertions.get(operation_key, set())),
                    "observedSuccessfulAssertions": sorted(successful_assertions.get(operation_key, set())),
                })
            return rows
        finally:
            backend.close()


def collect_probe_strings(value: Any) -> set[str]:
    if isinstance(value, str):
        return {value}
    if isinstance(value, list):
        return set().union(*(collect_probe_strings(item) for item in value)) if value else set()
    if isinstance(value, dict):
        return set().union(*(collect_probe_strings(item) for item in value.values())) if value else set()
    return set()


def is_named_fault_5xx(case: dict[str, Any]) -> bool:
    if (
        not isinstance(case.get("status"), int)
        or case["status"] < 500
        or case.get("expectedError") is None
    ):
        return False
    purpose = f"{case.get('case', '')} {case.get('reason', '')}".lower()
    return any(term in purpose for term in (
        "malformed", "invalid stored record", "corrupt stored record",
    ))


def api_fixture_evidence(
    operations_result: list[dict[str, Any]],
    router_evidence: dict[str, Any] | None = None,
) -> dict[str, Any]:
    """Summarize API fixtures and the two named Rust router-only positives."""
    statuses: list[str] = []
    positive_operations = 0
    router_positive_operations = 0
    validated_negative_cases = 0
    implemented_operations = 0
    router_positive = {
        assertion["operation"]: assertion["test"]
        for assertion in (router_evidence or {}).get("assertions", [])
        if assertion.get("status") == "passed"
    }
    for operation in operations_result:
        implementation = operation.get("implementation")
        if implementation == "implemented":
            implemented_operations += 1
        cases = operation.get("probeCases", [])
        positive_case = any(
            case.get("expected") == "2xx"
            and case.get("expectedMatches")
            and isinstance(case.get("status"), int)
            and 200 <= case["status"] < 300
            for case in cases
        )
        # `covered` is retained for report compatibility; gate only on named
        # passing consumer assertions that observed a 2xx response.
        positive_assertion = bool(operation.get("observedSuccessfulAssertions"))
        router_assertion = router_positive.get(f"{operation.get('method')} {operation.get('path')}")
        operation["routerPositiveAssertions"] = [router_assertion] if router_assertion else []
        if router_assertion:
            router_positive_operations += 1
        if positive_case or positive_assertion or router_assertion:
            positive_operations += 1
        validated_negative_cases += sum(
            case.get("expected") not in (None, "2xx")
            and case.get("expectedMatches") is True
            and (
                not isinstance(case.get("status"), int)
                or case["status"] < 500
                or is_named_fault_5xx(case)
            )
            for case in cases
        )

        unexpected = operation.get("classification") == "unexpected" or any(
            case.get("expected") is not None
            and case.get("expectedMatches") is False
            for case in cases
        )
        unresolved = any(
            (
                case.get("dispatched", True)
                or not case.get("consumerOnly")
                or not case.get("reason")
            )
            and (
                case.get("expected") is None
                or case.get("unverified")
                or isinstance(case.get("status"), int)
                and case["status"] >= 500
                and not is_named_fault_5xx(case)
            )
            for case in cases
        )
        if unexpected:
            statuses.append("unexpected")
        elif (
            implementation == "implemented"
            and (positive_case or positive_assertion or router_assertion)
            and not unresolved
        ):
            statuses.append("pass")
        else:
            statuses.append("unverified")

    unexpected_operations = statuses.count("unexpected")
    unverified_operations = statuses.count("unverified")
    if router_evidence and router_evidence.get("status") == "failed":
        status = "failed"
    elif not operations_result:
        status = "unverified"
    elif unexpected_operations:
        status = "failed"
    elif unverified_operations:
        status = "unverified"
    else:
        status = "pass"
    return {
        "source": "disposable-fixture-records-and-named-router-tests",
        "status": status,
        "operations": len(operations_result),
        "implementedOperations": implemented_operations,
        "positiveOperations": positive_operations,
        "routerPositiveOperations": router_positive_operations,
        "unverifiedOperations": unverified_operations,
        "unexpectedOperations": unexpected_operations,
        "validatedNegativeCases": validated_negative_cases,
    }


def copied_data_evidence_status(evidence: dict[str, Any]) -> str:
    """Require old-row preservation, a finder no-write proof, and rollback."""
    required = (
        "stateCheckBefore",
        "existingRowRead",
        "preexistingRowsPreservedFromBeforeStartup",
        "unchangedDuringFinderRead",
        "schemaChangesReviewed",
        "derivedIndexesReviewed",
        "stateCheckAfter",
        "baselineRestore",
        "pythonRollbackReadback",
    )
    if any(evidence.get(key) is False for key in required):
        return "failed"
    if any(evidence.get(key) is not True for key in required):
        return "unverified"
    return "pass"


def probe_copied_data(
    state_root: Path,
    witness: dict[str, str],
) -> dict[str, Any]:
    """Read a pre-existing agent from the marked copy without fixture seeding."""
    sys.path.insert(0, str(TESTS))
    from http_backend_harness import BackendProcess

    DEFAULT_CACHE.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(
        prefix="rust-http-copy-read-", dir=DEFAULT_CACHE,
    ) as scratch_name:
        scratch = Path(scratch_name).resolve(strict=True)
        backend = BackendProcess(
            scratch,
            claude_bin=None,
            initialize_workspace=False,
            state_root=state_root,
        )
        preservation_snapshot: SqliteSnapshot | None = None
        finder_snapshot: SqliteSnapshot | None = None
        try:
            backend.create_codex_replay("copy-read-only-replay.json")
            # Pin the pre-start database so startup changes can be checked
            # against its old tables and rows after the finder read.
            preservation_snapshot = sqlite_snapshot(state_root / "canvas.sqlite3")
            backend.start()
            headers = {
                "X-Canvas-Token": backend.session["token"],
                "X-Studio-API-Schema": backend.schema_hash,
            }
            finder_snapshot = sqlite_snapshot(state_root / "canvas.sqlite3")
            started = time.monotonic()
            result = backend.request(
                "GET",
                "/api/agents/finder-view?agent="
                + urllib.parse.quote(witness["id"], safe=""),
                headers=headers,
                timeout=2.0,
            )
            unchanged_during_read = finder_snapshot.unchanged()
            old_data = preservation_snapshot.preserved_existing_rows()
            try:
                body = result.json()
            except (ValueError, TypeError):
                body = None
            read_matches = result.status == 200 and body == {
                "kind": "native",
                "path": witness["cwd"],
            }
            return {
                "source": "marked-copy-existing-data",
                "status": copied_data_evidence_status({
                    "stateCheckBefore": None,
                    "existingRowRead": read_matches,
                    "preexistingRowsPreservedFromBeforeStartup": old_data["preserved"],
                    "unchangedDuringFinderRead": unchanged_during_read,
                    "schemaChangesReviewed": None,
                    "derivedIndexesReviewed": None,
                    "stateCheckAfter": None,
                    "baselineRestore": None,
                    "pythonRollbackReadback": None,
                }),
                "existingRowReads": [{
                    "resource": "pre-existing native agent",
                    "observedStatus": result.status,
                    "matched": read_matches,
                    "elapsedSeconds": round(time.monotonic() - started, 4),
                }],
                "preexistingRowsPreservedFromBeforeStartup": old_data["preserved"],
                "unchangedDuringFinderRead": unchanged_during_read,
                "schemaChangesReviewed": None,
                "derivedIndexesReviewed": None,
                "schemaChanges": {
                    "newTables": old_data["newTables"],
                    "newColumns": old_data["newColumns"],
                    "newSchemaObjects": old_data["newSchemaObjects"],
                    "changedTableDefinitions": old_data["changedTableDefinitions"],
                    "unsupportedTables": old_data["unsupportedTables"],
                    "unreadableDerivedIndexes": old_data["unreadableDerivedIndexes"],
                    "derivedIndexSchemaChanges": old_data["derivedIndexSchemaChanges"],
                    "internalBookkeepingTables": old_data["internalBookkeepingTables"],
                    "sqliteSequenceChanged": old_data["sqliteSequenceChanged"],
                },
                "stateCheckBefore": None,
                "stateCheckAfter": None,
                "baselineRestore": None,
                "pythonRollbackReadback": None,
            }
        finally:
            if finder_snapshot is not None:
                finder_snapshot.close()
            if preservation_snapshot is not None:
                preservation_snapshot.close()
            backend.close()


def probe_status_matches(
    expected: Any,
    status: int,
    *,
    standard_501: bool,
    response_body: Any,
    expected_error: str | None = None,
) -> bool:
    """Match only an explicit status contract; never infer success from the census."""
    if expected_error is not None and response_body != {"error": expected_error}:
        return False
    if expected == "501":
        return status == 501 and standard_501
    if expected == "404":
        return status == 404 and response_body == {"error": "Not found"}
    if expected in {"400", "403", "409"}:
        return status == int(expected)
    if expected == "4xx":
        return 400 <= status < 500
    if expected == "2xx":
        return 200 <= status < 300
    if isinstance(expected, str) and expected.isdecimal():
        return status == int(expected)
    return False


def apply_probe_expectations(
    classification: str,
    cases: list[tuple[str, dict[str, Any]]],
    observations: list[dict[str, Any]],
) -> str:
    if classification == "unexpected":
        matching_server_error = any(
            observation["status"] >= 500
            and observation["expectedMatches"]
            and observation.get("expected") == str(observation["status"])
            for observation in observations
        )
        if not matching_server_error:
            return classification
        classification = "unverified"
    declared_case_failed = any(
        case.get("expected") is not None
        and not case.get("unverified")
        and observations[index].get("dispatched", True)
        and not observations[index]["expectedMatches"]
        for index, (_, case) in enumerate(cases)
    )
    if declared_case_failed:
        return "unexpected"
    unverified_cases = [
        index for index, (_, case) in enumerate(cases)
        if observations[index].get("dispatched", True)
        and (case.get("unverified") or not case.get("expected"))
    ]
    if unverified_cases:
        unresolved_server_error = any(
            observations[index]["status"] >= 500
            and observations[index]["status"] != 501
            for index in unverified_cases
        )
        classification = "unexpected" if unresolved_server_error else "unverified"
    return classification


def unsafe_probe_exclusion(method: str, template: str) -> str | None:
    """Keep global-preference and reset-credit writes out of every ledger run."""
    return UNSAFE_PROBE_EXCLUSIONS.get((method, template))


def persisted_probe_body(
    probe_case: dict[str, Any],
    response_body: Any,
    *,
    copy_mode: bool = False,
) -> Any:
    """Keep copied-state contents and paths out of portable ledger reports."""
    if copy_mode or probe_case.get("functional_witness") == "copy-existing-agent":
        return None
    return response_body


def seed_catalog_state(backend: Any, python: str) -> None:
    accounts = backend.state / "accounts"
    if accounts.exists():
        raise RuntimeError("ledger fixture refuses to overwrite a copied accounts directory")
    accounts.mkdir(parents=True)
    account_key = getattr(backend, "account_key", "default")
    fixture_namespace = account_fixture_namespace(backend)
    (accounts / "registry.json").write_text(json.dumps({
        "version": 1,
        "defaultAccountKey": account_key,
        "accounts": {
            account_key: {
                "id": account_key, "home": str(backend.codex_home), "label": "Codex",
                "source": "Codex CLI", "provider": "codex", "accountId": "fixture-account",
                "status": "ready",
            },
            f"{fixture_namespace}disconnected": {
                "id": f"{fixture_namespace}disconnected", "home": str(backend.root / "missing-account"),
                "label": "Disconnected", "source": "ledger fixture", "provider": "codex",
                "accountId": "stale-id", "status": "ready", "disconnected": True,
            },
            f"{fixture_namespace}claude-ledger": {
                "id": f"{fixture_namespace}claude-ledger", "home": str(backend.codex_home), "label": "Claude probe",
                "source": "ledger fixture", "provider": "claude", "accountId": "claude:ledger",
                "status": "ready",
            },
        },
        "logins": {},
    }), encoding="utf-8")
    environment = runtime_environment(backend)
    environment["CODEX_AGENTS_PYTHON"] = python
    environment["PYTHONPATH"] = os.pathsep.join(filter(None, (
        str(TESTS), str(ROOT / "workspaces/runtime/apps/server/src"), environment.get("PYTHONPATH", ""),
    )))
    subprocess.run(
        [python, "-c", "from runtime_test_environment import make_runtime; from codex_runtime import Runtime; import os, sys; make_runtime(Runtime, sys.argv[1], environment=dict(os.environ)).close()", str(backend.state)],
        check=True,
        env=environment,
        timeout=30,
    )


def seed_codex_login(backend: Any, python: str) -> str:
    """Create the pending local login through AccountStore's Python writer."""
    login_id = backend.fixture_login_id
    account_prefix = backend.fixture_namespace or "ledger-"
    account_key = f"{account_prefix}pending-login"
    profile = backend.state / "accounts" / account_key
    profile.mkdir(parents=True, exist_ok=True)
    source = ROOT / "workspaces/runtime/apps/server/src"
    environment = backend._environment()
    environment["PYTHONPATH"] = os.pathsep.join(filter(None, (str(source), environment.get("PYTHONPATH", ""))))
    environment["CODEX_AGENTS_PYTHON"] = python
    script = f"""
import sys
from pathlib import Path
sys.path.insert(0, {str(source)!r})
from codex_accounts import AccountStore
store = AccountStore(Path(sys.argv[1]))
store.data["accounts"][{account_key!r}] = {{
    "id": {account_key!r}, "home": {str(profile)!r}, "label": "Pending login fixture",
    "source": "ledger fixture", "provider": "codex", "accountId": None,
    "email": None, "plan": None, "status": "signedOut",
    "_credentialIdentity": None, "duplicateOf": {backend.account_key!r},
}}
store.data["logins"][{login_id!r}] = {{
    "requestId": {login_id!r}, "accountKey": {account_key!r}, "status": "pending",
    "createdAt": 1.0, "loginId": "ledger-live-login",
}}
store._save()
"""
    subprocess.run([python, "-B", "-c", script, str(backend.state)], env=environment, check=True, timeout=30)
    return login_id


def case_results(log_text: str) -> list[dict[str, Any]]:
    cases: list[dict[str, Any]] = []
    current: dict[str, str] | None = None
    for line in log_text.splitlines():
        start = re.match(r"^(test_[\w]+)\s+\(([^)]*)\)\s+\.\.\.\s*(.*)$", line)
        if start:
            if current is not None:
                # unittest writes test output and warnings to separate streams;
                # a warning can put the next test heading before this test's status.
                cases.append({**current, "status": "unreported"})
            current = {"test": start.group(1), "testId": start.group(2)}
            tail = start.group(3).strip()
            if tail in {"ok", "FAIL", "ERROR"} or tail.startswith("skipped"):
                cases.append({**current, "status": tail.split()[0].lower()})
                current = None
            continue
        if current is not None and line.strip() in {"ok", "FAIL", "ERROR"}:
            cases.append({**current, "status": line.strip().lower()})
            current = None
    if current is not None:
        cases.append({**current, "status": "unreported"})
    for failure in failure_headings(log_text):
        if failure["test"] == "setUpClass":
            test_id = f"setUpClass ({failure['context']})"
            if not any(case["testId"] == test_id for case in cases):
                cases.append({
                    "test": failure["test"],
                    "testId": test_id,
                    "status": failure["kind"],
                })
    return cases


def failure_headings(log_text: str) -> list[dict[str, str]]:
    failures: list[dict[str, str]] = []
    pattern = re.compile(r"^(FAIL|ERROR):\s+(test_[\w]+|setUpClass)\s+\(([^)]*)\)")
    for line in log_text.splitlines():
        match = pattern.match(line)
        if match:
            failures.append({"kind": match.group(1).lower(), "test": match.group(2), "context": match.group(3)})
    return failures


def install_request_trace(directory: Path) -> Path:
    """Trace consumer requests in the harness process without changing its fixtures."""
    hook = directory / "sitecustomize.py"
    hook.write_text(
        '''import io, json, os, threading, unittest
from urllib.error import HTTPError
from urllib.parse import urlsplit
from urllib.request import Request
import urllib.request

_trace_local = threading.local()
_trace_lock = threading.Lock()
_original_run = unittest.TestCase.run
_original_urlopen = urllib.request.urlopen

def _run(test, result=None):
    _trace_local.case = test.id()
    try:
        return _original_run(test, result)
    finally:
        _trace_local.case = None

def _record(request, status, body=None):
    destination = os.environ.get("STUDIO_HTTP_LEDGER_TRACE")
    if not destination:
        return
    if isinstance(request, Request):
        method = request.get_method()
        url = request.full_url
    else:
        method = "GET"
        url = str(request)
    record = {"test": getattr(_trace_local, "case", None), "method": method,
              "path": urlsplit(url).path, "status": status}
    if body is not None:
        try:
            record["body"] = json.loads(body)
        except (ValueError, TypeError):
            record["body"] = body.decode("utf-8", "replace")
    with _trace_lock, open(destination, "a", encoding="utf-8") as stream:
        stream.write(json.dumps(record, separators=(",", ":")) + "\\n")

def _urlopen(request, *args, **kwargs):
    try:
        response = _original_urlopen(request, *args, **kwargs)
    except HTTPError as error:
        body = error.read()
        _record(request, error.code, body)
        raise HTTPError(error.url, error.code, error.msg, error.headers, io.BytesIO(body))
    _record(request, response.status)
    return response

unittest.TestCase.run = _run
urllib.request.urlopen = _urlopen
''',
        encoding="utf-8",
    )
    return hook


def canonical_operation(method: str, path: str, known: list[tuple[str, str]]) -> str | None:
    parts = path.split("/")
    for expected_method, template in known:
        expected_parts = template.split("/")
        if method == expected_method and len(parts) == len(expected_parts) and all(
            want.startswith("{") or want == actual
            for want, actual in zip(expected_parts, parts)
        ):
            return f"{method} {template}"
    hidden_voice = ("POST", "/api/voice/{action:path}")
    if method == hidden_voice[0] and path.startswith("/api/voice/") and len(path) > len("/api/voice/"):
        return f"{hidden_voice[0]} {hidden_voice[1]}"
    return None


def run_suite(
    path: Path,
    environment: dict[str, str],
    trace_root: Path,
    known_operations: list[tuple[str, str]],
    manifest: dict[tuple[str, str], dict[str, Any]],
) -> dict[str, Any]:
    relative = str(path.relative_to(ROOT))
    trace_path = trace_root / f"{path.stem}.jsonl"
    python_baseline_only = relative in PYTHON_BASELINE_ONLY_SUITES
    suite_environment = environment.copy()
    if python_baseline_only:
        suite_environment.pop("STUDIO_HTTP_BACKEND_COMMAND", None)
        suite_environment.pop("STUDIO_HTTP_LEDGER_TRACE", None)
    else:
        suite_environment["STUDIO_HTTP_LEDGER_TRACE"] = str(trace_path)
    # Preserve complete unittest records and HTTP attribution across runner output.
    suite_environment["CODEX_SERVER_TEST_JOBS"] = "1"
    skipped_for_service = missing_service_tests_for_suite(relative)
    if skipped_for_service:
        suite_environment["STUDIO_HTTP_LEDGER_SKIP_TESTS"] = json.dumps({
            test_id: details["reason"] for test_id, details in skipped_for_service.items()
        })
    process = subprocess.run(
        ["just", "test-json", "server", relative],
        cwd=ROOT,
        env=suite_environment,
        text=True,
        capture_output=True,
        check=False,
    )
    try:
        result = json.loads(process.stdout.strip().splitlines()[-1])
    except (IndexError, json.JSONDecodeError):
        result = {
            "status": "failed",
            "passed": 0,
            "failed": 1,
            "skipped": 0,
            "executed": 1,
            "exitCode": process.returncode or 1,
            "diagnostic": (process.stderr + process.stdout)[-4000:],
        }
    log_path = result.get("logPath")
    log_text = Path(log_path).read_text(encoding="utf-8") if log_path else ""
    cases = case_results(log_text)
    actual_skips = {case["testId"] for case in cases if case["status"] == "skipped"}
    required_skips = set(skipped_for_service)
    if not required_skips.issubset(actual_skips):
        raise RuntimeError(
            f"missing-service test skip mismatch in {relative}: "
            f"expected at least={sorted(required_skips)}, observed={sorted(actual_skips)}"
        )
    observed_counts = {
        "passed": sum(case["status"] == "ok" for case in cases),
        "failed": sum(case["status"] in {"fail", "error"} for case in cases),
        "skipped": sum(case["status"] == "skipped" for case in cases),
    }
    ran_summaries = re.findall(r"^Ran (\d+) tests? in ", log_text, re.MULTILINE)
    setup_failures = {
        failure["context"]
        for failure in failure_headings(log_text)
        if failure["test"] == "setUpClass"
    }
    # unittest reports a failed class setup as a failure but excludes it from
    # `Ran N tests`; include each such suite-level case in the count contract.
    ran_count = sum(map(int, ran_summaries)) + len(setup_failures)
    suite_summary = re.search(
        r"Server suites: (\d+) passed, (\d+) failed, (\d+) skipped \(opt-in\), (\d+) skipped \(environment\)",
        log_text,
    )
    structured_failed_suites = len(re.findall(r"^FAIL .*failing IDs:", log_text, re.MULTILINE))
    if not ran_summaries or suite_summary is None:
        raise RuntimeError(f"consumer result summary missing in {relative}")
    suite_counts = {
        "passed": int(suite_summary.group(1)),
        "failed": max(0, int(suite_summary.group(2)) - structured_failed_suites),
        "skipped": int(suite_summary.group(3)) + int(suite_summary.group(4)),
    }
    reported_counts = {name: result.get(name, 0) or 0 for name in suite_counts}
    expected_test_counts = {
        name: reported_counts[name] - suite_counts[name] for name in suite_counts
    }
    unreported = [case for case in cases if case["status"] == "unreported"]
    observed_test_counts = {
        "passed": observed_counts["passed"],
        "failed": observed_counts["failed"],
        "skipped": observed_counts["skipped"],
    }
    missing_counts = {
        name: expected_test_counts[name] - observed_test_counts[name]
        for name in expected_test_counts
    }
    if (
        any(count < 0 for count in missing_counts.values())
        or sum(missing_counts.values()) != len(unreported)
        or sum(count > 0 for count in missing_counts.values()) > 1
    ):
        raise RuntimeError(
            f"consumer result-count mismatch in {relative}: "
            f"unreported test statuses cannot be reconciled: missing={missing_counts}, "
            f"unreported={[case['testId'] for case in unreported]}"
        )
    for status, count in missing_counts.items():
        if count:
            for case in unreported:
                case["status"] = {"passed": "ok", "failed": "fail", "skipped": "skipped"}[status]
            break
    observed_counts = {
        "passed": sum(case["status"] == "ok" for case in cases),
        "failed": sum(case["status"] in {"fail", "error"} for case in cases),
        "skipped": sum(case["status"] == "skipped" for case in cases),
    }
    expected_reported_counts = {
        name: observed_counts[name] + suite_counts[name] for name in suite_counts
    }
    if (
        sum(observed_counts.values()) != ran_count
        or expected_reported_counts != reported_counts
    ):
        raise RuntimeError(
            f"consumer result-count mismatch in {relative}: "
            f"parsed={observed_counts}, reported={reported_counts}, "
            f"expected-reported={expected_reported_counts}, ran={ran_count}"
        )
    requests: list[dict[str, Any]] = []
    if trace_path.exists():
        for line in trace_path.read_text(encoding="utf-8").splitlines():
            event = json.loads(line)
            event["operation"] = canonical_operation(event["method"], event["path"], known_operations)
            event["assertion"] = (
                f"{relative}::{event['test']}" if event.get("test") else None
            )
            requests.append(event)
    by_test: dict[str, list[dict[str, Any]]] = defaultdict(list)
    for event in requests:
        if event["test"]:
            by_test[event["test"]].append(event)
        else:
            by_test[None].append(event)
    unattributed = by_test.get(None, [])
    failures = []
    for failure in failure_headings(log_text):
        test_id = failure["context"]
        evidence = unattributed if failure["test"] == "setUpClass" else by_test.get(test_id, [])
        blockers = sorted({event["operation"] for event in evidence if event["status"] == 501 and event["operation"]})
        failure.update({
            "requests": evidence,
            "blocking501": blockers,
            "standard501Envelope": all(
                valid_501_envelope(event, manifest)
                for event in evidence if event["status"] == 501
            ),
        })
        failures.append(failure)
    for case in cases:
        case_requests = by_test.get(case["testId"], [])
        case["requests"] = case_requests
        case["blocking501"] = [
            event["operation"] for event in case_requests
            if event["status"] == 501 and event["operation"]
        ]
        case["standard501Envelope"] = all(valid_501_envelope(event, manifest) for event in case_requests if event["status"] == 501)
    return {
        "suite": relative,
        "backend": "python-baseline" if python_baseline_only else "rust",
        "status": result.get("status", "failed"),
        "passed": result.get("passed") or 0,
        "failed": result.get("failed") or 0,
        "skipped": result.get("skipped") or 0,
        "exitCode": result.get("exitCode", process.returncode),
        "tests": cases,
        "requiresMissingServices": [
            {"testId": test_id, **details}
            for test_id, details in sorted(MISSING_SERVICE_TESTS.get(relative, {}).items())
        ],
        "undeclaredProviderTurn": (
            {
                "service": UNDECLARED_PROVIDER_TURN_SERVICE,
                "reason": "requires a provider turn; no operation declares it yet",
                "tests": [
                    {"testId": test_id, "reason": reason}
                    for test_id, reason in sorted(
                        UNDECLARED_PROVIDER_TURN_TESTS.get(relative, {}).items()
                    )
                ],
            }
            if UNDECLARED_PROVIDER_TURN_TESTS.get(relative)
            else None
        ),
        "failures": failures,
        "requests": requests,
        "unattributedRequests": unattributed,
        **({"diagnostic": result["diagnostic"]} if "diagnostic" in result else {}),
    }


def sqlite_snapshot(path: Path) -> SqliteSnapshot:
    """Pin a read-only view and skip table scans when no other connection committed."""
    return SqliteSnapshot(path)


class SqliteSnapshot:
    """Compare SQLite contents or preserve old rows with bounded Python memory."""

    def __init__(self, path: Path) -> None:
        self.path = path
        self.identity = _sqlite_file_identity(path)
        self.monitor = _open_readonly_sqlite(path)
        self.baseline = _open_readonly_sqlite(path)
        self.version_before = self._data_version()
        self.baseline.execute("BEGIN")
        _sqlite_schema(self.baseline)
        self.stable = (
            self.version_before == self._data_version()
            and self.identity == _sqlite_file_identity(path)
        )

    def _data_version(self) -> int:
        return int(self.monitor.execute("PRAGMA data_version").fetchone()[0])

    def unchanged(self) -> bool:
        """Use SQLite's connection-local commit counter before a streamed comparison."""
        if not self.stable:
            return False
        try:
            current_identity = _sqlite_file_identity(self.path)
            version_at_compare = self._data_version()
        except (OSError, sqlite3.Error):
            return False
        if self.identity != current_identity:
            return False
        if self.version_before == version_at_compare:
            return True

        try:
            current = _open_readonly_sqlite(self.path)
        except (OSError, sqlite3.Error):
            return False
        try:
            current.execute("BEGIN")
            equal = _sqlite_content_digest(self.baseline) == _sqlite_content_digest(current)
            return equal and version_at_compare == self._data_version()
        finally:
            current.close()

    def preserved_existing_rows(self) -> dict[str, Any]:
        """Prove old rows remain intact while allowing appended rows/schema."""
        result: dict[str, Any] = {
            "preserved": None,
            "newTables": 0,
            "newColumns": 0,
            "newSchemaObjects": 0,
            "changedTableDefinitions": 0,
            "unsupportedTables": 0,
            "unreadableDerivedIndexes": 0,
            "derivedIndexSchemaChanges": 0,
            "internalBookkeepingTables": 0,
            "sqliteSequenceChanged": False,
        }
        if not self.stable:
            result["preserved"] = False
            return result
        try:
            if _sqlite_file_identity(self.path) != self.identity:
                result["preserved"] = False
                return result
            before_schema = _preservation_schema(self.baseline, "main")
            before_tables = {
                name for (kind, name) in before_schema if kind == "table"
            }
            known_fts = _known_contentless_fts_tables(before_schema)
            result["unreadableDerivedIndexes"] = len(known_fts)
            result["internalBookkeepingTables"] = sum(
                name.startswith("sqlite_") for name in before_tables
            )
            version_before = self._data_version()
            if version_before == self.version_before:
                result["preserved"] = True
                return result
            current_uri = self.path.resolve().as_uri() + "?mode=ro"
            self.baseline.execute(
                "ATTACH DATABASE ? AS after_state", (current_uri,),
            )
            after_schema = _preservation_schema(self.baseline, "after_state")
            after_tables = {
                name for (kind, name) in after_schema if kind == "table"
            }
            after_fts = _known_contentless_fts_tables(after_schema)
            derived_and_shadow_tables = _known_fts_shadow_tables(
                before_schema, after_schema,
            )
            result["unreadableDerivedIndexes"] = len(known_fts)
            result["derivedIndexSchemaChanges"] = sum(
                before_schema.get(("table", name))
                != after_schema.get(("table", name))
                for name in known_fts | after_fts
            )
            result["newTables"] = sum(
                1 for name in after_tables - before_tables
                if not name.startswith("sqlite_")
                and name not in (known_fts | after_fts)
                and name not in derived_and_shadow_tables
            )
            result["newSchemaObjects"] = sum(
                kind != "table"
                and not name.startswith("sqlite_")
                and (kind, name) not in before_schema
                for kind, name in after_schema
            )
            old_user_tables = {
                name for name in before_tables
                if not name.startswith("sqlite_")
                and name not in derived_and_shadow_tables
            }
            new_user_tables = {
                name for name in after_tables
                if not name.startswith("sqlite_")
                and name not in derived_and_shadow_tables
            }
            preserved: bool | None = old_user_tables.issubset(new_user_tables)
            for table in sorted(old_user_tables & new_user_tables):
                old_sql = before_schema[("table", table)][1]
                new_sql = after_schema[("table", table)][1]
                if old_sql != new_sql:
                    result["changedTableDefinitions"] += 1
                old_columns = _preservation_table_columns(
                    self.baseline, "main", table,
                )
                new_columns = _preservation_table_columns(
                    self.baseline, "after_state", table,
                )
                result["newColumns"] += len(new_columns.keys() - old_columns.keys())
                if any(
                    new_columns.get(name) != definition
                    for name, definition in old_columns.items()
                ):
                    preserved = False
                    continue
                if old_sql.lstrip().upper().startswith("CREATE VIRTUAL TABLE"):
                    result["unsupportedTables"] += 1
                    if preserved is True:
                        preserved = None
                    continue
                comparison = _preserve_table_rows(
                    self.baseline, table, old_columns,
                    new_columns,
                    without_rowid=bool(re.search(r"\bWITHOUT\s+ROWID\b", old_sql, re.I)),
                )
                if comparison is None:
                    result["unsupportedTables"] += 1
                    if preserved is True:
                        preserved = None
                elif not comparison:
                    preserved = False

            result["sqliteSequenceChanged"] = _sqlite_sequence_changed(
                self.baseline, before_schema, after_schema,
            )

            for key, (_table_name, old_sql) in before_schema.items():
                kind, _name = key
                if (
                    kind == "table"
                    or _name.startswith("sqlite_")
                    or _name in derived_and_shadow_tables
                ):
                    continue
                current_object = after_schema.get(key)
                if current_object is None or current_object[1] != old_sql:
                    preserved = False
            if self._data_version() != version_before:
                # A concurrent writer made the comparison's endpoint unstable.
                result["preserved"] = None if preserved is not False else False
            else:
                result["preserved"] = preserved
            return result
        except (OSError, sqlite3.Error):
            result["preserved"] = None
            return result

    def close(self) -> None:
        self.baseline.close()
        self.monitor.close()

    def __enter__(self) -> SqliteSnapshot:
        return self

    def __exit__(self, _exc_type: Any, _exc: Any, _traceback: Any) -> None:
        self.close()


def _open_readonly_sqlite(path: Path) -> sqlite3.Connection:
    connection = sqlite3.connect(f"{path.resolve().as_uri()}?mode=ro", uri=True)
    connection.execute("PRAGMA query_only=ON")
    connection.execute("PRAGMA cache_size=-2048")
    connection.execute("PRAGMA temp_store=FILE")
    connection.execute("PRAGMA mmap_size=0")
    return connection


def _sqlite_file_identity(path: Path) -> tuple[int, int]:
    stat = path.stat()
    return stat.st_dev, stat.st_ino


def _sqlite_schema(connection: sqlite3.Connection) -> tuple[tuple[Any, ...], ...]:
    return tuple(connection.execute(
        "SELECT type,name,tbl_name,sql FROM sqlite_master "
        "WHERE name NOT LIKE 'sqlite_%' "
        "ORDER BY type COLLATE BINARY,name COLLATE BINARY"
    ))


def _preservation_schema(
    connection: sqlite3.Connection,
    schema: str,
) -> dict[tuple[str, str], tuple[str, str | None]]:
    if schema not in {"main", "after_state"}:
        raise ValueError("unsupported SQLite schema name")
    return {
        (kind, name): (table, sql)
        for kind, name, table, sql in connection.execute(
            f"SELECT type,name,tbl_name,sql FROM {schema}.sqlite_master "
            "WHERE type IN ('table','index','view','trigger') "
            "ORDER BY type COLLATE BINARY,name COLLATE BINARY"
        )
    }


_CONTENTLESS_DERIVED_FTS = "runtime_search_next"
_CONTENTLESS_FTS_SHADOWS = frozenset({
    "runtime_search_next_config",
    "runtime_search_next_data",
    "runtime_search_next_docsize",
    "runtime_search_next_idx",
})


def _known_contentless_fts_tables(
    schema: dict[tuple[str, str], tuple[str, str | None]],
) -> set[str]:
    """Recognize only the repository's known unreadable derived FTS index."""
    entry = schema.get(("table", _CONTENTLESS_DERIVED_FTS))
    if entry is None or not entry[1]:
        return set()
    sql = entry[1]
    if (
        not sql.lstrip().upper().startswith("CREATE VIRTUAL TABLE")
        or re.search(r"\bUSING\s+fts5\s*\(", sql, re.I) is None
        or re.search(r"""\bcontent\s*=\s*(['"])\s*\1""", sql, re.I) is None
    ):
        return set()
    return {_CONTENTLESS_DERIVED_FTS}


def _known_fts_shadow_tables(
    before_schema: dict[tuple[str, str], tuple[str, str | None]],
    after_schema: dict[tuple[str, str], tuple[str, str | None]],
) -> set[str]:
    has_known_index = bool(
        _known_contentless_fts_tables(before_schema)
        or _known_contentless_fts_tables(after_schema)
    )
    if not has_known_index:
        return set()
    return {*_CONTENTLESS_FTS_SHADOWS, _CONTENTLESS_DERIVED_FTS}


def _preservation_table_columns(
    connection: sqlite3.Connection,
    schema: str,
    table: str,
) -> dict[str, tuple[Any, ...]]:
    if schema not in {"main", "after_state"}:
        raise ValueError("unsupported SQLite schema name")
    quoted_table = _quote_sqlite_identifier(table)
    return {
        name: (column_type, not_null, default, primary_key_order, hidden)
        for _cid, name, column_type, not_null, default, primary_key_order, hidden
        in connection.execute(f"PRAGMA {schema}.table_xinfo({quoted_table})")
    }


def _preserve_table_rows(
    connection: sqlite3.Connection,
    table: str,
    columns: dict[str, tuple[Any, ...]],
    current_columns: dict[str, tuple[Any, ...]],
    *,
    without_rowid: bool,
) -> bool | None:
    """Stream old identities through an indexed join; None means unsupported."""
    quoted_table = _quote_sqlite_identifier(table)
    folded_columns = {
        name.casefold() for name in (*columns.keys(), *current_columns.keys())
    }
    rowid_alias = next(
        (name for name in ("_rowid_", "rowid", "oid") if name not in folded_columns),
        None,
    )
    if rowid_alias is not None and not without_rowid:
        old_identity = f"old_row.{rowid_alias}"
        new_identity = f"current_row.{rowid_alias}"
        join = f"{new_identity} IS {old_identity}"
    else:
        primary_keys = sorted(
            (
                (metadata[3], name)
                for name, metadata in columns.items()
                if metadata[3]
            ),
        )
        if not primary_keys:
            return None
        if not without_rowid and not all(columns[name][1] for _, name in primary_keys):
            # Legacy rowid tables can allow NULL in composite primary keys.
            # If their rowid aliases are shadowed, there is no safe identity.
            return None
        old_identity = f"old_row.{_quote_sqlite_identifier(primary_keys[0][1])}"
        new_identity = f"current_row.{_quote_sqlite_identifier(primary_keys[0][1])}"
        join_terms = []
        for _, name in primary_keys:
            quoted = _quote_sqlite_identifier(name)
            join_terms.append(
                f"typeof(current_row.{quoted}) = typeof(old_row.{quoted}) "
                f"AND current_row.{quoted} IS old_row.{quoted}"
            )
        join = " AND ".join(join_terms)

    differences = []
    for name in columns:
        quoted = _quote_sqlite_identifier(name)
        differences.append(
            f"typeof(current_row.{quoted}) <> typeof(old_row.{quoted}) "
            f"OR (typeof(old_row.{quoted}) IN ('text','blob') "
            f"AND CAST(current_row.{quoted} AS BLOB) IS NOT "
            f"CAST(old_row.{quoted} AS BLOB)) "
            f"OR (typeof(old_row.{quoted}) NOT IN ('text','blob') "
            f"AND current_row.{quoted} IS NOT old_row.{quoted})"
        )
    mismatch = " OR ".join(differences) or "0"
    query = (
        f"SELECT 1 FROM main.{quoted_table} AS old_row "
        f"LEFT JOIN after_state.{quoted_table} AS current_row ON {join} "
        f"WHERE {new_identity} IS NULL OR ({mismatch}) LIMIT 1"
    )
    return connection.execute(query).fetchone() is None


def _sqlite_sequence_changed(
    connection: sqlite3.Connection,
    before_schema: dict[tuple[str, str], tuple[str, str | None]],
    after_schema: dict[tuple[str, str], tuple[str, str | None]],
) -> bool | None:
    """Report SQLite AUTOINCREMENT bookkeeping outside old user-row proof."""
    before = before_schema.get(("table", "sqlite_sequence"))
    after = after_schema.get(("table", "sqlite_sequence"))
    if before is None and after is None:
        return False
    if before is None or after is None or before[1] != after[1]:
        return True
    old_columns = _preservation_table_columns(connection, "main", "sqlite_sequence")
    new_columns = _preservation_table_columns(
        connection, "after_state", "sqlite_sequence",
    )
    if old_columns != new_columns:
        return True
    rows_preserved = _preserve_table_rows(
        connection, "sqlite_sequence", old_columns, new_columns,
        without_rowid=False,
    )
    if rows_preserved is None:
        return None
    old_count = connection.execute(
        "SELECT COUNT(*) FROM main.sqlite_sequence",
    ).fetchone()[0]
    new_count = connection.execute(
        "SELECT COUNT(*) FROM after_state.sqlite_sequence",
    ).fetchone()[0]
    return not rows_preserved or old_count != new_count


def _sqlite_content_digest(connection: sqlite3.Connection) -> bytes:
    digest = hashlib.sha256()
    schema = _sqlite_schema(connection)
    for entry in schema:
        _update_sqlite_digest(digest, ("schema", entry))
    for kind, table, _owner, _sql in schema:
        if kind != "table":
            continue
        quoted_table = _quote_sqlite_identifier(table)
        order_clause = _sqlite_table_order(connection, table)
        _update_sqlite_digest(digest, ("table", table))
        row_count = 0
        rows = connection.execute(f"SELECT * FROM {quoted_table}{order_clause}")
        for row in rows:
            _update_sqlite_digest(digest, row)
            row_count += 1
        _update_sqlite_digest(digest, ("end-table", table, row_count))
    return digest.digest()


def _update_sqlite_digest(digest: Any, value: Any) -> None:
    encoded = repr(value).encode("utf-8", "surrogatepass")
    digest.update(len(encoded).to_bytes(8, "big"))
    digest.update(encoded)


def _quote_sqlite_identifier(value: str) -> str:
    return '"' + value.replace('"', '""') + '"'


def _sqlite_table_order(connection: sqlite3.Connection, table: str) -> str:
    quoted_table = _quote_sqlite_identifier(table)
    columns = connection.execute(f"SELECT * FROM {quoted_table} LIMIT 0").description or ()
    order = ",".join(
        item
        for column in (item[0] for item in columns)
        for item in (
            f"typeof({_quote_sqlite_identifier(column)}) COLLATE BINARY",
            f"{_quote_sqlite_identifier(column)} COLLATE BINARY",
            f"printf('%+.17g',{_quote_sqlite_identifier(column)}) COLLATE BINARY",
        )
    )
    return f" ORDER BY {order}" if order else ""


def valid_501_envelope(event: dict[str, Any], manifest: dict[tuple[str, str], dict[str, Any]]) -> bool:
    if event.get("expected_error"):
        return event.get("body") == {"error": event["expected_error"]}
    operation = event.get("operation")
    if operation:
        method, path = operation.split(" ", 1)
        declared = manifest.get((method, path))
        if declared and declared.get("status") == "partial":
            dependencies = declared.get("dependencies", [])
            return bool(dependencies) and event.get("body") == {
                "error": f"Not implemented: requires {', '.join(dependencies)}"
            }
    return event.get("body") == {"error": "Not implemented"}


def classify_observation(
    status: int,
    implementation: str,
    schema_header_matches: bool,
    standard_501_envelope: bool,
    dependency_response_matches: bool,
) -> str:
    if not schema_header_matches or not standard_501_envelope or not dependency_response_matches:
        return "unexpected"
    if implementation == "unimplemented":
        return "not_implemented" if status == 501 else "unexpected"
    if implementation == "implemented":
        if status >= 500:
            return "unexpected"
        return "implemented" if 200 <= status < 300 else "unverified"
    if implementation == "partial":
        if status == 501:
            return "partial"
        if status >= 500:
            return "unexpected"
        return "partial" if 200 <= status < 300 else "unverified"
    return "unexpected"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=DEFAULT_LEDGER)
    parser.add_argument(
        "--state-root", type=Path,
        help=f"run read-only old-data verification on a marked backup copy (requires {COPY_MARKER})",
    )
    parser.add_argument(
        "--live-state-root", type=Path,
        help="known live state root that a marked backup copy must not alias",
    )
    parser.add_argument(
        "--router-evidence", type=Path,
        help="JSON sidecar binding the exact source revision to a hashed nextest CI JUnit report",
    )
    options = parser.parse_args()
    command = os.environ.get("STUDIO_HTTP_BACKEND_COMMAND")
    if not command:
        parser.error("STUDIO_HTTP_BACKEND_COMMAND must name the Rust server executable")

    copy_state_root = None
    if options.state_root is not None:
        try:
            copy_state_root = validate_copy_state_root(
                options.state_root,
                live_state_root=options.live_state_root,
                configured_state_root=os.environ.get("CODEX_AGENTS_STATE_DIR"),
            )
            backend_command = validate_ephemeral_backend_command(command)
        except (OSError, ValueError) as error:
            parser.error(str(error))
        command = shlex.join(backend_command)
        os.environ["STUDIO_HTTP_BACKEND_COMMAND"] = command
    copy_witness = copied_agent_witness(copy_state_root) if copy_state_root is not None else None

    environment = os.environ.copy()
    isolated_python = str(resolve_python(ROOT / "workspaces/runtime/apps/server/src", environment))
    environment["STUDIO_HTTP_BACKEND_PYTHON"] = isolated_python
    differential = subprocess.run(
        [
            sys.executable,
            str(ROOT / "workspaces/runtime/apps/server/src/codex_python.py"),
            "--exec",
            str(TESTS / "http_runtime_extractor_differential.py"),
        ],
        cwd=ROOT,
        env=environment,
        check=False,
        capture_output=True,
        text=True,
    )
    if differential.returncode:
        diagnostic = (differential.stderr + differential.stdout)[-12000:]
        diagnostic_path = DEFAULT_CACHE / "http-runtime-extractor-differential-failure.txt"
        diagnostic_path.parent.mkdir(parents=True, exist_ok=True)
        diagnostic_path.write_text(diagnostic, encoding="utf-8")
        print(f"HTTP extractor differential failed; details: {diagnostic_path}", file=sys.stderr)
        raise RuntimeError("HTTP extractor differential contract failed")
    sys.stdout.write(differential.stdout)
    sys.stderr.write(differential.stderr)
    trace_regression = subprocess.run(
        [sys.executable, str(TESTS / "http_runtime_conformance_ledger_test.py")],
        cwd=ROOT, env=environment, check=False,
    )
    if trace_regression.returncode:
        raise RuntimeError("HTTP ledger error-body trace regression failed")
    routes = operations()
    manifest = rust_operation_manifest()
    validate_missing_service_exclusions(manifest)
    ensure_operation_probe_cases(manifest)
    DEFAULT_CACHE.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="rust-http-traces-", dir=DEFAULT_CACHE) as trace_dir_name:
        trace_root = Path(trace_dir_name)
        hook = install_request_trace(trace_root)
        existing_pythonpath = environment.get("PYTHONPATH", "")
        environment["PYTHONPATH"] = str(hook.parent) + (
            os.pathsep + existing_pythonpath if existing_pythonpath else ""
        )
        suites = [
            run_suite(path, environment, trace_root, routes, manifest)
            for path in consumer_suites()
        ]
    planned_assertions: dict[str, set[str]] = defaultdict(set)
    known_operations = {f"{method} {path}" for method, path in operations()}
    for operation, assertions in appendix_http_assertions(known_operations).items():
        planned_assertions[operation].update(assertions)
    successful_assertions: dict[str, set[str]] = defaultdict(set)
    for suite in suites:
        passing = {
            f"{suite['suite']}::{test['testId']}"
            for test in suite.get("tests", []) if test.get("status") == "ok"
        }
        for event in suite.get("requests", []):
            if event.get("operation") and event.get("assertion") in passing and 200 <= event["status"] < 300:
                successful_assertions[event["operation"]].add(event["assertion"])
    operations_result = probe_operations(
        manifest,
        planned_assertions,
        successful_assertions,
        python=isolated_python,
    )
    router_evidence = load_router_test_evidence(options.router_evidence)
    copied_data = (
        probe_copied_data(copy_state_root, copy_witness)
        if copy_state_root is not None and copy_witness is not None
        else None
    )
    api_evidence = api_fixture_evidence(operations_result, router_evidence)
    operation_tests: dict[str, set[str]] = defaultdict(set)
    failures_without_501: list[str] = []
    envelope_mismatches: list[str] = []
    for suite in suites:
        for index, failure in enumerate(suite["failures"]):
            test_id = f"{suite['suite']}::{failure['context']}::{failure['test']}#{index + 1}"
            blockers = set(failure["blocking501"])
            if not blockers:
                failures_without_501.append(test_id)
            for operation in blockers:
                operation_tests[operation].add(test_id)
            if blockers and not failure["standard501Envelope"]:
                envelope_mismatches.append(test_id)
    blocker_ranking = [
        {"operation": operation, "failedTests": len(tests), "tests": sorted(tests)}
        for operation, tests in sorted(
            operation_tests.items(), key=lambda entry: (-len(entry[1]), entry[0])
        )
    ]
    client_schema = ROOT / "workspaces/client/apps/web/src/generated/apiSchema.ts"
    schema_match = re.search(r'"([0-9a-f]{64})"', client_schema.read_text(encoding="utf-8"))
    if schema_match is None:
        raise ValueError("generated TypeScript schema hash is missing")
    totals = {
        "operations": len(operations_result),
        "implemented": sum(row["classification"] == "implemented" for row in operations_result),
        "partial": sum(row["classification"] == "partial" for row in operations_result),
        "notImplemented": sum(row["classification"] == "not_implemented" for row in operations_result),
        "unverified": sum(row["classification"] == "unverified" for row in operations_result),
        "uncovered": sum(not row["covered"] for row in operations_result),
        "unexpected": sum(row["classification"] == "unexpected" for row in operations_result),
        "schemaHeaderMismatches": sum(not row["schemaHeaderMatches"] for row in operations_result),
        "suites": len(suites),
        "suitePassed": sum(row["status"] == "passed" for row in suites),
        "suiteFailed": sum(row["status"] != "passed" for row in suites),
        "testsPassed": sum(row["passed"] for row in suites),
        "testsFailed": sum(row["failed"] for row in suites),
        "testsSkipped": sum(row["skipped"] for row in suites),
        "testsRequiringMissingService": sum(
            len(row.get("requiresMissingServices", []))
            + len((row.get("undeclaredProviderTurn") or {}).get("tests", []))
            for row in suites
        ),
        "failedTestsWithout501": len(failures_without_501),
        "standard501EnvelopeMismatches": len(envelope_mismatches),
        "apiPositiveOperations": api_evidence["positiveOperations"],
        "apiRouterPositiveOperations": api_evidence["routerPositiveOperations"],
        "apiUnverifiedOperations": api_evidence["unverifiedOperations"],
        "apiUnexpectedOperations": api_evidence["unexpectedOperations"],
        "apiValidatedNegativeCases": api_evidence["validatedNegativeCases"],
    }
    ledger = {
        "probeMode": "seeded-scratch",
        "apiEvidence": api_evidence,
        "routerEvidence": router_evidence,
        **({"copiedDataEvidence": copied_data} if copied_data is not None else {}),
        "schemaHash": schema_match.group(1),
        "operations": operations_result,
        "suites": suites,
        "blockingOperationRanking": blocker_ranking,
        "failedTestsWithout501": failures_without_501,
        "standard501EnvelopeMismatches": envelope_mismatches,
        "totals": totals,
    }
    options.output.parent.mkdir(parents=True, exist_ok=True)
    options.output.write_text(json.dumps(ledger, indent=2) + "\n", encoding="utf-8")
    print(json.dumps({"totals": totals}, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
