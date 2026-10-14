import json
import sys
import threading
import time
from pathlib import Path
from types import SimpleNamespace

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "turn_lifecycle"))
from oracle_guard import install_guard

install_guard(block_processes=True)
sys.path.insert(0, str(Path(__file__).resolve().parents[4] / "apps" / "server" / "src"))
import codex_provider_versions as versions
from codex_provider_update_notice import update_warnings

versions.time.time = lambda: 200_000.0
runtime = SimpleNamespace(
    lock=threading.RLock(),
    offline_accounts={"offline"},
    servers={
        "connected": SimpleNamespace(provider="codex"),
        "offline": SimpleNamespace(provider="codex"),
    },
    connection_ids={"connected": "connection-9", "offline": "connection-10"},
    accounts={
        "connected": {"provider": "codex"},
        "offline": {"provider": "codex"},
        "registry-only": {"provider": "codex"},
    },
)
with runtime.lock:
    connected = [
        (key, server, runtime.connection_ids.get(key))
        for key, server in runtime.servers.items()
        if key not in runtime.offline_accounts
    ]
providers = [
    versions._diagnostic(
        key,
        server.provider,
        {
            "runningVersion": "0.153.3",
            "installedVersion": "0.153.3",
            "availableVersion": "0.153.5",
            "upstreamVersion": "0.154.0",
        },
    )
    for key, server, _connection_id in connected
]
warnings = [
    {
        "id": row["id"],
        "accountKey": row["accountKey"],
        "provider": row["provider"],
        "version": row.get("runningVersion")
        or row.get("installedVersion")
        or row.get("configuredVersion"),
        "baseline": row["baseline"],
        "message": row["message"],
        "at": row["at"],
    }
    for row in providers
    if row["status"] == "outdated" and row.get("message")
]
warnings.extend(update_warnings(providers, versions.PROVIDER_LABELS))
print(json.dumps({
    "checkedAt": 200_000.0,
    "providers": providers,
    "warnings": warnings,
}, sort_keys=True))
