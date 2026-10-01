"""Nonblocking advisories for CLI versions below repo-tested baselines."""
from __future__ import annotations

import re
import subprocess
import threading
import time


# Lowest real CLI versions recorded by repository verification. These are
# advisory reference points, not compatibility or protocol acceptance limits.
TESTED_BASELINES = {
    "codex": "0.153.4",
    "claude": "2.1.278",
}
CHECK_INTERVAL = 300
VERSION_PATTERN = re.compile(r"(?<![0-9])([0-9]+)\.([0-9]+)\.([0-9]+)(?:-([0-9A-Za-z.-]+))?")


def parse_version(value):
    if not isinstance(value, str):
        return None
    match = VERSION_PATTERN.search(value)
    if not match:
        return None
    prerelease = match[4]
    parts = tuple((0, int(part)) if part.isdigit() else (1, part)
                  for part in (prerelease or "").split("."))
    return (*map(int, match.group(1, 2, 3)), prerelease is None, parts)


def version_warning(provider, version):
    baseline = TESTED_BASELINES.get(provider)
    current, tested = parse_version(version), parse_version(baseline)
    if not baseline or current is None or tested is None or current >= tested:
        return None
    label = "Claude Code" if provider == "claude" else "Codex CLI" if provider == "codex" else provider
    return {
        "provider": provider,
        "version": version,
        "baseline": baseline,
        "message": (
            f"{label} {version} is older than this repository's lowest recorded tested version "
            f"({baseline}). It may work poorly or fail. You can continue at your own risk."
        ),
    }


def _codex_version(server, _account):
    from codex_native_runtime import _version
    return _version(server)


def _claude_version(_server, account):
    from codex_claude import installed, subscription_env
    executable = installed(account)
    if not executable:
        return None
    result = subprocess.run([executable, "--version"], env=subscription_env(account),
                            capture_output=True, text=True, timeout=5, check=False)
    if result.returncode:
        return None
    return result.stdout.strip()[:200]


# Add a provider's read-only CLI version adapter and tested baseline here.
VERSION_READERS = {"codex": _codex_version, "claude": _claude_version}


class ProviderVersionMonitor:
    """Read active provider versions off-thread and expose account notices."""

    def __init__(self, interval=CHECK_INTERVAL, read_version=None):
        self.interval = interval
        self.read_version = read_version
        self.lock = threading.Lock()
        self.checked_at = None
        self.next_check = 0.0
        self.signature = None
        self.warnings = []
        self.worker = None

    def tick(self, runtime):
        with runtime.lock:
            connected = list(runtime.servers.items())
        signature = tuple((key, id(server)) for key, server in connected)
        with self.lock:
            if self.worker and self.worker.is_alive():
                return
            if signature == self.signature and time.monotonic() < self.next_check:
                return
            self.signature = signature
            self.next_check = time.monotonic() + self.interval
            self.worker = threading.Thread(target=self._check, args=(runtime, connected),
                                           name="provider-version-check", daemon=True)
            self.worker.start()

    def _check(self, runtime, connected):
        with self.lock:
            warnings = {item["accountKey"]: item for item in self.warnings}
        active_accounts = set()
        for account_key, server in connected:
            active_accounts.add(account_key)
            try:
                account = runtime.accounts.get(account_key)
                provider = account.get("provider", "codex")
                if provider not in TESTED_BASELINES or provider not in VERSION_READERS:
                    warnings.pop(account_key, None)
                    continue
                version = self.read_version(provider, server, account) if self.read_version else \
                    VERSION_READERS[provider](server, account)
                if parse_version(version) is None:
                    continue
                warning = version_warning(provider, version)
                if warning:
                    warnings[account_key] = {
                        "id": f"provider-version:{account_key}",
                        "accountKey": account_key,
                        "provider": provider,
                        "version": warning["version"],
                        "baseline": warning["baseline"],
                        "message": warning["message"],
                        "at": time.time(),
                    }
                else:
                    warnings.pop(account_key, None)
            except (KeyError, OSError, ValueError, TypeError, subprocess.SubprocessError):
                # Diagnostics must never interfere with a working provider session.
                continue
        with self.lock:
            self.warnings = [item for key, item in warnings.items() if key in active_accounts]
            self.checked_at = time.time()
        runtime.changed.set()

    def status(self):
        with self.lock:
            return {"checkedAt": self.checked_at, "warnings": [dict(item) for item in self.warnings]}


def monitor(runtime):
    with runtime.lock:
        current = getattr(runtime, "provider_version_monitor", None)
        if current is None:
            current = runtime.provider_version_monitor = ProviderVersionMonitor()
        return current


def tick(runtime):
    monitor(runtime).tick(runtime)


def status(runtime):
    current = getattr(runtime, "provider_version_monitor", None)
    return current.status() if current else {"checkedAt": None, "warnings": []}
