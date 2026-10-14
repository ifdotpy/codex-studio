#!/usr/bin/env python3
"""Census the Rust HTTP route manifest and prove the host lifecycle contract."""
from __future__ import annotations

import argparse
import importlib.util
import json
import os
from pathlib import Path
import sys
import unittest
from unittest.mock import patch
from typing import Any

from test_isolation import isolate_supervisor_environment

isolate_supervisor_environment()

ROOT = Path(__file__).resolve().parents[5]
TESTS = Path(__file__).resolve().parent
CORPUS = TESTS / "wave7-p20-cutover-corpus.json"
sys.path.insert(0, str(TESTS))
sys.path.insert(0, str(TESTS.parent / "src"))

_ledger_spec = importlib.util.spec_from_file_location(
    "http_runtime_conformance_ledger",
    TESTS / "http_runtime_conformance_ledger.py",
)
assert _ledger_spec and _ledger_spec.loader
ledger = importlib.util.module_from_spec(_ledger_spec)
sys.modules[_ledger_spec.name] = ledger
_ledger_spec.loader.exec_module(ledger)


ACCEPTED_BRANCHES = {
    "P01": "origin/feat/wave7-p01-runtime-composition",
    "P03": "origin/feat/wave7-p03-message-ingress",
    "P04": "origin/feat/wave7-p04-callback-runtime",
    "P06": "origin/feat/wave7-p06-notification-effects",
    "P07": "origin/feat/wave7-p07-recovery-runtime",
    "P08": "origin/feat/wave7-p08-claude-runtime",
    "P09": "origin/feat/wave7-p09-budget-runtime",
    "P10": "origin/feat/wave7-p10-stop-delete",
    "P11": "origin/feat/wave7-p11-native-actions",
    "P13": "origin/feat/wave7-p13-transcript-delivery",
    "P14": "origin/feat/wave7-p14-sync-resources",
    "P15": "origin/feat/wave7-p15-monitors-rules",
    "P16": "origin/feat/wave7-p16-work-assignment",
    "P17": "origin/feat/wave7-p17-terminals",
    "P18": "origin/feat/wave7-p18-account-catalog",
    "P19": "origin/feat/wave7-p19-projects-radio",
}
OPEN_TASKS = {
    "P02": "Wave 7 P02 Host creation/configuration",
    "P05": "Wave 7 P05 Managed host tools",
    "P12": "Wave 7 P12 Completion/continuations",
    "direct provider launch": "Direct provider launch runtime composition",
}

# The route dependency is the unit of evidence. Each accepted branch or open
# package which closes it is retained in the emitted census, including
# dependencies that stay explicit first-cutover refusals.
EXACT_OWNERS: dict[str, list[str]] = {
    "account-driver/unavailable": ["P01", "P02", "P18"],
    "account-driver/reset-credit-redemption": ["R3"],
    "account-worker/agent-interrupt": ["P10"],
    "account-worker/deleted-settlement": ["P10"],
    "account-worker/native-action": ["P11"],
    "account-worker/recovery-history": ["P07"],
    "account-worker/voice-session": ["R2"],
    "account-transfer/usage-resume-cancellation": ["P12", "P18"],
    "account-transfer/provider-history-executor": ["P07", "P18"],
    "account-transfer/destination-settings": ["P18"],
    "store-bootstrap/account-transfer-schema": ["P18"],
    "agent-management/agent-mode-settings": ["P02"],
    "agent-management/conversation-settings": ["P02"],
    "agent-management/conversation-termination": ["P10"],
    "agent-management/empty-chat-reuse": ["P02"],
    "agent-management/snapshot-limit": ["P10"],
    "budget-policy/message-admission": ["P03", "P09"],
    "capacity-retry/store": ["P03", "P12"],
    "coordination-policy/chat-history": ["P03"],
    "coordination-policy/room-visibility": ["P03"],
    "context-repair/native-history-rewrite": ["P11"],
    "store-bootstrap/context-repair-schema": ["P11"],
    "cross-server/agent-management": ["R1", "P10"],
    "cross-server/native-review": ["P11"],
    # The wave 7 plan explicitly excludes the macOS vault adapter; the
    # registered route keeps that platform dependency visible in the census.
    "macos-platform/layr-desk": ["NEW macOS layr-desk adapter (out of scope)"],
    "cross-server/remote-events": ["R1"],
    "cross-server-driver/local-identity": ["P19", "R1"],
    "cross-server-driver/project-binding": ["P19"],
    "cross-server-driver/peer-team-transport": ["R1", "P19"],
    "cross-server-driver/project-location-read": ["P19", "R1"],
    "cross-server-driver/project-location-write": ["P19", "R1"],
    "cross-server-driver/project-registration": ["P19"],
    "host-project/radio-create": ["P19"],
    "event-driver/child-stop-notification": ["P06", "P10"],
    "event-driver/message-receipts": ["P03", "P06", "P13"],
    "event-driver/queue-mutations": ["P12"],
    "federation-credentials/invitation": ["R1"],
    "federation-credentials/key-generation": ["R1"],
    "federation-credentials/private-key": ["R1"],
    "federation-driver/rooms": ["R1", "P03"],
    "federation/trusted-origin-observation": ["R1"],
    "monitor-driver/active-checks": ["P15"],
    "monitor-driver/agent-stop": ["P10", "P15"],
    "payload-store/tool-request-recovery": ["P04", "P05"],
    "payload-store/uploads": ["P03"],
    "payload-store/voice-audio": ["R2"],
    "project-driver/chat-binding": ["P19", "P02"],
    "project-driver/folder-selection": ["P02"],
    "provider-service/claude-discovery": ["P18"],
    "provider-session/account-rate-limits": ["P18"],
    "provider-session/app-server": ["P01", "P14", "P17"],
    "provider-session/account-session": ["P18"],
    "provider-session/claude-controls": ["P08"],
    "provider-session/claude-discovery": ["P18"],
    "provider-session/claude-login": ["P18"],
    "provider-session/claude-profile-auth": ["P18"],
    "provider-session/codex-login": ["P18"],
    "provider-session/model-list": ["P18", "P02"],
    "provider-session/monitor-commands": ["P15"],
    "provider-session/question-answer": ["P04"],
    "provider-session/thread-fork": ["P11"],
    "provider-session/thread-import": ["first-cutover refusal"],
    "provider-session/title-generation": ["R4"],
    "provider-session/voice": ["R2"],
    "question-answer/async-question": ["P04", "P05"],
    "question-answer/browser-open": ["P04", "P05", "R1"],
    "question-answer/monitor-approval": ["P04", "P15"],
    "remote-transport/identity-write": ["R1"],
    "remote-transport/owner-lookup": ["R1"],
    "remote-transport/signed-federation-exchange": ["R1"],
    "remote-transport/signed-server-requests": ["R1"],
    "remote-transport/tailscale-discovery": ["R1"],
    "remote-transport/tailscale-identity": ["R1"],
    "rule-driver/monitor-finish": ["P15"],
    "server-credentials/local-identity": ["R1"],
    "store-bootstrap/capacity-retry-schema": ["P12"],
    "store-bootstrap/tool-request-schema": ["P04", "P05"],
    "store-bootstrap/usage-resume-schema": ["P12"],
    "transcript/event-driver": ["P06", "P13"],
    "transcript/payload-store": ["P13"],
    "transcript/receipt-page": ["P13"],
    "transcript/reasoning-page": ["P13"],
    "transcript/search-index": ["P13"],
    "transcript-service/pull": ["P13"],
    "transcript/workspace-state": ["P13"],
    "turn-driver/capacity-retry": ["P12"],
    "turn-driver/usage-resume": ["P12"],
    "turn-path/native-command-input": ["P11", "P18"],
    "usage-policy/agent-stop": ["P09", "P10"],
    "usage-policy/session-cost-refresh": ["P09"],
    "usage-policy/token-rates": ["P06", "P14"],
    "voice-driver/agent-stop": ["R2", "P10"],
    "vm-agents/finder-view": ["first-cutover refusal: VM excluded"],
    "worker-driver/settings-catalog": ["P18", "P02"],
    "worker-driver/work-assignment": ["P16", "P02"],
    "worker-driver/workspace-preparation": ["P02"],
    "work-driver/deletion-release": ["P10", "P16"],
    "workspace-policy/message-admission": ["P03"],
    "workspace-state/agent-lifecycle": ["P10"],
    "workspace-state/image-execution": ["first-cutover refusal: image excluded"],
    "workspace-state/image-workspace": ["first-cutover refusal: image workspace excluded"],
    "workspace-state/lead-workspace": ["P02"],
    "workspace-state/organization-effects": ["P16"],
    "workspace-state/peer-radio": ["P19"],
    "workspace-state/vm-workspace": ["first-cutover refusal: VM workspace excluded"],
    "workspace-state/worker-environment": ["P19"],
    "multi-server-runtime/orchestration": ["R1"],
    "multi-server-runtime/pairing": ["R1"],
    "multi-server/move-receipt": ["R1"],
    "multi-server/project-location-receipt": ["R1"],
    "multi-server/discovery-pairing": ["R1"],
    "multi-server/folder-browse": ["R1"],
    "thread-preparation/tool-catalog": ["P02", "P18"],
    "costs/local-scanner": ["R5"],
    "store-bootstrap/analytics-usage-schema": ["P09", "R5"],
    "costs/pricing-refresh": ["P09", "R5"],
    "costs/claude-local-usage": ["P09", "R5"],
    "runtime-process/process-tree": ["R5"],
    "runtime-process/host-resources": ["R5"],
    "runtime-process/resource-attribution": ["R5"],
    "provider-session/native-accounts": ["P18", "R5"],
    "runtime-process/studio-loaded-threads": ["R5"],
    "runtime-process/live-queues": ["R5"],
    "runtime-process/lock-samples": ["R5"],
    "runtime-store/sqlite-contention": ["R5"],
    "runtime-http/request-metrics": ["R5"],
    "runtime-process/analytics-capture": ["R5"],
    "runtime-process/supervisor-status": ["P01", "R5"],
    "thread-preparation/tool-catalog": ["P02", "P18"],
    "question-answer/monitor-approval": ["P04", "P15"],
    "provider-session/claude-controls": ["P08"],
}

UNREGISTERED_BY_PATH: dict[str, list[str]] = {
    "/api/vault/aws-login": ["NEW vault/runtime-service (out of scope)"],
    "/api/vault/pull": ["NEW vault/runtime-service (out of scope)"],
    "/api/vault/setup": ["NEW vault/runtime-service (out of scope)"],
    "/api/vault/setup/facts": ["NEW vault/runtime-service (out of scope)"],
    "/api/vault/unlock": ["NEW vault/runtime-service (out of scope)"],
    "/api/project-secrets/list": ["NEW project-secrets/runtime-service (out of scope)"],
    "/api/project-secrets/remove": ["NEW project-secrets/runtime-service (out of scope)"],
    "/api/project-secrets/set": ["NEW project-secrets/runtime-service (out of scope)"],
    "/api/linux-vm/settings": ["NEW linux-vm/settings-service (out of scope)"],
    "/api/mac-sync/config": ["NEW mac-sync/runtime-service (out of scope)"],
    "/api/mac-sync/resolve": ["NEW mac-sync/runtime-service (out of scope)"],
    "/api/monitor/cancel": ["P15", "R1"],
    "/api/monitor/input": ["P15"],
    "/api/action": ["P10", "P11"],
    "/api/stop": ["P10"],
    "/api/conversation/delete": ["P10"],
    "/api/agents/account": ["P12", "P18"],
    "/api/agents/account-transfer": ["P07", "P18"],
    "/api/claude/session": ["P08"],
    "/api/connection-recovery": ["P07"],
    "/api/context-repair": ["P11"],
    "/api/native-command": ["P11"],
    "/api/sync/notify": ["P14"],
    "/api/limits/reset": ["R3"],
    "/api/voice/approve": ["R2"],
    "/api/voice/audio": ["R2"],
    "/api/voice/end": ["R2"],
    "/api/voice/speech": ["R2"],
    "/api/voice/start": ["R2"],
    "/api/projects": ["P19", "R1"],
    "/api/organization": ["P16"],
    "/api/peer-teams": ["P19", "R1"],
    "/api/terminals/close": ["P17"],
    "/api/terminals/create": ["P17"],
    "/api/terminals/input": ["P17"],
    "/api/terminals/rename": ["P17"],
    "/api/terminals/resize": ["P17"],
    "/api/voice/approval_speech": ["R2"],
    "/api/voice/approvals": ["R2"],
    "/api/voice/record": ["R2"],
    "/api/voice/records": ["R2"],
    "/api/voice/status": ["R2"],
    "/api/voice/submit": ["R2"],
}


def assignment_for(owner: str) -> dict[str, str]:
    if owner in ACCEPTED_BRANCHES:
        return {"owner": owner, "kind": "accepted_branch", "closes": ACCEPTED_BRANCHES[owner]}
    if owner in OPEN_TASKS:
        return {"owner": owner, "kind": "open_task", "closes": OPEN_TASKS[owner]}
    return {"owner": owner, "kind": "external_or_scope", "closes": owner}


def closure_for_dependency(name: str) -> list[dict[str, str]]:
    owners = EXACT_OWNERS.get(name)
    if owners is None:
        if name in ACCEPTED_BRANCHES or name in OPEN_TASKS:
            owners = [name]
        elif name.startswith("NEW "):
            owners = [name]
        elif name.startswith("vault/"):
            owners = ["NEW vault/runtime-service (out of scope)"]
        elif name.startswith("project-secrets/"):
            owners = ["NEW project-secrets/runtime-service (out of scope)"]
        elif name.startswith("linux-vm/"):
            owners = ["NEW linux-vm/settings-service (out of scope)"]
        elif name.startswith("mac-sync/"):
            owners = ["NEW mac-sync/runtime-service (out of scope)"]
        elif name.startswith(("remote-transport/", "federation-", "multi-server", "server-credentials/")):
            owners = ["R1"]
        elif name.startswith(("runtime-process/", "runtime-store/", "runtime-http/", "costs/")):
            owners = ["R5"]
        elif name.startswith("voice-driver/") or name.startswith("provider-session/voice"):
            owners = ["R2"]
        else:
            raise AssertionError(f"new route dependency has no P20 closure: {name}")
    return [assignment_for(owner) for owner in owners]


def build_census(manifest: dict[tuple[str, str], dict[str, Any]] | None = None) -> dict[str, Any]:
    openapi = ledger.operations()
    if manifest is None:
        cargo_home = Path(os.environ.get("CARGO_HOME", Path.home() / ".cargo"))
        cargo_bin = cargo_home / "bin"
        inherited_path = os.environ.get("PATH", os.defpath)
        with patch.dict(os.environ, {"PATH": os.pathsep.join((str(cargo_bin), inherited_path))}):
            manifest = ledger.rust_operation_manifest()
    manifest_rows = {f"{method} {path}": row for (method, path), row in manifest.items()}
    openapi_keys = {f"{method} {path}" for method, path in openapi}
    if len(openapi_keys) != len(openapi):
        raise AssertionError("OpenAPI contains duplicate operations")
    unexpected = sorted(set(manifest_rows) - openapi_keys)
    if unexpected:
        raise AssertionError(f"route manifest contains undocumented operations: {unexpected}")

    operations: list[dict[str, Any]] = []
    dependency_ops: dict[str, set[str]] = {}
    for method, path in openapi:
        key = f"{method} {path}"
        route = manifest_rows.get(key)
        if route is None:
            deps = UNREGISTERED_BY_PATH.get(path)
            if deps is None:
                raise AssertionError(f"unregistered OpenAPI operation needs a named P20 blocker: {key}")
            row = {"operation": key, "status": "not_registered", "dependencies": deps}
        else:
            status = route["status"]
            deps = sorted(route.get("dependencies", []))
            if status not in {"implemented", "partial"}:
                raise AssertionError(f"unknown route status for {key}: {status}")
            if status == "partial" and not deps:
                raise AssertionError(f"partial operation lacks named dependencies: {key}")
            if status == "implemented" and deps:
                raise AssertionError(f"implemented operation has dependencies: {key}")
            row = {"operation": key, "status": status, "dependencies": deps}
        for dependency in row["dependencies"]:
            dependency_ops.setdefault(dependency, set()).add(key)
        operations.append(row)

    all_dependencies = sorted(dependency_ops)
    closures = {name: closure_for_dependency(name) for name in all_dependencies}
    blockers = [
        {
            "dependency": name,
            "operation_count": len(dependency_ops[name]),
            "operations": sorted(dependency_ops[name]),
            "closures": closures[name],
        }
        for name in all_dependencies
    ]
    blockers.sort(key=lambda item: (-item["operation_count"], item["dependency"]))
    counts = {
        "total": len(operations),
        "implemented": sum(item["status"] == "implemented" for item in operations),
        "partial": sum(item["status"] == "partial" for item in operations),
        "not_registered": sum(item["status"] == "not_registered" for item in operations),
        "named_dependencies": len(blockers),
    }
    return {
        "schema": 1,
        "source": {
            "route_manifest": "studio-runtime-http test routes::manifest::tests::print_operation_manifest",
            "openapi": "workspaces/runtime/packages/runtime-http/openapi.json",
            "plan": "/var/tmp/codex-studio-relocation/WAVE7-PLAN.md",
        },
        "counts": counts,
        "operations": operations,
        "blockers_ordered_by_operations_unblocked": blockers,
        "lifecycle_dependencies": {
            "direct provider launch": assignment_for("direct provider launch"),
        },
    }


def route_dependencies(method: str, path: str) -> list[str]:
    key = f"{method} {path}"
    rows = {
        entry["operation"]: entry
        for entry in json.loads(CORPUS.read_text(encoding="utf-8"))["operations"]
    }
    entry = rows.get(key)
    return sorted(entry.get("dependencies", [])) if entry else []


def refresh_corpus() -> None:
    census = build_census()
    CORPUS.write_text(json.dumps(census, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    print(json.dumps({"census": str(CORPUS), "counts": census["counts"]}, sort_keys=True))


class RouteCensusContract(unittest.TestCase):
    def test_manifest_openapi_census_matches_complete_corpus(self) -> None:
        actual = build_census()
        expected = json.loads(CORPUS.read_text(encoding="utf-8"))
        self.assertEqual(actual, expected)
        self.assertEqual(actual["counts"]["total"], 160)
        self.assertEqual(actual["counts"]["implemented"], 107)
        self.assertEqual(actual["counts"]["partial"], 53)
        self.assertEqual(actual["counts"]["not_registered"], 0)

    def test_every_partial_and_unregistered_route_has_a_named_closure(self) -> None:
        census = json.loads(CORPUS.read_text(encoding="utf-8"))
        blocker_names = {item["dependency"] for item in census["blockers_ordered_by_operations_unblocked"]}
        for operation in census["operations"]:
            if operation["status"] == "implemented":
                self.assertEqual(operation["dependencies"], [], operation["operation"])
            else:
                self.assertTrue(operation["dependencies"], operation["operation"])
                self.assertLessEqual(set(operation["dependencies"]), blocker_names)


class CombinedHostLifecycleContract(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        boundary_path = TESTS / "http-boundary-contract.py"
        spec = importlib.util.spec_from_file_location("p20_boundary_fixture", boundary_path)
        assert spec and spec.loader
        boundary = importlib.util.module_from_spec(spec)
        sys.modules[spec.name] = boundary
        spec.loader.exec_module(boundary)
        if os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"):
            base_backend = boundary.BackendProcess

            class RustBackendProcess(base_backend):
                def _environment(self) -> dict[str, str]:
                    environment = super()._environment()
                    environment["CODEX_AGENTS_PYTHON"] = os.environ.get(
                        "STUDIO_HTTP_BACKEND_PYTHON", sys.executable
                    )
                    return environment

            # Rust host preparation needs the isolated Python runtime path.
            # The shared harness strips CODEX_AGENTS_PYTHON for arbitrary
            # caller-supplied backend commands, so restore only this explicit
            # test interpreter on the disposable Rust process.
            boundary.BackendProcess = RustBackendProcess
        cls.backend = boundary.create_backend(cls)
        cls.census = json.loads(CORPUS.read_text(encoding="utf-8"))

    def headers(self) -> dict[str, str]:
        return {
            "Origin": self.backend.origin or "",
            "X-Canvas-Token": self.backend.session["token"],
            "X-Studio-API-Schema": self.backend.schema_hash,
        }

    def request(self, method: str, path: str, body: Any = None):
        return self.backend.request(method, path, body=body, headers=self.headers())

    def check_route_result(self, label: str, method: str, path: str, response: Any) -> list[str] | None:
        expected = 200
        if response.status == 501:
            deps = route_dependencies(method, path)
            self.assertTrue(deps, f"{label} refused without a manifest dependency: {response.body[:1000]!r}")
            error = response.json().get("error", "")
            prefix = "Not implemented: requires "
            self.assertTrue(error.startswith(prefix), f"{label}: {response.body[:1000]!r}")
            refused = error[len(prefix):].split(", ")
            self.assertTrue(refused and all(refused), label)
            self.assertLessEqual(set(refused), set(deps), f"{label} named an unlisted dependency: {refused}")
            self.assertTrue(
                all(
                    any(blocker["dependency"] == dependency for blocker in self.census["blockers_ordered_by_operations_unblocked"])
                    for dependency in refused
                ),
                f"{label} named an unlisted dependency: {refused}",
            )
            return refused
        self.assertEqual(response.status, expected, f"{label}: {response.body[:2000]!r}")
        return None

    def seed_answerable_question(self, agent_id: str) -> str:
        import sqlite3

        question_id = "wave7-p20-monitor-approval"
        monitor_id = "wave7-p20-approval-monitor"
        with sqlite3.connect(self.backend.state / "canvas.sqlite3") as db:
            row = db.execute("SELECT record FROM runtime_agents WHERE id=?", (agent_id,)).fetchone()
            self.assertIsNotNone(row, "new agent record is missing")
            agent = json.loads(row[0])
            monitor = {
                "id": monitor_id,
                "agent": agent_id,
                "epoch": agent.get("epoch", 0),
                "status": "approval",
                "created": 1.0,
                "tail": "",
                "bytes": 0,
                "ruleId": None,
            }
            question = {
                "id": question_id,
                "agent": agent_id,
                "epoch": agent.get("epoch", 0),
                "method": "monitor/approve",
                "status": "pending",
                "createdAt": 1.0,
                "params": {"monitorId": monitor_id, "command": "echo fixture"},
            }
            db.execute(
                "INSERT INTO runtime_monitors(id,record) VALUES(?,?)",
                (monitor_id, json.dumps(monitor, separators=(",", ":"))),
            )
            db.execute(
                "INSERT INTO runtime_requests(id,record) VALUES(?,?)",
                (question_id, json.dumps(question, separators=(",", ":"))),
            )
            db.commit()
        return question_id

    def test_create_send_turn_transcript_answer_stop_delete(self) -> None:
        steps: list[dict[str, Any]] = []
        lead = self.request("POST", "/api/leads", {
            "id": "77777777-7777-4777-8777-777777777701",
            "cwd": str(self.backend.cwd),
            "workspaceMode": "worktree",
        })
        create_deps = self.check_route_result("create agent", "POST", "/api/leads", lead)
        if create_deps:
            steps.append({"step": "create agent", "status": "skipped", "dependencies": create_deps})
            for name in ("send message", "complete turn", "read transcript", "answer question", "stop", "delete"):
                steps.append({"step": name, "status": "skipped", "dependencies": create_deps})
            self.assertTrue(steps)
            self._report_steps(steps)
            return

        agent_id = lead.json()["id"]
        steps.append({"step": "create agent", "status": "passed"})
        message = {
            "id": "77777777-7777-4777-8777-777777777702",
            "room": agent_id,
            "text": "Reply with the fixture result.",
            "delivery": "queue",
            "assets": [],
        }
        sent = self.request("POST", "/api/messages", message)
        send_deps = self.check_route_result("send message", "POST", "/api/messages", sent)
        if send_deps:
            steps.append({"step": "send message", "status": "skipped", "dependencies": send_deps})
            steps.append({"step": "complete turn", "status": "skipped", "dependencies": send_deps})
            steps.append({"step": "read transcript", "status": "skipped", "dependencies": send_deps})
        else:
            steps.append({"step": "send message", "status": "passed"})
            transcript_path = "/api/transcript?id=" + agent_id
            initial = self.request("GET", transcript_path)
            transcript_deps = self.check_route_result("read transcript", "GET", "/api/transcript", initial)
            if transcript_deps:
                steps.append({"step": "complete turn", "status": "skipped", "dependencies": transcript_deps})
                steps.append({"step": "read transcript", "status": "skipped", "dependencies": transcript_deps})
            else:
                completed: dict[str, Any] = {"value": initial.json()}

                def turn_completed() -> bool:
                    response = self.request("GET", transcript_path)
                    if response.status == 501:
                        completed["refusal"] = response.body[:1000].decode(
                            "utf-8", errors="replace"
                        )
                        return False
                    self.assertEqual(response.status, 200, response.body[:1000])
                    completed["value"] = response.json()
                    return "Recovered output." in json.dumps(completed["value"])

                try:
                    turn_did_complete = self.backend.wait_until(
                        turn_completed, "P20 scripted provider turn"
                    )
                except TimeoutError:
                    turn_did_complete = False
                if turn_did_complete:
                    steps.append({"step": "complete turn", "status": "passed"})
                    self.assertIn("Recovered output.", json.dumps(completed["value"]))
                    provider_frames = [
                        frame
                        for frame in self.backend.received_provider_frames()
                        if frame.get("method") in {"thread/start", "turn/start"}
                    ]
                    provider_methods = [frame["method"] for frame in provider_frames]
                    self.assertIn("thread/start", provider_methods, provider_frames)
                    self.assertIn("turn/start", provider_methods, provider_frames)
                    print(json.dumps({"p20ProviderFrames": provider_frames}))
                    steps.append({"step": "read transcript", "status": "passed"})
                else:
                    methods = [
                        frame.get("method")
                        for frame in self.backend.received_provider_frames()
                    ]
                    self.fail(
                        "Provider turn did not complete through the transcript HTTP route; "
                        f"last refusal={completed.get('refusal')!r}, "
                        f"transcript={completed.get('value')!r}, methods={methods!r}"
                    )

        question_id = self.seed_answerable_question(agent_id)
        answer = self.request("POST", "/api/answer", {"id": question_id, "decision": "decline"})
        answer_deps = self.check_route_result("answer question", "POST", "/api/answer", answer)
        if answer_deps:
            steps.append({"step": "answer question", "status": "skipped", "dependencies": answer_deps})
        else:
            self.assertEqual(answer.json().get("status"), "answered")
            steps.append({"step": "answer question", "status": "passed"})

        stopped = self.request("POST", "/api/stop", {"id": agent_id})
        stop_deps = self.check_route_result("stop", "POST", "/api/stop", stopped)
        if stop_deps:
            steps.append({"step": "stop", "status": "skipped", "dependencies": stop_deps})
        else:
            self.assertEqual(stopped.json().get("stopped"), [agent_id])
            steps.append({"step": "stop", "status": "passed"})

        deleted = self.request("POST", "/api/conversation/delete", {"id": agent_id})
        delete_deps = self.check_route_result("delete", "POST", "/api/conversation/delete", deleted)
        if delete_deps:
            steps.append({"step": "delete", "status": "skipped", "dependencies": delete_deps})
        else:
            self.assertEqual(deleted.json().get("deleted"), [agent_id])
            steps.append({"step": "delete", "status": "passed"})
        self._report_steps(steps)

    @staticmethod
    def _report_steps(steps: list[dict[str, Any]]) -> None:
        print(json.dumps({"p20HostLifecycle": steps}, sort_keys=True), flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--write-census", action="store_true")
    args, unittest_args = parser.parse_known_args()
    if args.write_census:
        refresh_corpus()
    else:
        unittest.main(argv=[sys.argv[0], *unittest_args], verbosity=2)
