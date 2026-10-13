"""Run discovered HTTP consumers against Rust and record operation responses."""
from __future__ import annotations

import argparse
import ast
from contextlib import contextmanager
import json
import os
from pathlib import Path
import re
import sqlite3
import subprocess
import sys
import tempfile
import time
import urllib.parse
import urllib.request
from urllib.error import HTTPError
from collections import defaultdict
from typing import Any, Iterator


ROOT = Path(__file__).resolve().parents[5]
TESTS = ROOT / "workspaces/runtime/apps/server/tests"
SPEC = ROOT / "workspaces/runtime/packages/runtime-http/openapi.json"
HARNESS = TESTS / "http_backend_harness.py"
JSON_METHODS = {"POST", "PUT", "PATCH"}
DEFAULT_CACHE = Path(os.environ.get("XDG_CACHE_HOME", Path.home() / ".cache"))
DEFAULT_LEDGER = DEFAULT_CACHE / "codex-studio" / "rust-runtime-http-conformance-ledger.json"

# These consumer cases progress into provider turns which this runtime does
# not implement. The ledger skips them explicitly instead of treating a
# timeout as a conformance result.
MISSING_SERVICE_TESTS: dict[str, dict[str, dict[str, str]]] = {
    "workspaces/runtime/apps/server/tests/http-boundary-contract.py": {
        "__main__.BoundaryContractTests.test_codex_provider_turn_uses_replay_executable": {"service": "turn-driver/provider-turn", "reason": "Provider turn execution requires turn-driver/provider-turn."},
        "__main__.RestartContractTests.test_restart_serves_sync_receipt_and_provider_facts": {"service": "transcript/event-driver", "reason": "The transcript request requires transcript/event-driver."},
    },
    "workspaces/runtime/apps/server/tests/native-action-context-repair-contract.py": {
        "__main__.NativeActionHTTPContract.test_compact_request_receipt_replay_targets_one_provider_thread": {"service": "turn-driver/provider-turn", "reason": "The receipt replay is followed by a provider turn requiring turn-driver/provider-turn."},
        "__main__.NativeActionHTTPContract.test_review_request_receipt_replay_targets_one_provider_thread": {"service": "turn-driver/provider-turn", "reason": "The receipt replay is followed by a provider turn requiring turn-driver/provider-turn."},
    },
    "workspaces/runtime/apps/server/tests/protocol-fixtures-contract.py": {
        "__main__.ProtocolFixturesContract.test_supervisor_and_provider_fixtures_match_http_observations": {"service": "turn-driver/provider-turn", "reason": "The fixture comparison starts a provider turn requiring turn-driver/provider-turn."},
    },
    "workspaces/runtime/apps/server/tests/tool-request-http-contract.py": {
        "__main__.ToolRequestHTTPContract.test_provider_tool_request_is_recoverable_and_scoped_by_actor": {"service": "turn-driver/provider-turn", "reason": "Recovering the tool request requires a provider turn from turn-driver/provider-turn."},
    },
    "workspaces/runtime/apps/server/tests/yolo-contract.py": {
        "__main__.YoloHTTPContract.test_turns_after_yolo_is_disabled_use_restricted_provider_permissions": {"service": "turn-driver/provider-turn", "reason": "The test executes provider turns requiring turn-driver/provider-turn."},
        "__main__.YoloHTTPContract.test_yolo_default_inherits_to_worker_and_selects_provider_permissions": {"service": "turn-driver/provider-turn", "reason": "The test executes a worker provider turn requiring turn-driver/provider-turn."},
        "__main__.YoloWorkerMessageOrderingHTTPContract.test_message_ordering_before_and_after_worker_first_turn_is_deterministic": {"service": "turn-driver/provider-turn", "reason": "The worker first turn requires turn-driver/provider-turn."},
        "__main__.YoloRestartHTTPContract.test_disabled_yolo_mode_is_served_after_backend_restart": {"service": "turn-driver/provider-turn", "reason": "The post-restart message waits for a provider turn requiring turn-driver/provider-turn."},
    },
    "workspaces/runtime/apps/server/tests/sync-http-contract.py": {
        "__main__.SyncHTTPContract.test_voice_submit_replays_its_stable_message_id_once": {"service": "event-driver/message-receipts", "reason": "Voice replay checks the unavailable event-driver/message-receipts endpoint."},
    },
}

# Probe inputs are keyed by the documented HTTP operation so a schema-valid
# sample can point at records created by the Python runtime seed. Each case may
# override query/header values and top-level body fields. A value prefixed with
# "$" names an identifier returned by seed_python_probe_state.
PROBE_CASES: dict[tuple[str, str], dict[str, Any]] = {
    ("GET", "/api/agent-chat"): {
        "primary": {"query": {"room": "$room"}, "expected": "501"},
    },
    ("GET", "/api/agents/finder-view"): {
        "primary": {"query": {"agent": "$agent"}},
    },
    ("GET", "/api/analytics"): {
        "primary": {"query": {"agent": "$analyticsAgent"}, "query_omit": ["view", "detail", "item", "turn", "thread", "turns", "tool", "timing", "export"], "expected": "2xx"},
    },
    ("GET", "/api/capabilities"): {
        "primary": {"query": {"agent": "$agent"}},
    },
    ("GET", "/api/changes"): {
        "primary": {
            "query": {"agent": "$agent"}, "query_omit": ["scope"], "expected": "2xx",
        },
        "additional": [{
            "name": "image execution dependency",
            "query": {"agent": "$agent"},
            "query_omit": ["scope"],
            "fixture_mutations": {"agent_execution_mode": "vm"},
            "restore": ["agent_record"],
            "expected": "501",
            "expected_error": "Not implemented: requires workspace-state/image-execution",
        }],
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
            "expected": "2xx",
            "wait_for_success": True,
        },
        "additional": [{
            "name": "Claude catalog dependency",
            "query": {"account_key": "$claudeAccount"},
            "query_omit": ["workers", "retry"],
            "expected": "501",
            "expected_error": "Not implemented: requires provider-service/claude-model-catalog",
        }],
    },
    ("GET", "/api/multi-server"): {
        "primary": {"expected": "2xx"},
        "additional": [{
            "name": "local identity dependency",
            "fixture_mutations": {"remove_identity": True},
            "restore": ["identity_file"],
            "expected": "501",
            "expected_error": "Not implemented: requires server-credentials/local-identity",
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
    ("GET", "/api/messages"): {
        "primary": {"query": {"room": "$room"}},
    },
    ("GET", "/api/messages/receipts"): {
        "primary": {"query": {"agent": "$agent", "ids": "$message"}, "expected": "501", "assert_no_write": True},
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
        "primary": {"query": {"agent": "$analyticsAgent"}, "expected": "2xx"},
        "additional": [{
            "name": "stale cost cache requires refresh",
            "query": {"agent": "$analyticsAgent"},
            "fixture_mutations": {"stale_session_cost_cache": True},
            "restore": ["session_cost_cache"],
            "expected": "501",
            "expected_error": "Not implemented: requires usage-policy/session-cost-refresh",
            "assert_no_write": True,
        }],
    },
    ("GET", "/api/sync/stream"): {
        "primary": {"query": {"protocol": "3", "resources": "[{\"kind\":\"state\"}]", "apiSchema": "$schemaHash"}, "expected": "2xx"},
    },
    ("GET", "/api/skills"): {
        "primary": {"query": {"agent": "$agent"}},
    },
    ("GET", "/api/tool-requests"): {
        "primary": {"query": {"agent": "$agent"}},
    },
    ("GET", "/api/transcript"): {
        "primary": {"query": {"id": "$agent"}, "expected": "501"},
    },
    ("GET", "/api/transcript/page"): {
        "primary": {"query": {"id": "$agent"}, "expected": "501"},
    },
    ("GET", "/api/accounts/claude/login"): {
        "primary": {"query": {"request_id": "88888888-8888-4888-8888-888888888888"}, "expected": "501"},
    },
    ("GET", "/api/multi-server/v1/identity"): {"primary": {"expected": "501"}},
    ("GET", "/api/multi-server/v1/status"): {"primary": {"expected": "501"}},
    ("GET", "/api/project-locations"): {"primary": {"expected": "501"}},
    ("GET", "/api/search/item"): {
        "primary": {"query": {"id": "$agent", "message_id": "$item"}, "expected": "501"},
    },
    ("GET", "/api/search"): {
        "primary": {"query": {"q": "ledger-probe"}, "expected": "501"},
    },
    ("GET", "/api/complaint"): {
        "primary": {"query": {"id": "$complaint"}},
    },
    ("GET", "/api/file"): {
        "primary": {
            "query": {"agent": "$agent", "path": "$filePath", "asset": ""},
            "expected": "2xx",
        },
        "additional": [{
            "name": "execution-backed workspace refusal",
            "query": {"agent": "$imageAgent", "path": "$filePath", "asset": ""},
            "expected": "501",
        }],
    },
    ("GET", "/api/file-info"): {
        "primary": {
            "query": {"agent": "$agent", "path": "$filePath", "asset": ""},
            "expected": "2xx",
        },
        "additional": [{
            "name": "execution-backed workspace refusal",
            "query": {"agent": "$imageAgent", "path": "$filePath", "asset": ""},
            "expected": "501",
        }],
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
        "additional": [{"name": "live provider limits", "query": {"account_key": "default"}, "expected": "501"}],
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
        "primary": {"query": {"account_key": "default"}},
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
    ("POST", "/api/messages"): {
        "primary": {
            "body": {"id": "$message", "room": "$chat", "text": "Ledger message"},
            "expected": "2xx",
        },
        "additional": [{
            "name": "attachment payload dependency",
            "body": {
                "id": "77777777-7777-4777-8777-777777777777",
                "room": "$agent", "text": "", "assets": [{"name": "pending"}],
            },
            "expected": "501",
            "expected_error": "Not implemented: requires payload-store/uploads",
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
            "expected": "501",
            "expected_error": "Not implemented: requires federation-credentials/private-key, remote-transport/tailscale-identity, federation-credentials/key-generation, federation-credentials/invitation, remote-transport/signed-federation-exchange",
            "assert_no_write": True,
        }],
    },
    ("POST", "/api/limits/reset"): {
        "primary": {
            "body": {
                "account_key": "default", "account_id": "fixture-account",
                "credit_id": "ledger-probe-credit", "request_id": "88888888-8888-4888-8888-888888888888",
            },
            "expected": "501",
            "expected_error": "Not implemented: requires account-driver/reset-credit-redemption",
            "assert_no_write": True,
            "reason": "redemption is never performed or simulated in tests",
        },
    },
    ("POST", "/api/multi-server"): {
        "primary": {
            "body": {"action": "settings", "autoPair": False, "requestId": "ledger-probe-settings"},
            "replace_body": True,
            "expected": "2xx",
        },
        "additional": [{
            "name": "signed invite dependency",
            "body": {"action": "create_invite", "requestId": "ledger-probe-invite"},
            "replace_body": True,
            "expected": "501",
            "expected_error": "Not implemented: requires federation-credentials/private-key, multi-server-runtime/orchestration, remote-transport/identity-write, remote-transport/owner-lookup, remote-transport/signed-server-requests, remote-transport/tailscale-discovery, server-credentials/local-identity",
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
            "expected": "501",
            "expected_error": "Not implemented: requires workspace-state/peer-radio",
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
            "body": {"action": "register", "path": "$projectPath", "name": "Remote project", "server": "remote"},
            "replace_body": True,
            "expected": "501",
            "expected_error": "Not implemented: requires cross-server-driver/project-registration",
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
        "additional": [{
            "name": "provider-owned question answer dependency",
            "body": {"id": "$providerQuestion"},
            "expected": "501",
            "expected_error": "Not implemented: requires provider-session/question-answer",
        }],
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
        "additional": [{"name": "provider generated title", "body": {"id": "$agent", "name": None, "request_id": "ledger-smart-title"}, "expected": "501", "expected_error": "Not implemented: requires provider-session/title-generation", "assert_no_write": True}],
    },
    ("POST", "/api/accounts/claude/add"): {
        "primary": {"body": {"login_id": "88888888-8888-4888-8888-888888888881", "email": "ledger@example.test"}, "replace_body": True, "expected": "501", "expected_error": "Not implemented: requires provider-session/claude-login", "assert_no_write": True},
    },
    ("POST", "/api/accounts/claude/login"): {
        "primary": {"body": {"login_id": "88888888-8888-4888-8888-888888888882", "account_key": "$claudeAccount"}, "replace_body": True, "expected": "501", "expected_error": "Not implemented: requires provider-session/claude-login", "assert_no_write": True},
    },
    ("POST", "/api/accounts/claude/login/cancel"): {
        "primary": {"body": {"login_id": "88888888-8888-4888-8888-888888888883"}, "replace_body": True, "expected": "501", "expected_error": "Not implemented: requires provider-session/claude-login", "assert_no_write": True},
    },
    ("POST", "/api/accounts/claude/login/code"): {
        "primary": {"body": {"login_id": "88888888-8888-4888-8888-888888888884", "code": "123456"}, "replace_body": True, "expected": "501", "expected_error": "Not implemented: requires provider-session/claude-login", "assert_no_write": True},
    },
    ("POST", "/api/accounts/login"): {
        "primary": {"body": {"login_id": "88888888-8888-4888-8888-888888888885"}, "replace_body": True, "expected": "501", "expected_error": "Not implemented: requires provider-session/codex-login", "assert_no_write": True},
    },
    ("POST", "/api/accounts/login/cancel"): {
        "primary": {"body": {"login_id": "88888888-8888-4888-8888-888888888886"}, "replace_body": True, "expected": "501", "expected_error": "Not implemented: requires provider-session/codex-login", "assert_no_write": True},
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
    ("POST", "/api/answer"): {"primary": {"body": {"id": "$question", "decision": "cancel"}, "expected": "501", "assert_no_write": True}},
    ("POST", "/api/branch"): {"primary": {"body": {"agent": "$agent", "id": "ledger-branch", "message_id": "ledger-message"}, "expected": "501", "assert_no_write": True}},
    ("POST", "/api/capacity-retry"): {
        "primary": {"body": {"id": "$agent", "retry_id": "ledger-capacity-cancel", "action": "cancel"}, "expected": "2xx"},
        "additional": [{"name": "provider turn retry", "body": {"id": "$agent", "retry_id": "ledger-capacity-retry", "action": "retry"}, "expected": "501", "expected_error": "Not implemented: requires turn-driver/capacity-retry", "assert_no_write": True}],
    },
    ("POST", "/api/checkpoint/restore"): {"primary": {"body": {"agent": "$agent", "checkpoint": "$checkpoint", "expectedTree": "0000000000000000000000000000000000000000"}, "expected": "501", "assert_no_write": True}},
    ("POST", "/api/claude/profiles"): {"primary": {"body": {}, "expected": "501", "assert_no_write": True}},
    ("POST", "/api/conversation/delete"): {"primary": {"body": {"id": "$agent"}, "expected": "501", "assert_no_write": True}},
    ("POST", "/api/federation/v1/message"): {"primary": {"body": {"protocol": 1, "envelope": {}}, "expected": "501", "assert_no_write": True}},
    ("POST", "/api/federation/v1/pair"): {"primary": {"body": {"protocol": 1, "inviteId": "fixture-invite", "token": "fixture-token", "clientId": "fixture-client", "label": "Ledger", "kind": "ui", "publicKey": "fixture-key", "requestId": "ledger-pair"}, "expected": "501", "assert_no_write": True}},
    ("POST", "/api/federation/v1/pull"): {"primary": {"body": {"protocol": 1, "serverId": "local", "cursor": 0}, "expected": "501", "assert_no_write": True}},
    ("POST", "/api/federation/v1/status"): {"primary": {"body": {"protocol": 1, "serverId": "local", "requestId": "ledger-status"}, "expected": "501", "assert_no_write": True}},
    ("POST", "/api/multi-server/v1/auto-pair"): {"primary": {"body": {"enabled": True}, "expected": "501", "assert_no_write": True}},
    ("POST", "/api/multi-server/v1/name"): {"primary": {"body": {"requestId": "ledger-name", "label": "Ledger"}, "expected": "501", "assert_no_write": True}},
    ("POST", "/api/multi-server/v1/pair"): {"primary": {"body": {"protocol": 1, "inviteId": "fixture-invite", "token": "fixture-token", "clientId": "fixture-client", "label": "Ledger", "kind": "ui", "publicKey": "fixture-key", "requestId": "ledger-pair"}, "expected": "501", "assert_no_write": True}},
    ("POST", "/api/organization"): {"primary": {"body": {"id": "$agent", "pinned": True}, "expected": "501", "assert_no_write": True}},
    ("POST", "/api/room/delete"): {"primary": {"body": {"id": "$room"}, "expected": "501", "assert_no_write": True}},
    ("POST", "/api/servers/orchestration"): {"primary": {"body": {"serverId": "local", "payload": {}}, "expected": "501", "assert_no_write": True}},
    ("POST", "/api/terminals/close"): {"primary": {"body": {"id": "$terminal"}, "expected": "501", "assert_no_write": True}},
    ("POST", "/api/terminals/create"): {"primary": {"body": {"id": "ledger-terminal", "agent": "$agent"}, "expected": "501", "assert_no_write": True}},
    ("POST", "/api/terminals/input"): {"primary": {"body": {"id": "$terminal", "text": "x", "request_id": "ledger-input"}, "expected": "501", "assert_no_write": True}},
    ("POST", "/api/terminals/rename"): {"primary": {"body": {"id": "$terminal", "title": "Ledger"}, "expected": "501", "assert_no_write": True}},
    ("POST", "/api/terminals/resize"): {"primary": {"body": {"id": "$terminal", "cols": 80, "rows": 24}, "expected": "501", "assert_no_write": True}},
    ("POST", "/api/usage-resume"): {
        "primary": {"body": {"id": "$agent", "resume_id": "ledger-resume-disable", "enabled": False}, "expected": "2xx"},
        "additional": [{"name": "provider turn resume", "body": {"id": "$agent", "resume_id": "ledger-resume-enable", "enabled": True}, "expected": "501", "expected_error": "Not implemented: requires turn-driver/usage-resume", "assert_no_write": True}],
    },
    ("POST", "/api/voice/approve"): {"primary": {"body": {"agent": "$agent", "session_id": "$voiceActiveSession", "speech_id": "ledger-speech", "transcript_id": "ledger-transcript"}, "expected": "501", "assert_no_write": True}},
    ("POST", "/api/voice/audio"): {"primary": {"body": {"agent": "$agent", "session_id": "$voiceActiveSession", "chunk_id": "ledger-audio", "audio": "", "mime": "audio/webm"}, "expected": "501", "assert_no_write": True}},
    ("POST", "/api/voice/start"): {"primary": {"body": {"agent": "$agent", "session_id": "ledger-voice-start", "sdp": "v=0\\r\\n"}, "expected": "501", "assert_no_write": True}},
    ("POST", "/api/agents"): {
        "primary": {"body": {"id": "66666666-6666-4666-8666-666666666666", "prompt": "Ledger agent", "cwd": "$projectPath"}, "expected": "2xx"},
        "additional": [{"name": "worker assignment dependency", "body": {"id": "66666666-6666-4666-8666-666666666667", "prompt": "Ledger worker dependency", "cwd": "$projectPath", "parent": "$agent"}, "expected": "501", "expected_error": "Not implemented: requires worker-driver/workspace-preparation", "assert_no_write": True}],
    },
    ("POST", "/api/assets"): {
        "primary": {"body": {"agent": "$agent", "name": "ledger.txt", "base64": "bGVkZ2Vy"}, "expected": "2xx"},
    },
    ("POST", "/api/checkpoint"): {
        "primary": {"body": {"agent": "$imageAgent", "label": "ledger checkpoint"}, "expected": "501", "expected_error": "Not implemented: requires workspace-state/image-execution", "assert_no_write": True},
        "additional": [{"name": "lead checkpoint shape", "body": {"agent": "$agent", "label": "ledger checkpoint alternate"}, "fixture_mutations": {"agent_name": "HTTP ledger probe owner"}, "restore": ["agent_record"], "expected": "2xx"}],
    },
    ("POST", "/api/checkpoint/preview"): {
        "primary": {"body": {"agent": "$imageAgent", "checkpoint": "$imageCheckpoint"}, "expected": "501", "expected_error": "Not implemented: requires workspace-state/image-execution", "assert_no_write": True},
        "additional": [{"name": "lead preview shape", "body": {"agent": "$agent", "checkpoint": "$checkpoint"}, "expected": "2xx"}],
    },
    ("POST", "/api/configure"): {
        "primary": {"body": {"id": "$agent", "maxAgents": 10}, "fixture_mutations": {"agent_name": "HTTP ledger probe owner"}, "restore": ["agent_record"], "expected": "2xx"},
        "additional": [{"name": "agent mode settings dependency", "body": {"id": "$agent", "concurrency": 0}, "expected": "501", "expected_error": "Not implemented: requires agent-management/agent-mode-settings", "assert_no_write": True}],
    },
    ("POST", "/api/conversation"): {
        "primary": {"body": {"id": "$agent", "excluded_account_keys": ["other-account"]}, "fixture_mutations": {"agent_name": "HTTP ledger probe owner"}, "restore": ["agent_record"], "expected": "2xx"},
        "additional": [{"name": "conversation settings dependency", "body": {"id": "$agent", "agent_mode": "single"}, "expected": "501", "expected_error": "Not implemented: requires agent-management/conversation-settings", "assert_no_write": True}],
    },
    ("POST", "/api/leads"): {
        "primary": {"body": {"id": "77777777-7777-4777-8777-777777777777", "cwd": "$projectPath", "workspaceMode": "worktree"}, "replace_body": True, "expected": "2xx"},
        "additional": [{"name": "lead model catalog dependency", "body": {"id": "77777777-7777-4777-8777-777777777778", "cwd": "$projectPath", "workspaceMode": "worktree", "model": "claude-sonnet-4-5"}, "replace_body": True, "expected": "501", "expected_error": "Not implemented: requires provider-session/model-list", "assert_no_write": True}],
    },
    ("POST", "/api/voice/approval_speech"): {
        "primary": {"body": {"agent": "$agent", "request_id": "$approvalRequest"}, "replace_body": True, "expected": "2xx"},
    },
    ("POST", "/api/voice/approvals"): {"primary": {"body": {"agent": "$agent"}, "expected": "2xx"}},
    ("POST", "/api/voice/record"): {
        "primary": {"body": {"agent": "$agent", "session_id": "$voiceEndedSession", "event_id": "ledger-event", "kind": "event", "text": "ledger"}, "replace_body": True, "expected": "2xx"},
    },
    ("POST", "/api/voice/records"): {"primary": {"body": {"agent": "$agent"}, "expected": "2xx"}},
    ("POST", "/api/voice/status"): {"primary": {"body": {"agent": "$agent"}, "expected": "2xx"}},
    ("POST", "/api/voice/submit"): {
        "primary": {"body": {"agent": "$agent", "message_id": "ledger-message", "record_ids": ["$voiceRecord"]}, "expected": "2xx"},
        "additional": [{"name": "missing transcript records", "body": {"agent": "$agent", "message_id": "ledger-message-missing-records", "record_ids": ["ledger-missing-voice-record"]}, "expected": "400", "assert_no_write": True}],
    },
    ("POST", "/api/voice/end"): {
        "primary": {"body": {"agent": "$agent", "session_id": "$voiceEndedSession"}, "expected": "2xx"},
        "additional": [{"name": "active voice session dependency", "body": {"agent": "$agent", "session_id": "$voiceActiveSession"}, "expected": "501", "expected_error": "Not implemented: requires provider-session/voice", "assert_no_write": True}],
    },
    ("POST", "/api/voice/speech"): {
        "primary": {"body": {"agent": "$agent", "record_id": "$voiceRecord", "session_id": "$voiceEndedSession"}, "expected": "2xx"},
        "additional": [{"name": "provider-backed voice speech", "body": {"agent": "$agent", "record_id": "$voiceRecord", "session_id": "$voiceActiveSession"}, "expected": "501", "expected_error": "Not implemented: requires provider-session/voice", "assert_no_write": True}],
    },
    ("POST", "/api/rules"): {
        "primary": {"body": {
            "agent": "$agent", "name": "HTTP ledger probe rule", "kind": "interval",
            "command": "echo ledger probe", "text": "Check the probe fixture.",
        }, "expected": "2xx"},
        "additional": [{
            "name": "active-check dependency",
            "body": {"agent": "$agent", "action": "pause", "id": "$activeRule"},
            "expected": "501",
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


def rust_operation_manifest() -> dict[tuple[str, str], dict[str, Any]]:
    process = subprocess.run(
        [
            "cargo",
            "test",
            "--locked",
            "-p",
            "studio-runtime-http",
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
    """Generate only unambiguous probes; partial routes require explicit cases."""
    for key, entry in manifest.items():
        status = entry.get("status")
        spec = PROBE_CASES.setdefault(key, {})
        if status == "unimplemented":
            # The manifest state has one exact contract for schema-valid input.
            spec.setdefault("primary", {}).setdefault("expected", "501")
            continue
        if status not in {"implemented", "partial"}:
            continue
        primary = spec.setdefault("primary", {})
        if status == "implemented":
            primary.setdefault("expected", "2xx")
        elif not primary.get("expected"):
            primary["unverified"] = True


def seed_python_probe_state(backend: Any) -> dict[str, str]:
    """Seed the disposable backend with records written by the Python runtime."""
    agent_id = "ledger-probe-agent"
    image_agent_id = "ledger-probe-image-agent"
    task_id = "ledger-probe-task"
    work_id = "ledger-probe-work"
    complaint_id = "ledger-probe-complaint"
    room_id = "ledger-probe-room"
    checkpoint_id = "ledger-probe-checkpoint"
    item_id = "ledger-probe-item"
    monitor_id = "ledger-probe-monitor"
    terminal_id = "ledger-probe-terminal"
    active_rule_id = "ledger-probe-active-rule"
    active_monitor_id = "ledger-probe-active-rule-monitor"
    chat_id = "55555555-5555-4555-8555-555555555555"
    message_id = "66666666-6666-4666-8666-666666666666"
    file_path = "ledger-probe.txt"
    source = Path(__file__).resolve().parents[1] / "src"
    seed = f"""
import os, subprocess, sys, time
from pathlib import Path
sys.path.insert(0, {str(source)!r})
from codex_runtime import Runtime
from codex_canvas import Canvas

runtime = Runtime(Path(os.environ["CODEX_AGENTS_STATE_DIR"]))
try:
    agent = {{
        "id": {agent_id!r}, "rootId": {agent_id!r}, "epoch": 0,
        "status": "idle", "name": "HTTP ledger probe owner", "role": "orchestrator",
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
            "id": "ledger-probe-provider-question", "agent": {agent_id!r}, "epoch": 0,
            "method": "item/tool/requestUserInput", "status": "pending", "createdAt": 1.0,
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
    image_checkpoint = runtime.capture_checkpoint({image_agent_id!r}, "HTTP ledger image checkpoint")
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
        {agent_id!r}, {{"command": "echo active rule probe", "stall_timeout_seconds": 0}},
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
            "checkpoint": checkpoint["id"], "imageCheckpoint": image_checkpoint["id"], "monitor": monitor["id"],
            "activeMonitor": active_monitor["id"],
            }}), encoding="utf-8"
        )
    finally:
        manager.close()
finally:
    runtime.close()
"""
    python = os.environ.get("CODEX_AGENTS_PYTHON", sys.executable)
    subprocess.run(
        [python, "-B", "-c", seed],
        cwd=backend.cwd,
        env=backend._environment(),
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
        (backend.state / "ledger-probe-identifiers.json").read_text(encoding="utf-8")
    )
    terminal_id = terminal_identifiers["terminal"]
    return {
        "agent": agent_id,
        "imageAgent": image_agent_id,
        "task": task_id,
        "complaint": complaint_id,
        "room": room_id,
        "checkpoint": terminal_identifiers["checkpoint"],
        "item": f"{agent_id}:{item_id}",
        "monitor": terminal_identifiers["monitor"],
        "terminal": terminal_id,
        "queueIds": [],
        "activeRule": active_rule_id,
        "question": "ledger-probe-question",
        "questionDelete": "ledger-probe-question",
        "providerQuestion": "ledger-probe-provider-question",
        "voiceRecord": "ledger-probe-voice-record",
        "voiceEndedSession": "ledger-probe-voice-ended",
        "voiceActiveSession": "ledger-probe-voice-active",
        "approvalRequest": "ledger-probe-monitor-approval",
        "imageCheckpoint": terminal_identifiers["imageCheckpoint"],
        "schemaHash": backend.schema_hash,
        "analyticsAgent": "analytics-contract-agent",
        "chat": chat_id,
        "message": message_id,
        "filePath": file_path,
        "layoutRevision": layout_revision,
        "emptyQueueRevision": terminal_identifiers["emptyQueueRevision"],
        "projectPath": str(backend.cwd),
        "claudeAccount": "claude-ledger",
    }


def seed_post_start_voice_session(backend: Any, agent_id: str) -> str:
    """Use the Python voice writer after startup, when a live session can exist."""
    source = Path(__file__).resolve().parents[1] / "src"
    session_id = "ledger-probe-voice-active"
    script = f"""
import os, sys, threading
from pathlib import Path
sys.path.insert(0, {str(source)!r})
from codex_runtime import Runtime
from codex_voice import VoiceStore
runtime = Runtime(Path(os.environ["CODEX_AGENTS_STATE_DIR"]))
try:
    voice = object.__new__(VoiceStore)
    voice.runtime = runtime
    voice.native_lock = threading.RLock()
    voice.connections = {{}}
    voice._publish_voice = lambda _agent: None
    voice._start_native = lambda _agent, _session, _sdp: None
    voice.start({agent_id!r}, {session_id!r}, "v=0\\r\\n")
    voice._state({session_id!r}, "ready", publish=False)
finally:
    runtime.close()
"""
    environment = backend._environment()
    python = os.environ.get("CODEX_AGENTS_PYTHON", sys.executable)
    subprocess.run(
        [python, "-B", "-c", script], cwd=backend.cwd, env=environment,
        check=True, timeout=30,
    )
    return session_id


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

    agent_record: dict[str, Any] | None = None
    identity_file: tuple[Path, bytes, int] | None = None
    session_cost_cache: tuple[Path, bytes] | None = None
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
            identity_file = (path, path.read_bytes(), path.stat().st_mode & 0o777)
            path.unlink()
        if mutations.get("stale_session_cost_cache"):
            cache_files = sorted((backend.state / "session-costs").glob("cost-*.json"))
            if not cache_files:
                raise RuntimeError("seeded session-cost cache is missing")
            path = cache_files[0]
            session_cost_cache = (path, path.read_bytes())
            cache = json.loads(session_cost_cache[1])
            cache["source"]["usage"]["generation"] += 1
            path.write_text(json.dumps(cache), encoding="utf-8")
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


def probe_operations(
    manifest: dict[tuple[str, str], dict[str, Any]],
    planned_assertions: dict[str, set[str]],
    successful_assertions: dict[str, set[str]],
) -> list[dict[str, Any]]:
    sys.path.insert(0, str(TESTS))
    from http_backend_harness import BackendProcess, HttpResult

    with tempfile.TemporaryDirectory(
        prefix="rust-http-ledger-", dir=DEFAULT_CACHE
    ) as scratch_name:
        scratch = Path(scratch_name).resolve(strict=True)
        backend = BackendProcess(scratch, claude_bin=None)
        try:
            backend.create_codex_replay("restart-mid-turn-supervisor-replay.json")
            seed_catalog_state(backend)
            import importlib.util
            analytics_path = TESTS / "rust-analytics-http-contract.py"
            analytics_spec = importlib.util.spec_from_file_location("ledger_analytics_seed", analytics_path)
            if analytics_spec is None or analytics_spec.loader is None:
                raise RuntimeError("could not load analytics contract seed")
            analytics_module = importlib.util.module_from_spec(analytics_spec)
            analytics_spec.loader.exec_module(analytics_module)
            analytics_module.seed_analytics(backend)
            identifiers = seed_python_probe_state(backend)
            backend.start()
            identifiers["voiceActiveSession"] = seed_post_start_voice_session(
                backend, identifiers["agent"]
            )
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
                probe_spec = PROBE_CASES.get((method, template), {})
                primary_case = probe_spec.get("primary", {})
                cases = [("primary", primary_case)] + [
                    (case.get("name", f"additional-{index}"), case)
                    for index, case in enumerate(probe_spec.get("additional", []), start=1)
                ]
                observations = []
                for case_name, probe_case in cases:
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
                    database_before = (
                        sqlite_snapshot(backend.state / "canvas.sqlite3")
                        if probe_case.get("assert_no_write") else None
                    )
                    with probe_fixture_state(backend, identifiers, probe_case):
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
                    operation_key = f"{method} {template}"
                    probe_event = {
                        "operation": operation_key,
                        "body": response_body,
                        "expected_error": probe_case.get("expected_error"),
                    }
                    if probe_case.get("wait_for_success") and not 200 <= result.status < 300:
                        deadline = time.monotonic() + 5
                        while time.monotonic() < deadline and not 200 <= result.status < 300:
                            time.sleep(0.05)
                            result = backend.request(method, path, headers=headers, timeout=2.0)
                            try:
                                response_body = json.loads(result.body)
                            except (ValueError, TypeError):
                                response_body = None
                        probe_event["body"] = response_body
                    standard_501 = result.status != 501 or valid_501_envelope(probe_event, manifest)
                    no_write = (
                        database_before is None
                        or database_before == sqlite_snapshot(backend.state / "canvas.sqlite3")
                    )
                    expected = probe_case.get("expected")
                    expected_matches = (
                        (expected == "501" and result.status == 501 and standard_501)
                        or (implementation == "unimplemented" and result.status == 501 and standard_501)
                        or (expected == "404" and result.status == 404 and response_body == {"error": "Not found"})
                        or (expected == "400" and result.status == 400)
                        or (expected == "2xx" and 200 <= result.status < 300)
                    )
                    observations.append({
                        "case": case_name,
                        "status": result.status,
                        "expected": expected,
                        "expectedMatches": expected_matches,
                        "body": response_body,
                        "standard501Envelope": standard_501,
                        "noWrite": no_write,
                        **({"reason": probe_case["reason"]} if "reason" in probe_case else {}),
                    })
                primary = observations[0]
                schema_header = result.headers.get("x-studio-api-schema")
                # The final response may belong to an additional probe case;
                # the schema header is invariant across the operation's cases.
                schema_header_matches = schema_header == backend.schema_hash
                status = primary["status"]
                operation_key = f"{method} {template}"
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
                if not all(observation["expectedMatches"] for observation in observations):
                    classification = "unexpected"
                if implementation == "partial" and any(
                    case.get("unverified") or not case.get("expected")
                    for _, case in cases
                ):
                    classification = "unverified"
                if not all(observation["noWrite"] for observation in observations):
                    classification = "unexpected"
                rows.append({
                    "method": method,
                    "path": template,
                    "status": status,
                    "implementation": implementation,
                    "observedStatus": status,
                    "classification": classification,
                    "covered": bool(successful_assertions.get(operation_key)),
                    "expectedStatus": (
                        "not_501"
                        if implementation == "implemented"
                        else "any"
                        if implementation == "partial"
                        else 501
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


def seed_catalog_state(backend: Any) -> None:
    accounts = backend.state / "accounts"
    accounts.mkdir(parents=True)
    (accounts / "registry.json").write_text(json.dumps({
        "version": 1,
        "defaultAccountKey": "default",
        "accounts": {
            "default": {
                "id": "default", "home": str(backend.codex_home), "label": "Codex",
                "source": "Codex CLI", "provider": "codex", "accountId": "fixture-account",
                "status": "ready",
            },
            "claude-ledger": {
                "id": "claude-ledger", "home": str(backend.codex_home), "label": "Claude probe",
                "source": "ledger fixture", "provider": "claude", "accountId": "claude:ledger",
                "status": "ready",
            },
        },
        "logins": {},
    }), encoding="utf-8")
    python = os.environ.get("CODEX_AGENTS_PYTHON")
    if not python:
        raise RuntimeError("CODEX_AGENTS_PYTHON must name the isolated Python interpreter")
    environment = backend._environment()
    environment["PYTHONPATH"] = os.pathsep.join(filter(None, (
        str(ROOT / "workspaces/runtime/apps/server/src"), environment.get("PYTHONPATH", ""),
    )))
    subprocess.run(
        [python, "-c", "from codex_runtime import Runtime; import sys; Runtime(sys.argv[1]).close()", str(backend.state)],
        check=True,
        env=environment,
        timeout=30,
    )


def case_results(log_text: str) -> list[dict[str, Any]]:
    cases: list[dict[str, Any]] = []
    current: str | None = None
    for line in log_text.splitlines():
        start = re.match(r"^(test_[\w]+)\s+\(([^)]*)\)\s+\.\.\.\s*(.*)$", line)
        if start:
            current = start.group(1)
            full_id = start.group(2)
            tail = start.group(3).strip()
            if tail in {"ok", "FAIL", "ERROR"} or tail.startswith("skipped"):
                cases.append({"test": current, "testId": full_id, "status": tail.split()[0].lower()})
                current = None
            continue
        if current is not None and line.strip() in {"ok", "FAIL", "ERROR"}:
            cases.append({"test": current, "testId": full_id, "status": line.strip().lower()})
            current = None
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
    suite_environment = environment.copy()
    suite_environment["STUDIO_HTTP_LEDGER_TRACE"] = str(trace_path)
    skipped_for_service = MISSING_SERVICE_TESTS.get(relative, {})
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
    if actual_skips & required_skips != required_skips:
        raise RuntimeError(
            f"missing-service test skip mismatch in {relative}: "
            f"expected={sorted(required_skips)}, observed={sorted(actual_skips & required_skips)}"
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
        "status": result.get("status", "failed"),
        "passed": result.get("passed") or 0,
        "failed": result.get("failed") or 0,
        "skipped": result.get("skipped") or 0,
        "exitCode": result.get("exitCode", process.returncode),
        "tests": cases,
        "requiresMissingServices": [
            {"testId": test_id, **details}
            for test_id, details in sorted(skipped_for_service.items())
        ],
        "failures": failures,
        "requests": requests,
        "unattributedRequests": unattributed,
        **({"diagnostic": result["diagnostic"]} if "diagnostic" in result else {}),
    }


def sqlite_snapshot(path: Path) -> tuple[Any, ...]:
    with sqlite3.connect(f"file:{path}?mode=ro", uri=True) as connection:
        tables = connection.execute(
            "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name"
        ).fetchall()
        return tuple(
            (
                table[0],
                tuple(sorted(connection.execute(f'SELECT * FROM "{table[0]}"').fetchall(), key=repr)),
            )
            for table in tables
        )


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
    options = parser.parse_args()
    command = os.environ.get("STUDIO_HTTP_BACKEND_COMMAND")
    if not command:
        parser.error("STUDIO_HTTP_BACKEND_COMMAND must name the Rust server executable")

    environment = os.environ.copy()
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
    )
    if differential.returncode:
        raise RuntimeError("HTTP extractor differential contract failed")
    trace_regression = subprocess.run(
        [sys.executable, str(TESTS / "http_runtime_conformance_ledger_test.py")],
        cwd=ROOT, env=environment, check=False,
    )
    if trace_regression.returncode:
        raise RuntimeError("HTTP ledger error-body trace regression failed")
    routes = operations()
    manifest = rust_operation_manifest()
    advertised_services = {
        dependency
        for entry in manifest.values()
        for dependency in entry.get("dependencies", [])
    }
    stale_exclusions = sorted({
        details["service"]
        for suite in MISSING_SERVICE_TESTS.values()
        for details in suite.values()
        if details["service"] in advertised_services
    })
    if stale_exclusions:
        raise RuntimeError(
            "ledger has stale missing-service exclusions; service appears in the Rust manifest: "
            + ", ".join(stale_exclusions)
        )
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
    operations_result = probe_operations(manifest, planned_assertions, successful_assertions)
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
            len(row.get("requiresMissingServices", [])) for row in suites
        ),
        "failedTestsWithout501": len(failures_without_501),
        "standard501EnvelopeMismatches": len(envelope_mismatches),
    }
    ledger = {
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
