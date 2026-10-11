#!/usr/bin/env python3
"""Generate a source-marker inventory of the server test suites."""

from __future__ import annotations

import ast
from collections import Counter
from dataclasses import dataclass
import re
import subprocess
import sys
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
RUNNER = ROOT / "workspaces/runtime/apps/server/tests/server/run.py"
REPORT = Path(__file__).with_name("http-boundary-suite-inventory-2026-10-11.md")
INTERNAL = re.compile(
    r"\b(?:sqlite3?|call_count|assert_(?:called|not_called|has_calls)|"
    r"assert_called|_lock|threading\.|inspect\.getsource|active_transactions|"
    r"publication_checks|dispatch_lock|native_lock)\b|"
    r"\.\w*\.(?:execute|fetchone|fetchall|commit|rollback)\b|"
    r"\.__[A-Za-z]|\._[A-Za-z]\w*|\b(?:SELECT|INSERT|UPDATE|DELETE)\b",
    re.IGNORECASE,
)
PUBLIC = re.compile(
    r"\b(?:assertEqual|assertDict|assertIn|assertNotIn|assertRaises|"
    r"assertTrue|assertFalse|assertIs|assertRegex|assertGreater|assertLess|"
    r"assertAlmostEqual|assertContains|assertStatus|assertJSON|assert\s*\(|"
    r"\bassert\s+)",
    re.IGNORECASE,
)
HTTP = re.compile(r"(?:/api/|https?://|client\.(?:get|post|put|patch|delete)|"
                  r"\.request\(|httpx\.|requests\.|urllib\.request)", re.IGNORECASE)
REAL_BACKEND_PROCESS = re.compile(
    r"\bBackendProcess\b|\bstart_contract_backend\b|"
    r"subprocess\.Popen\s*\(\s*\[\s*sys\.executable\s*,\s*"
    r"str\(SERVER_SOURCE_ROOT\s*/\s*['\"]codex-canvas['\"]\)",
)
DOMAIN_ROUTES = {
    "sync": "/api/sync/*",
    "accounts": "/api/accounts/*",
    "agents": "/api/agents/*",
    "work": "/api/work/*",
    "history": "/api/history/*",
    "voice": "/api/voice/*",
    "insights": "/api/insights/*",
    "federation": "/api/federation/*",
    "multi_server": "/api/multi-server/*",
    "io": "/api/io/*",
    "system": "/api/* and /api/session",
}
DOMAIN_IMPORTS = {
    "codex_sync": "sync", "studio_api.sync": "sync",
    "codex_voice": "voice", "codex_claude_controls": "agents",
    "codex_accounts": "accounts", "studio_api.accounts": "accounts",
    "codex_agents": "agents", "studio_api.agents": "agents",
    "codex_voice": "voice", "studio_api.voice": "voice",
    "studio_api.work": "work",
    "studio_api.history": "history", "studio_api.insights": "insights",
    "studio_api.federation": "federation", "studio_api.multi_server": "multi_server",
    "studio_api.io": "io", "studio_api.system": "system",
}
VISIBLE_IMPORT = re.compile(
    r"(?:studio_api(?:\.|\s)|codex_(?:sync|voice|claude_controls|browser)(?:\s|$)|"
    r"(?:^|\s)(?:analytics|sync|voice|agents|accounts|"
    r"federation|history|insights|work|multi_server)\.)",
    re.MULTILINE,
)
PROVIDER_OPERATION = re.compile(
    r"(?:thread/(?:start|resume|settings/update|compact/start)|turn/start|"
    r"review/start|skills/extraRoots/set|model/list|tool/request|"
    r"responses\.create|response\.output)",
    re.IGNORECASE,
)
BLOCKERS = (
    (re.compile(r"\b(?:sleep|monotonic|perf_counter|freeze_time|freezegun)\b"), "time control"),
    (re.compile(r"claude|anthropic", re.IGNORECASE), "Claude provider fake"),
    (re.compile(r"\b(?:mock|monkeypatch|patch|side_effect|fault|raise_for_status)\b", re.IGNORECASE),
     "fault injection inside the process"),
    (re.compile(r"\b(?:second server|two servers|multi.server|server_a|server_b)\b", re.IGNORECASE),
     "second server"),
)
HAND_REVIEW: dict[str, frozenset[str]] = {
    "workspaces/runtime/apps/server/src/analytics/tests/test_rollout_parser.py": frozenset({"B"}),
    "workspaces/runtime/apps/server/src/studio_api/sync/resources/test_entity_commit_publisher.py": frozenset({"B", "C"}),
    "workspaces/runtime/apps/server/src/sync/tests/test_sync_store.py": frozenset({"B", "C"}),
    "workspaces/runtime/apps/server/src/test_codex_api_client.py": frozenset({"B", "C"}),
    "workspaces/runtime/apps/server/src/test_codex_voice_producers.py": frozenset({"B", "C"}),
    "workspaces/runtime/apps/server/tests/analytics-history-agent-projection-contract.py": frozenset({"B", "C"}),
    "workspaces/runtime/apps/server/tests/browser-lock-contract.py": frozenset({"B"}),
    "workspaces/runtime/apps/server/tests/browser-native-contract.py": frozenset({"B", "C"}),
    "workspaces/runtime/apps/server/tests/claude-controls-runtime-contract.py": frozenset({"B", "C"}),
    "workspaces/runtime/apps/server/tests/context-role-dedup-contract.py": frozenset({"B"}),
    "workspaces/runtime/apps/server/tests/harness-response-contract.py": frozenset({"B"}),
    "workspaces/runtime/apps/server/tests/message-intent-contract.py": frozenset({"B"}),
    "workspaces/runtime/apps/server/tests/native-action-context-repair-contract.py": frozenset({"B"}),
    "workspaces/runtime/apps/server/tests/native-action-receipts-contract.py": frozenset({"B", "C"}),
    "workspaces/runtime/apps/server/tests/notification-wakeup-contract.py": frozenset({"B", "C"}),
    "workspaces/runtime/apps/server/tests/old-turn-task-completion-contract.py": frozenset({"B"}),
    "workspaces/runtime/apps/server/tests/protocol-reader-contract.py": frozenset({"B"}),
    "workspaces/runtime/apps/server/tests/projects-contract.py": frozenset({"B"}),
    "workspaces/runtime/apps/server/tests/runtime-agent-scope-contract.py": frozenset({"B", "C"}),
    "workspaces/runtime/apps/server/tests/runtime-accounts-contract.py": frozenset({"B", "C"}),
    "workspaces/runtime/apps/server/tests/send-idle-state-contract.py": frozenset({"B", "C"}),
    "workspaces/runtime/apps/server/tests/source-inventory-contract.py": frozenset({"C"}),
    "workspaces/runtime/apps/server/tests/server-exec-path-contract.py": frozenset({"C"}),
    "workspaces/runtime/apps/server/tests/spawn-recovery-native.py": frozenset({"C"}),
    "workspaces/runtime/apps/server/tests/sync-contract.py": frozenset({"C"}),
    "workspaces/runtime/apps/server/tests/sync-entities-contract.py": frozenset({"B", "C"}),
    "workspaces/runtime/apps/server/tests/tool-parity.py": frozenset({"B"}),
    "workspaces/runtime/apps/server/tests/windows-process-supervisor-contract.py": frozenset({"C"}),
    "workspaces/runtime/apps/server/tests/yolo-contract.py": frozenset({"B"}),
}
INVENTORY_MAIN_COMMIT = "02599ed5dccc0c524ed69f1fc72a230038031d59"
HAND_SAMPLE = (
    "workspaces/runtime/apps/server/src/analytics/tests/test_rollout_parser.py",
    "workspaces/runtime/apps/server/src/studio_api/sync/resources/test_entity_commit_publisher.py",
    "workspaces/runtime/apps/server/src/sync/tests/test_sync_store.py",
    "workspaces/runtime/apps/server/src/test_codex_api_client.py",
    "workspaces/runtime/apps/server/src/test_codex_voice_producers.py",
    "workspaces/runtime/apps/server/tests/analytics-history-agent-projection-contract.py",
    "workspaces/runtime/apps/server/tests/browser-lock-contract.py",
    "workspaces/runtime/apps/server/tests/browser-native-contract.py",
    "workspaces/runtime/apps/server/tests/claude-controls-runtime-contract.py",
    "workspaces/runtime/apps/server/tests/context-role-dedup-contract.py",
    "workspaces/runtime/apps/server/tests/harness-response-contract.py",
    "workspaces/runtime/apps/server/tests/message-intent-contract.py",
    "workspaces/runtime/apps/server/tests/native-action-context-repair-contract.py",
    "workspaces/runtime/apps/server/tests/native-action-receipts-contract.py",
    "workspaces/runtime/apps/server/tests/notification-wakeup-contract.py",
    "workspaces/runtime/apps/server/tests/old-turn-task-completion-contract.py",
    "workspaces/runtime/apps/server/tests/projects-contract.py",
    "workspaces/runtime/apps/server/tests/protocol-reader-contract.py",
    "workspaces/runtime/apps/server/tests/server-exec-path-contract.py",
    "workspaces/runtime/apps/server/tests/sync-entities-contract.py",
)


@dataclass
class Unit:
    name: str
    source: str
    tests: int


def discovered_suites() -> list[str]:
    result = subprocess.run(
        [sys.executable, str(RUNNER), "--list"], cwd=ROOT, check=True,
        capture_output=True, text=True,
    )
    return [line.split(maxsplit=1)[1] for line in result.stdout.splitlines()
            if line.startswith(("default", "opt-in:"))]


def python_units(path: Path) -> list[Unit]:
    source = path.read_text(encoding="utf-8")
    tree = ast.parse(source, filename=str(path))
    units: list[Unit] = []
    for node in ast.walk(tree):
        if isinstance(node, ast.ClassDef):
            methods = [child for child in node.body
                       if isinstance(child, (ast.FunctionDef, ast.AsyncFunctionDef))
                       and child.name.startswith("test")]
            method_by_name = {method.name: method for method in node.body
                              if isinstance(method, (ast.FunctionDef, ast.AsyncFunctionDef))}
            for method in methods:
                method_source = ast.get_source_segment(source, method) or ""
                calls = {child.func.attr if isinstance(child.func, ast.Attribute) else child.func.id
                         for child in ast.walk(method)
                         if isinstance(child, ast.Call)
                         and isinstance(child.func, (ast.Name, ast.Attribute))}
                helpers = [ast.get_source_segment(source, method_by_name[name]) or ""
                           for name in calls.intersection(method_by_name)
                           if name != method.name and not name.startswith("assert")]
                units.append(Unit(node.name + "." + method.name,
                                  "\n".join([method_source, *helpers]), 1))
    functions = [node for node in tree.body
                 if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef))
                 and node.name.startswith("test")]
    units.extend(Unit("<module>." + function.name,
                      ast.get_source_segment(source, function) or "", 1)
                 for function in functions)
    if not units:
        units.append(Unit("<module script>", source, 0))
    return units


def classify(unit: Unit, module_source: str) -> tuple[str, str, str, str]:
    source = unit.source
    imports = "\n".join(line for line in module_source.splitlines()
                         if line.startswith(("import ", "from ")))
    combined = imports + "\n" + source
    try:
        node = ast.parse(source)
        assertion_nodes = [child.test for child in ast.walk(node) if isinstance(child, ast.Assert)]
        assertion_nodes.extend(
            child for child in ast.walk(node)
            if isinstance(child, ast.Call)
            and isinstance(child.func, ast.Attribute)
            and child.func.attr.startswith("assert")
        )
        assertion_text = "\n".join(ast.unparse(child) for child in assertion_nodes)
    except SyntaxError:
        assertion_text = source
    internal = bool(INTERNAL.search(assertion_text))
    process_http = bool(REAL_BACKEND_PROCESS.search(combined) and HTTP.search(combined))
    in_process_http = bool(re.search(r"\bTestClient\b|ASGITransport", combined))
    local_http_protocol = bool(
        re.search(r"ThreadingHTTPServer|BaseHTTPRequestHandler", module_source)
        and re.search(r"/api/", source)
    )
    provider_frames = bool(PROVIDER_OPERATION.search(combined))
    runtime_direct = bool(re.search(r"\b(?:Runtime|Canvas|CodexRuntime)\s*\(|from\s+codex_runtime", combined))
    first_party_visible = bool(VISIBLE_IMPORT.search(imports))
    if process_http:
        klass = "A"
        detail = "real backend process plus HTTP request/response assertions"
    elif local_http_protocol or (provider_frames and first_party_visible):
        klass = "B"
        detail = "observable HTTP/provider protocol asserted through a local fake or Python client"
    elif internal:
        klass = "C"
        detail = "internal/storage/concurrency assertion markers"
    elif in_process_http:
        klass = "B"
        detail = "HTTP-shaped outcome asserted through in-process ASGI/TestClient"
    elif "codex_source_inventory" in imports:
        klass = "C"
        detail = "pure source inventory helper; protects implementation fingerprint semantics"
    elif runtime_direct or (PUBLIC.search(source) and re.search(r"\b(?:self\.runtime|runtime\.)", combined)):
        klass = "B"
        detail = "public outcome asserted after direct Python runtime/service construction"
    elif PUBLIC.search(source) and first_party_visible:
        klass = "B"
        detail = "asserted public/domain outcome through a Python module rather than a backend process"
    else:
        klass = "C"
        detail = "source exposes helper/implementation behavior without a client-visible assertion"

    route = ", ".join(sorted({DOMAIN_ROUTES[domain] for module, domain in DOMAIN_IMPORTS.items()
                               if module in combined}))
    if not route:
        path_literals = sorted(set(re.findall(r"['\"](/api/[^'\"]+)['\"]", source)))
        route = ", ".join(path_literals) if path_literals else ""
    if not route:
        tokens = unit.name.lower()
        inferred = [path for domain, path in DOMAIN_ROUTES.items()
                    if re.search(rf"\b{re.escape(domain)}\b", tokens)]
        route = ", ".join(sorted(set(inferred)))
    if not route:
        route = "route/frame mapping needed from the asserted operation"
    if re.search(r"\b(?:Codex|codex|AppServer|app-server)\b", combined):
        route += ("; Codex app-server JSON-RPC frames"
                  if route else "Codex app-server JSON-RPC frames")
    if re.search(r"\b(?:responses\.create|response\.output|base_url|wire_api)\b", combined):
        route += ("; OpenAI Responses API request/stream frames"
                  if route else "OpenAI Responses API request/stream frames")
    if re.search(r"thread/realtime/", combined):
        route += ("; Codex app-server realtime JSON-RPC frames"
                  if route else "Codex app-server realtime JSON-RPC frames")
    if re.search(r"\b(?:Claude|claude|Anthropic)\b", combined, re.IGNORECASE):
        route += ("; Claude bridge JSONL frames" if route else "Claude bridge JSONL frames")
    if provider_frames and not re.search(r"\b(?:Codex|codex|AppServer|app-server)\b", combined):
        route += ("; provider JSON-RPC frames" if route else "provider JSON-RPC frames")

    blockers = [label for pattern, label in BLOCKERS if pattern.search(source)]
    if klass == "B" and not blockers:
        blockers = ["none detected by source markers"]
    if klass == "B" and "route/frame mapping needed" in route:
        blockers.append("route/provider operation mapping")
    if klass == "C":
        if re.search(r"\b(?:sqlite|SELECT|INSERT|UPDATE|DELETE|commit|cursor)\b", source, re.I):
            detail += "; protects storage invariant"
        elif re.search(r"\b(?:Thread|threading|Lock|race|concurrent)\b", source, re.I):
            detail += "; protects concurrency race"
        elif re.search(r"\b(?:restart|recovery|journal|reconcile)\b", source, re.I):
            detail += "; protects recovery internals"
        else:
            detail += "; protects an implementation invariant/pure helper"
    return klass, detail, route, ", ".join(blockers) or "none detected"


def main() -> None:
    groups: dict[tuple[str, str, str], list[tuple[Unit, str, str, str]]] = {}
    suite_test_counts: Counter[str] = Counter()
    suite_paths = discovered_suites()
    for relative in suite_paths:
        path = ROOT / relative
        source = path.read_text(encoding="utf-8")
        units = python_units(path) if path.suffix == ".py" else [
            Unit(f"<script {i}>", source, 1)
            for i, _ in enumerate(re.finditer(r"\b(?:test|it)(?:\.\w+)?\s*\(", source))
        ] or [Unit("<script>", source, 0)]
        for unit in units:
            klass, detail, route, blockers = classify(unit, source)
            groups.setdefault((relative, klass, unit.name.split(".", 1)[0]), []).append(
                (unit, detail, route, blockers)
            )
            suite_test_counts[klass] += unit.tests

    suite_labels: dict[str, set[str]] = {}
    class_functions = Counter()
    exclusive_functions = Counter()
    for (relative, klass, _class_name), values in groups.items():
        suite_labels.setdefault(relative, set()).add(klass)
        count = sum(unit.tests for unit, *_ in values)
        class_functions[klass] += count
        exclusive_functions[(relative, klass)] += count
    suite_counts = Counter(label for labels in suite_labels.values() for label in labels)
    exclusive_suite_counts = Counter("/".join(sorted(labels)) for labels in suite_labels.values())
    exclusive_function_counts = Counter()
    for (relative, klass), count in exclusive_functions.items():
        category = "/".join(sorted(suite_labels[relative]))
        exclusive_function_counts[(category, klass)] += count

    blocker_suites: dict[str, set[str]] = {}
    blocker_functions = Counter()
    clear_b_suites: set[str] = set()
    clear_b_functions = 0
    for (relative, klass, _class_name), values in groups.items():
        if klass != "B":
            continue
        suite_blockers: set[str] = set()
        for unit, _detail, _route, blocker_text in values:
            blockers = set() if blocker_text == "none detected by source markers" else set(
                blocker_text.split(", ")
            )
            blockers.discard("none detected by source markers")
            for blocker in blockers:
                suite_blockers.add(blocker)
                blocker_functions[blocker] += unit.tests
            if not blockers:
                clear_b_functions += unit.tests
        for blocker in suite_blockers:
            blocker_suites.setdefault(blocker, set()).add(relative)
        if not suite_blockers:
            clear_b_suites.add(relative)
    sample_paths = list(HAND_SAMPLE)
    wrong = sum(1 for relative in sample_paths if HAND_REVIEW.get(relative) !=
                frozenset(suite_labels.get(relative, set())))

    lines = [
        "# HTTP-boundary server-suite inventory (2026-10-11)", "",
        f"Generated from main commit `{INVENTORY_MAIN_COMMIT}`.",
        "Generated by [`generate_http_boundary_suite_inventory.py`](generate_http_boundary_suite_inventory.py).",
        "Run `python3 docs/verification/generate_http_boundary_suite_inventory.py` to refresh.", "",
        "This is a static source screen, not semantic proof. Discovery comes from the server runner's `--list`; Python test functions are AST-counted by `test*` name, while JavaScript test registrations are counted from their `test`/`it` calls. Each Python test method/function is screened independently; mixed classes appear in separate rows so counts stay attached to their source class. A/B/C mean: A, real backend process and HTTP; B, client/provider-visible outcome but test bypasses the real backend HTTP process; C, assertion targets an implementation detail that another backend cannot expose. The evidence column summarizes the source indicators that matched.", "",
        "The B target names a candidate public route family or provider protocol inferred from source imports and request strings. `none detected by source markers` is not a proof that conversion is unblocked. C notes name the internal invariant category to preserve in Python regression tests or re-prove through the replacement's implementation-specific suite.", "",
        f"Totals: **{len(suite_labels)} suites**, **{sum(suite_test_counts.values())} named test functions**. A: {suite_counts['A']} suites / {class_functions['A']} functions; B: {suite_counts['B']} / {class_functions['B']}; C: {suite_counts['C']} / {class_functions['C']}. Mixed suites may appear in more than one suite class; functions are classified by their containing test method.", "",
        "Earlier inventory snapshot: 436 suites / 4,992 named test functions.", "",
        f"Hand-check method: the 20 suites below were selected across A, B, mixed, and C source-marker labels. Their imports, setup, test bodies, and assertions were manually read and compared to the source-marker classification. Hand-check errors: **{wrong}/20** (manual answers are recorded in the generator).", "",
        "The sampled mismatch went from **B to C**: `sync-entities-contract.py` was manually classified B/C, while the source screen reported only C. Its module-level script mixes client-visible entity projection checks with SQLite migration and retention assertions; a rule that records both public-output and internal assertion evidence for a mixed module script would have caught it.", "",
        "Exclusive suite groups (each suite appears once; the mixed row is the union of suites with more than one class):", "",
        "| Exclusive suite group | Suites | A functions | B functions | C functions | Total functions |",
        "|---|---:|---:|---:|---:|---:|",
    ]
    for category in ("A", "B", "C"):
        if category not in exclusive_suite_counts:
            continue
        lines.append(
            f"| Pure {category} | {exclusive_suite_counts[category]} | "
            f"{exclusive_function_counts[(category, 'A')]} | "
            f"{exclusive_function_counts[(category, 'B')]} | "
            f"{exclusive_function_counts[(category, 'C')]} | "
            f"{sum(exclusive_function_counts[(category, label)] for label in 'ABC')} |"
        )
    mixed_categories = [category for category in exclusive_suite_counts if "/" in category]
    if mixed_categories:
        mixed_functions = {
            label: sum(exclusive_function_counts[(category, label)] for category in mixed_categories)
            for label in "ABC"
        }
        lines.append(
            f"| Mixed | {sum(exclusive_suite_counts[category] for category in mixed_categories)} | "
            f"{mixed_functions['A']} | {mixed_functions['B']} | {mixed_functions['C']} | "
            f"{sum(mixed_functions.values())} |"
        )
    lines.extend([
        "",
        f"B source-screen conversion estimate: **{len(clear_b_suites)} suites with {clear_b_functions} named B test functions have no detected blocker**. The other {suite_counts['B'] - len(clear_b_suites)} B-bearing suites have at least one detected blocker. Blocker counts below overlap when a suite or test needs more than one capability; these are source-screen estimates, not a conversion plan.",
        "",
        "| Source blocker | B suites | B test functions |",
        "|---|---:|---:|",
    ])
    for blocker in (
        "Claude provider fake", "time control", "fault injection inside the process",
        "second server", "route/provider operation mapping",
    ):
        lines.append(
            f"| {blocker} | {len(blocker_suites.get(blocker, set()))} | "
            f"{blocker_functions[blocker]} |"
        )
    lines.extend([
        "",
        "The blocker columns are non-exclusive. Route/provider mapping is separate from the four execution blockers; it means source did not identify a concrete public boundary target.", "",
        "| Hand-sampled suite | Source-marker label | Manual label |",
        "|---|---:|---:|",
    ])
    for relative in sample_paths:
        predicted = "/".join(sorted(suite_labels.get(relative, set())))
        manual = "/".join(sorted(HAND_REVIEW.get(relative, frozenset()))) or "pending"
        lines.append(f"| `{relative}` | {predicted} | {manual} |")
    lines.extend([
        "",
        "## Suite rows", "",
        "| Suite | Class / tests | Label | Source evidence and what C protects | B boundary target | B conversion blocker |",
        "|---|---|---:|---|---|---|",
    ])
    for (relative, klass, class_name), values in sorted(groups.items()):
        n = sum(unit.tests for unit, *_ in values)
        details = sorted({detail for _, detail, _, _ in values})
        routes = sorted({route for _, _, route, _ in values})
        blockers = sorted({blocker for _, _, _, blocker in values})
        esc = lambda value: value.replace("|", "\\|").replace("\n", " ")
        lines.append(
            f"| `{relative}` | `{class_name}` / {n} | **{klass}** | {esc('; '.join(details))} | "
            f"{esc('; '.join(routes)) if klass == 'B' else '—'} | "
            f"{esc('; '.join(blockers)) if klass == 'B' else '—'} |"
        )
    REPORT.write_text("\n".join(lines) + "\n", encoding="utf-8")
    subprocess.run(
        ["pnpm", "exec", "oxfmt", str(REPORT.relative_to(ROOT))],
        cwd=ROOT, check=True, capture_output=True, text=True,
    )


if __name__ == "__main__":
    main()
