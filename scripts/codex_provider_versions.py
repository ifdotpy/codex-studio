"""Nonblocking, account-scoped CLI version diagnostics and advisories."""
from __future__ import annotations

import re
import subprocess
import threading
import time


# Lowest real CLI versions recorded by repository verification. These are
# advisory reference points, not compatibility or protocol acceptance limits.
TESTED_BASELINES = {"codex": "0.153.4", "claude": "2.1.278"}
PROVIDER_LABELS = {"codex": "Codex CLI", "claude": "Claude Code"}
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


def version_warning(provider, version, *, source="running"):
    baseline = TESTED_BASELINES.get(provider)
    current, tested = parse_version(version), parse_version(baseline)
    if not baseline or current is None or tested is None or current >= tested:
        return None
    label = PROVIDER_LABELS.get(provider, provider)
    if source == "running":
        detail = f"The running {label} version {version} is below"
    elif source == "launchedExecutable":
        detail = f"The executable configured for this {label} connection currently reports {version}, below"
    else:
        detail = f"The current {label} profile executable reports {version}, below"
    running_caveat = "" if source == "running" else " The exact version already running may be different."
    return {
        "provider": provider,
        "version": version,
        "baseline": baseline,
        "message": (
            f"{detail} this repository's lowest recorded tested version ({baseline})."
            f"{running_caveat} It may work poorly or fail. "
            "You can continue at your own risk."
        ),
    }


def _codex_version(server, _account):
    from codex_native_runtime import _version
    version = _version(server)
    return {"runningVersion": version, "installedVersion": version}


def _read_claude_executable(profile):
    from codex_claude import installed, subscription_env
    executable = installed(profile)
    if not executable:
        return None, "Claude Code executable was not found for this profile."
    try:
        result = subprocess.run([executable, "--version"], env=subscription_env(profile),
                                capture_output=True, text=True, timeout=5, check=False)
    except (OSError, subprocess.SubprocessError):
        return None, "Could not read the Claude Code executable version."
    if result.returncode:
        return None, "Claude Code --version failed."
    output = result.stdout.strip()[:200]
    if parse_version(output) is None:
        return None, "Claude Code did not report a recognized semantic version."
    return output, None


def _claude_version(server, account):
    # AppServer retains the options used to launch this connection. Account
    # settings can change while that bridge stays alive, so inspect both paths.
    launched_profile = {"claudeOptions": getattr(server, "provider_options", {})}
    installed, launched_error = _read_claude_executable(launched_profile)
    configured = None
    configured_error = None
    if account.get("provider") == "claude":
        configured, configured_error = _read_claude_executable(account)
    return {
        "runningVersion": None,
        "installedVersion": installed,
        "configuredVersion": configured,
        "error": launched_error or configured_error,
        "runningNote": "The exact version already running in Claude sessions is unknown.",
    }


# Add a provider's read-only adapter and an evidence-backed baseline here.
VERSION_READERS = {"codex": _codex_version, "claude": _claude_version}


def _diagnostic(account_key, provider, versions):
    baseline = TESTED_BASELINES[provider]
    running = versions.get("runningVersion")
    installed = versions.get("installedVersion")
    configured = versions.get("configuredVersion")
    warning = version_warning(provider, running) if running else None
    if warning is None and installed:
        warning = version_warning(provider, installed, source="launchedExecutable")
    if warning is None and configured:
        warning = version_warning(provider, configured, source="configuredProfile")
    known = [parse_version(item) for item in (running, installed, configured) if item]
    if any(item is not None and item < parse_version(baseline) for item in known):
        check_status = "outdated"
    elif running:
        check_status = "current"
    elif versions.get("error") and not installed and not configured:
        check_status = "error"
    else:
        check_status = "unknown"
    result = {
        "id": f"provider-version:{account_key}",
        "accountKey": account_key,
        "provider": provider,
        "status": check_status,
        "runningVersion": running,
        "installedVersion": installed,
        "configuredVersion": configured,
        "baseline": baseline,
        "error": versions.get("error"),
        "message": warning["message"] if warning else versions.get("runningNote") or
        ("Could not determine a provider CLI version." if check_status in {"unknown", "error"} else None),
        "at": time.time(),
    }
    return result


def _unknown_provider(account_key, provider):
    return {
        "id": f"provider-version:{account_key}",
        "accountKey": account_key,
        "provider": provider,
        "status": "unknown",
        "runningVersion": None,
        "installedVersion": None,
        "configuredVersion": None,
        "baseline": TESTED_BASELINES.get(provider),
        "error": None,
        "message": "No evidence-backed version baseline is configured for this provider.",
        "at": time.time(),
    }


class ProviderVersionMonitor:
    """Check active provider sessions off-thread, guarded by connection identity."""

    def __init__(self, interval=CHECK_INTERVAL, read_version=None):
        self.interval = interval
        self.read_version = read_version
        self.lock = threading.Lock()
        self.checked_at = None
        self.next_check = 0.0
        self.signature = None
        self.generation = 0
        self.connections = {}
        self.providers = []
        self.worker = None

    def tick(self, runtime):
        with runtime.lock:
            offline = getattr(runtime, "offline_accounts", set())
            connected = [
                (key, server, runtime.connection_ids.get(key))
                for key, server in runtime.servers.items()
                if key not in offline
            ]
        signature = tuple((key, id(server), connection_id)
                          for key, server, connection_id in connected)
        with self.lock:
            changed = signature != self.signature
            if changed:
                self.generation += 1
                self.signature = signature
                next_connections = {key: (server, connection_id)
                                    for key, server, connection_id in connected}
                old = {row["accountKey"]: row for row in self.providers}
                self.providers = [
                    old[key] if key in old and self.connections.get(key) == (server, connection_id)
                    else {
                        "id": f"provider-version:{key}", "accountKey": key,
                        "provider": getattr(server, "provider", "unknown"),
                        "status": "checking", "runningVersion": None,
                        "installedVersion": None, "configuredVersion": None,
                        "baseline": TESTED_BASELINES.get(getattr(server, "provider", "")),
                        "error": None, "message": "Checking provider CLI version.",
                        "at": None,
                    }
                    for key, server, connection_id in connected
                ]
                self.connections = next_connections
                self.checked_at = None if connected else time.time()
                self.next_check = 0.0
            if self.worker and self.worker.is_alive():
                return
            if not changed and time.monotonic() < self.next_check:
                return
            self.next_check = time.monotonic() + self.interval
            generation = self.generation
            self.worker = threading.Thread(
                target=self._check, args=(runtime, connected, signature, generation),
                name="provider-version-check", daemon=True,
            )
            self.worker.start()

    @staticmethod
    def _is_current(runtime, signature):
        start_lock = getattr(runtime, "start_lock", None)
        if start_lock:
            start_lock.acquire()
        try:
            with runtime.lock:
                offline = getattr(runtime, "offline_accounts", set())
                current = tuple(
                    (key, id(server), runtime.connection_ids.get(key))
                    for key, server in runtime.servers.items()
                    if key not in offline
                )
        finally:
            if start_lock:
                start_lock.release()
        return current == signature

    def _check(self, runtime, connected, signature, generation):
        providers = []
        for account_key, server, connection_id in connected:
            if not self._is_current(runtime, signature):
                return
            try:
                account = runtime.accounts.get(account_key)
                provider = getattr(server, "provider", None) or account.get("provider", "codex")
                if provider not in TESTED_BASELINES or provider not in VERSION_READERS:
                    providers.append(_unknown_provider(account_key, provider))
                    continue
                versions = (self.read_version(provider, server, account) if self.read_version
                            else VERSION_READERS[provider](server, account))
                if not isinstance(versions, dict):
                    raise ValueError("Provider version reader returned no status object")
                diagnostic = _diagnostic(account_key, provider, versions)
                if not self._is_current(runtime, signature):
                    return
                providers.append(diagnostic)
            except (KeyError, OSError, ValueError, TypeError, subprocess.SubprocessError):
                if self._is_current(runtime, signature):
                    provider = getattr(server, "provider", "codex")
                    if provider in TESTED_BASELINES:
                        providers.append(_diagnostic(account_key, provider, {
                            "error": "Could not check the provider CLI version.",
                        }))
        self._publish(runtime, signature, generation, providers)

    def _publish(self, runtime, signature, generation, providers):
        # Runtime snapshots hold runtime.lock before reading our status. Keep
        # publication in the same order so a snapshot cannot wait on self.lock
        # while this worker waits on runtime.lock.
        start_lock = getattr(runtime, "start_lock", None)
        if start_lock:
            start_lock.acquire()
        try:
            with runtime.lock:
                offline = getattr(runtime, "offline_accounts", set())
                current = tuple(
                    (key, id(server), runtime.connection_ids.get(key))
                    for key, server in runtime.servers.items()
                    if key not in offline
                )
                with self.lock:
                    if generation != self.generation or current != signature:
                        return
                    self.providers = providers
                    self.checked_at = time.time()
        finally:
            if start_lock:
                start_lock.release()
        runtime.changed.set()

    def status(self):
        with self.lock:
            providers = [dict(item) for item in self.providers]
            return {
                "checkedAt": self.checked_at,
                "providers": providers,
                "warnings": [
                    {"id": row["id"], "accountKey": row["accountKey"],
                     "provider": row["provider"], "version": row.get("runningVersion") or
                     row.get("installedVersion") or row.get("configuredVersion"),
                     "baseline": row["baseline"], "message": row["message"], "at": row["at"]}
                    for row in providers
                    if row["status"] == "outdated" and row.get("message")
                ],
            }


def monitor(runtime):
    with runtime.lock:
        current = getattr(runtime, "provider_version_monitor", None)
        if current is None:
            current = runtime.provider_version_monitor = ProviderVersionMonitor()
        return current


def tick(runtime):
    monitor(runtime).tick(runtime)


def status(runtime):
    current = monitor(runtime)
    current.tick(runtime)
    return current.status()
