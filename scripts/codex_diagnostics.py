"""Shareable, read-only process and native session snapshot."""

import os
import json
import re
import subprocess
import time
from concurrent.futures import ThreadPoolExecutor, as_completed
from datetime import datetime, timezone

from codex_native_sweep import _loaded
from codex_sqlite import diagnostics as sqlite_diagnostics


def _kind(command):
    executable = command.split(None, 1)[0].rsplit("/", 1)[-1] if command else ""
    if "claude_bridge/bridge.mjs" in command:
        return "claude_bridge"
    if "codex-code-mode-host" in command:
        return "code_mode_host"
    if "codex app-server" in command:
        return "codex_app_server"
    if "claude --output-format" in command:
        return "claude_cli"
    if "node_repl" in command:
        return "node_repl"
    if "anarlog mcp" in command:
        return "anarlog_mcp"
    if "codex-canvas" in command:
        return "studio_backend"
    if "chrome-headless" in executable:
        return "headless_browser"
    if executable.lower().startswith("python"):
        return "python"
    if executable == "node":
        return "node"
    if executable in {"sh", "bash", "zsh"}:
        return "shell"
    return "other"


def process_tree(root_pid, output=None):
    if output is None:
        output = subprocess.check_output(
            ["ps", "-axo", "pid=,ppid=,rss=,%cpu=,command="], text=True, timeout=5)
    rows = {}
    for line in output.splitlines():
        fields = line.strip().split(None, 4)
        if len(fields) != 5:
            continue
        try:
            pid, parent, rss = map(int, fields[:3])
            cpu = float(fields[3])
        except ValueError:
            continue
        rows[pid] = (parent, rss, cpu, _kind(fields[4]))
    descendants = {root_pid}
    while True:
        found = {pid for pid, row in rows.items() if row[0] in descendants}
        if found <= descendants:
            break
        descendants.update(found)
    processes = [{"pid": pid, "parentPid": rows[pid][0], "kind": rows[pid][3],
                  "rssMiB": round(rows[pid][1] / 1024, 1), "cpuPercent": rows[pid][2]}
                 for pid in sorted(descendants) if pid in rows]
    kinds = {}
    for row in processes:
        item = kinds.setdefault(row["kind"], {"count": 0, "rssMiB": 0, "cpuPercent": 0})
        item["count"] += 1
        item["rssMiB"] += row["rssMiB"]
        item["cpuPercent"] += row["cpuPercent"]
    for item in kinds.values():
        item["rssMiB"] = round(item["rssMiB"], 1)
        item["cpuPercent"] = round(item["cpuPercent"], 1)
    return {"rootPid": root_pid, "processes": processes, "kinds": kinds,
            "totalRssMiB": round(sum(row["rssMiB"] for row in processes), 1)}


def _queue_size(queue):
    return queue.qsize() if queue is not None else None


def host_resources():
    """Read host capacity only when a diagnostics request arrives."""
    total = available = None
    try:
        total = int(subprocess.check_output(["sysctl", "-n", "hw.memsize"],
                                            text=True, timeout=2).strip())
        stats = subprocess.check_output(["vm_stat"], text=True, timeout=2)
        size = re.search(r"page size of (\d+) bytes", stats)
        pages = [re.search(r"^Pages " + name + r":\s+(\d+)\.", stats, re.M)
                 for name in ("free", "inactive", "speculative")]
        if size and all(pages):
            available = min(total, sum(int(value.group(1)) for value in pages)
                            * int(size.group(1)))
    except (OSError, ValueError, subprocess.SubprocessError):
        pass
    return {"cpuCount": os.cpu_count(), "totalMemoryBytes": total,
            "availableMemoryBytes": available}


def lock_samples(runtime, count=5):
    values = []
    for _ in range(count):
        start = time.monotonic()
        acquired = runtime.lock.acquire(timeout=.01)
        waited = round((time.monotonic() - start) * 1000, 2)
        holder = []
        if acquired:
            runtime.lock.release()
        else:
            runtime.sample_dispatch_lock_holder(holder, round(waited))
        values.append({"waitMs": waited, "holder": holder[0] if holder else None})
        time.sleep(.01)
    return values


def migration_status(runtime):
    """Expose durable migration cursors and transient errors to operators."""
    status = {}
    try:
        with runtime.db() as db:
            tables = {row[0] for row in db.execute(
                "SELECT name FROM sqlite_master WHERE type='table'")}
            if "runtime_search_rollout" in tables:
                row = db.execute("SELECT phase,cursor,updated FROM runtime_search_rollout WHERE id=1").fetchone()
                if row:
                    status["search"] = {"phase": row[0], "cursor": row[1], "updated": row[2],
                                         "error": getattr(runtime, "search_migration_error", None),
                                         "lastBatchBytes": getattr(runtime, "search_migration_last_batch_bytes", None)}
            if "runtime_payload_migrations" in tables:
                columns = {row[1] for row in db.execute("PRAGMA table_info(runtime_payload_migrations)")}
                selected = "name,cursor,complete" + (",status,updated,error" if "status" in columns else "")
                payload = {}
                for row in db.execute("SELECT " + selected + " FROM runtime_payload_migrations"):
                    payload[row[0]] = {"cursor": row[1], "complete": bool(row[2]),
                                       "status": row[3] if len(row) > 3 else ("complete" if row[2] else "pending"),
                                       "updated": row[4] if len(row) > 4 else None,
                                       "error": row[5] if len(row) > 5 else None}
                status["payloads"] = {name: payload.get("payload-v1:" + name,
                                                       {"cursor": 0, "complete": False,
                                                        "status": "pending", "updated": None,
                                                        "error": None})
                                       for name in ("checkpoints", "tool_requests", "tool_results", "tasks")}
            else:
                status["payloads"] = {name: {"cursor": 0, "complete": False,
                                             "status": "pending", "updated": None, "error": None}
                                       for name in ("checkpoints", "tool_requests", "tool_results", "tasks")}
            if "sync_entity_meta" in tables:
                values = dict(db.execute("SELECT key,value FROM sync_entity_meta WHERE key IN "
                                         "('entity_tombstone_count','entity_tombstone_floor')"))
                status["entityTombstones"] = {
                    "count": int(values.get("entity_tombstone_count", 0)),
                    "floor": int(values.get("entity_tombstone_floor", 0)),
                    "pruning": getattr(getattr(runtime, "sync_store", None),
                                       "entity_prune_status", {"status": "notStarted"}),
                }
    except Exception as error:
        status["stateReadError"] = f"{type(error).__name__}: {error}"[:500]
    status["analyticsFile"] = getattr(runtime, "analytics_migration_status", {"status": "idle"})
    return status


def snapshot(runtime, root_pid=None, ps_output=None):
    root_pid = os.getpid() if root_pid is None else root_pid
    started = time.monotonic()
    if ps_output is None:
        ps_output = subprocess.check_output(
            ["ps", "-axo", "pid=,ppid=,rss=,%cpu=,command="], text=True, timeout=5)
    tree = process_tree(root_pid, ps_output)
    attribution = [{"component": "process", "operation": "ps", "count": 1,
                    "logicalReadBytes": len(ps_output.encode()),
                    "durationMs": round((time.monotonic() - started) * 1000, 2)}]
    for kind, values in tree["kinds"].items():
        attribution.append({"component": kind, "operation": "resident", "count": values["count"],
                            "residentBytes": round(values["rssMiB"] * 1024 * 1024)})
    with runtime.lock:
        servers = sorted(runtime.servers.items())
        providers = {key: (runtime.accounts.get(key) or {}).get("provider", "codex")
                     for key, _server in servers}
        studio_loaded = len(runtime.loaded)
        queues = {"recoveryPending": runtime.recovery_pool._work_queue.qsize()}
        for index, (_account, server) in enumerate(servers, 1):
            alias = "account" + str(index)
            for name in ("callbacks", "clock_replies", "tool_requests"):
                queues[alias + "." + name] = _queue_size(getattr(server, name, None))
    with runtime.db() as db:
        queues["durableInputPending"] = db.execute(
            "SELECT COUNT(*) FROM runtime_events WHERE status IN "
            "('pending','reserved','dispatching','uncertain')").fetchone()[0]

    def native(account, server):
        started = time.monotonic()
        if providers[account] == "claude":
            result = server.call("claude/diagnostics", {}, timeout=5)
            value = {"provider": "claude", **result}
            read_bytes = len(json.dumps(result).encode())
        else:
            loaded = _loaded(server)
            value = {"provider": "codex", "loadedThreads": len(loaded)}
            read_bytes = len(json.dumps(loaded).encode())
        return value, round((time.monotonic() - started) * 1000, 2), read_bytes

    native_accounts = {}
    if servers:
        pool = ThreadPoolExecutor(max_workers=min(4, len(servers)))
        try:
            futures = {pool.submit(native, key, server): index
                       for index, (key, server) in enumerate(servers, 1)}
            for future in as_completed(futures):
                alias = "account" + str(futures[future])
                try:
                    value, duration, read_bytes = future.result()
                    native_accounts[alias] = value
                    attribution.append({"component": value["provider"], "operation": "nativeStatus",
                                        "count": 1, "logicalReadBytes": read_bytes,
                                        "durationMs": duration})
                except Exception as error:
                    native_accounts[alias] = {"error": type(error).__name__}
        finally:
            pool.shutdown(wait=False, cancel_futures=True)
    sampled = lock_samples(runtime)
    attribution.append({"component": "runtime", "operation": "lockSample",
                        "count": len(sampled), "durationMs": round(sum(x["waitMs"] for x in sampled), 2)})
    attribution.sort(key=lambda row: (row["component"], row["operation"]))
    result = {"at": datetime.now(timezone.utc).isoformat(), "processTree": tree,
            "hostResources": host_resources(), "resourceAttribution": attribution,
            "nativeAccounts": dict(sorted(native_accounts.items())),
            "studioLoadedThreads": studio_loaded, "queues": queues,
            "runtimeLockSamples": sampled,
            "sqliteContention": sqlite_diagnostics(),
            "analyticsCapture": getattr(runtime, "analytics_capture_status", lambda: {"available": False})(),
            "migrations": migration_status(runtime),
            "analyticsFileMigration": getattr(runtime, "analytics_migration_status", {"status": "idle"}),
            "searchMigrationError": getattr(runtime, "search_migration_error", None)}
    lock_metrics = getattr(runtime.lock, "runtime_lock_metrics", None)
    if callable(lock_metrics):
        result["runtimeLockOperations"] = lock_metrics()
    return result
