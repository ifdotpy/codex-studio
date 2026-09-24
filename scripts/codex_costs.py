"""Local cost estimates from native Codex profile logs.

The account reader uses the isolated helper in cost-scanner. Its upstream
parser owns token deltas, duplicate/fork reconciliation, and pricing.
No Runtime token totals are added to a scanner report. Estimates are not invoices.
"""

import copy
import hashlib
import json
import math
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import time
from codex_pricing import PricingCatalog, price_usage
from codex_claude_costs import parse_claude_usage


def amount(value):
    return (
        value
        if isinstance(value, (float, int))
        and not isinstance(value, bool)
        and math.isfinite(value)
        and value >= 0
        else None
    )


def source_day(row):
    days = []
    for day in row.get("daily") or []:
        if not isinstance(day, dict) or not isinstance(day.get("date"), str):
            continue
        value = day["date"]
        try:
            time.strptime(value, "%Y-%m-%d")
        except (TypeError, ValueError):
            continue
        days.append(value)
    return max(days) if days else None


def normalize(payload):
    """Expose only aggregate costs, never project paths or session contents."""
    rows = payload if isinstance(payload, list) else []
    row = next(
        (
            row
            for row in rows
            if isinstance(row, dict) and row.get("provider") == "codex"
        ),
        None,
    )
    if not row or row.get("error") or row.get("source") != "local":
        raise ValueError("CodexBar did not return local Codex costs.")
    if row.get("currencyCode") != "USD":
        raise ValueError("CodexBar returned an unsupported currency.")
    if not isinstance(row.get("coverage"), dict) or type(row.get("historyCoverageIsEstablished")) is not bool:
        raise ValueError("The local cost scanner returned invalid coverage.")
    coverage = row["coverage"]
    unknown = set()
    for day in row.get("daily") or []:
        if not isinstance(day, dict):
            continue
        breakdowns = day.get("modelBreakdowns") or []
        seen = set()
        for model in breakdowns:
            if not isinstance(model, dict):
                continue
            name = str(model.get("modelName", "Unknown model"))[:160]
            seen.add(name)
            cost = amount(model.get("cost"))
            # A zero with tokens is not evidence of free inference.
            if cost is None or (cost == 0 and amount(model.get("totalTokens")) != 0):
                unknown.add(name)
        unknown.update(
            str(name)[:160] for name in day.get("modelsUsed", []) if name not in seen
        )
    complete = row.get("historyCoverageIsEstablished") is True
    coverage_known = bool(coverage)
    unpriced = sum(
        value
        for key, value in coverage.items()
        if "unpriced" in key.lower() and amount(value) is not None
    )
    status = (
        "partial"
        if unknown or unpriced or row.get("historyCoverageIsEstablished") is False
        else "reported" if complete and coverage_known else "unverified"
    )
    today = amount(row.get("sessionCostUSD"))
    month = amount(row.get("last30DaysCostUSD"))
    # A claimed zero must not conceal missing rates.
    if today == 0 and (not complete or unknown or unpriced):
        today = None
    if month == 0 and (not complete or unknown or unpriced):
        month = None
    return {
        "source": "CodexBar local logs",
        "scope": "Local Codex sessions on this computer, including supported pi sessions. All projects and accounts in those logs.",
        "kind": "api_estimate",
        "currency": "USD",
        "billedUSD": None,
        "todayUSD": today,
        "last30DaysUSD": month,
        "todayTokens": amount(row.get("sessionTokens")),
        "last30DaysTokens": amount(row.get("last30DaysTokens")),
        "sourceUpdatedAt": row.get("updatedAt"),
        "sourceDay": source_day(row),
        "coverage": status,
        "unknownModels": sorted(unknown),
        "historyDays": row.get("historyDays", 30),
        "note": "API-rate estimate, not your ChatGPT bill. Base rates apply when the scanner cannot resolve a published context tier.",
    }


def report_fingerprint(data):
    """Identify report content without treating the source timestamp as usage."""
    if not isinstance(data, dict):
        return None
    content = {
        key: data.get(key)
        for key in (
            "todayUSD",
            "last30DaysUSD",
            "todayTokens",
            "last30DaysTokens",
            "sourceDay",
            "coverage",
            "unknownModels",
            "historyDays",
        )
    }
    return hashlib.sha256(
        json.dumps(content, sort_keys=True, separators=(",", ":")).encode()
    ).hexdigest()


class CostReader:
    """One local scan at a time. GET requests return without waiting for the CLI."""

    # A first scan of a large profile (25 GB of sessions) takes minutes. A kill
    # discards its progress, so a short bound never let such a profile finish.
    def __init__(self, root, interval=120, timeout=900, clock=None, *, command, environment=None, scan_lock=None):
        if not callable(command):
            raise ValueError("A local cost scanner command is required.")
        self.environment = dict(environment) if environment is not None else None
        self.scan_lock = scan_lock
        self.command = command
        self.path = Path(root) / "local-costs.json"
        self.scope = hashlib.sha256(json.dumps([
            str(Path((self.environment or os.environ).get("CODEX_HOME", "~/.codex")).expanduser().resolve()),
            str(Path(root).resolve()),
        ]).encode()).hexdigest()
        self.interval, self.timeout = max(1, interval), timeout
        self.clock = clock or time.time
        self.lock = threading.RLock()
        self.process = None
        self.closed = False
        self.busy = False
        self.state = {"at": None, "error": None, "data": None}
        try:
            cached = json.loads(self.path.read_text())
            if cached.get("scope") == self.scope and isinstance(
                cached.get("state"), dict
            ):
                self.state = cached["state"]
        except (OSError, ValueError, TypeError):
            pass
        self.attempt = 0

    def snapshot(self):
        with self.lock:
            now = self.clock()
            checked_at = amount(self.state.get("checkedAt"))
            if checked_at is None:
                checked_at = amount(self.state.get("at"))
            stale = (
                not checked_at
                or now - checked_at >= self.interval
                or bool(self.state.get("error"))
            )
            if (
                stale
                and not self.closed
                and not self.busy
                and now - self.attempt >= self.interval
            ):
                self.busy = True
                self.attempt = now
                threading.Thread(
                    target=self._refresh, name="local-costs", daemon=True
                ).start()
            state = copy.deepcopy(self.state)
            data = state.get("data")
            current_day = time.strftime("%Y-%m-%d", time.localtime(now))
            if isinstance(data, dict) and (
                not data.get("sourceDay") or data.get("sourceDay") != current_day
            ):
                data["todayUSD"] = None
                data["todayTokens"] = None
                stale = True
            return {**state, "refreshing": self.busy, "stale": stale}

    def _refresh(self):
        scan_lock = self.scan_lock
        acquired = False
        try:
            if scan_lock is not None:
                acquired = scan_lock.acquire(timeout=self.timeout)
                if not acquired:
                    raise ValueError("Local cost scanner is busy.")
            with self.lock:
                if self.closed:
                    return
            command = self.command()
            if not isinstance(command, list) or not command or any(not isinstance(part, str) for part in command):
                raise ValueError("The local cost scanner command is invalid.")
            with tempfile.TemporaryFile() as output:
                with self.lock:
                    if self.closed:
                        return
                    self.process = subprocess.Popen(
                        command,
                        stdout=output,
                        stderr=subprocess.DEVNULL,
                        env=self.environment,
                    )
                    process = self.process
                try:
                    code = process.wait(timeout=self.timeout)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
                    raise ValueError("Local cost scan timed out.") from None
                if code:
                    raise ValueError(f"Local cost scan failed (exit {code}).")
                if output.tell() > 32 * 1024 * 1024:
                    raise ValueError("Local cost report exceeds 32 MiB.")
                output.seek(0)
                data = normalize(json.load(output))
                fingerprint = report_fingerprint(data)
            with self.lock:
                if not self.closed:
                    previous = self.state.get("data")
                    previous_fingerprint = self.state.get("sourceFingerprint")
                    if previous_fingerprint is None:
                        previous_fingerprint = report_fingerprint(previous)
                    changed = previous is None or fingerprint != previous_fingerprint
                    finished_at = self.clock()
                    self.state = {
                        "at": (
                            finished_at
                            if changed or not amount(self.state.get("at"))
                            else self.state.get("at")
                        ),
                        "checkedAt": finished_at,
                        "error": None,
                        "data": data if changed else previous,
                        "sourceFingerprint": fingerprint,
                    }
                    self._save()
        except (OSError, ValueError, TypeError, subprocess.TimeoutExpired) as error:
            with self.lock:
                self.state["error"] = str(error)[:300]
                self.state["checkedAt"] = self.clock()
        finally:
            if acquired:
                scan_lock.release()
            with self.lock:
                self.process = None
                self.busy = False

    def _save(self):
        self.path.parent.mkdir(parents=True, exist_ok=True)
        fd, temporary = tempfile.mkstemp(prefix=".local-costs-", dir=self.path.parent)
        try:
            with os.fdopen(fd, "w") as stream:
                json.dump({"scope": self.scope, "state": self.state}, stream)
            os.replace(temporary, self.path)
        finally:
            if os.path.exists(temporary):
                os.unlink(temporary)

    def close(self):
        with self.lock:
            self.closed = True
            if self.process and self.process.poll() is None:
                self.process.terminate()


class AccountCostReader:
    """Account profile logs and caches. No native API or ambient Pi scan."""

    def __init__(self, root, accounts, *, reader_factory=CostReader, command_factory=None, pricing=None):
        self.root = Path(root) / "account-costs"
        self.accounts = accounts
        self.readers = {}
        self.lock = threading.RLock()
        self.scan_lock = threading.Lock()
        self.reader_factory = reader_factory
        self.command_factory = command_factory or self.command
        self.pricing = pricing or PricingCatalog(root)
        self.claude_readers = {}
        self.closed = False

    def command(self, home, cache):
        executable = self.root / "bin" / "codex-cost-scanner"
        builder = Path(__file__).with_name("cost-scanner") / "build.py"
        result = subprocess.run([sys.executable, str(builder), "--output", str(executable)],
                                stdout=subprocess.DEVNULL, stderr=subprocess.PIPE,
                                timeout=120, text=True)
        if result.returncode:
            raise ValueError("Cannot prepare the local cost scanner.")
        return [str(executable), "--home", str(home), "--cache", str(cache)]

    def snapshot(self, account_key="default"):
        account = self.accounts.get(account_key)
        if account.get("status") != "ready":
            raise ValueError("This account's local cost history is unavailable.")
        if account.get("provider") == "claude" or account_key == "claude-local":
            with self.lock:
                if self.closed:
                    raise ValueError("The local cost reader is closed.")
                reader = self.claude_readers.get(account_key)
                if reader is None:
                    config = Path(account.get("home") or Path.home() / ".claude").expanduser().resolve()
                    reader = ClaudeCostReader(self.root / "claude" / hashlib.sha256((account_key + str(config)).encode()).hexdigest(),
                                              config, self.pricing)
                    self.claude_readers[account_key] = reader
            return {**reader.snapshot(), "accountKey": account_key}
        home = Path(account["home"]).expanduser().resolve()
        identity = [account_key, account.get("accountId"), str(home)]
        scope = hashlib.sha256(json.dumps(identity).encode()).hexdigest()
        with self.lock:
            if self.closed:
                raise ValueError("The local cost reader is closed.")
            reader = self.readers.get(scope)
            if reader is None:
                cache = self.root / scope
                reader = self.reader_factory(cache, environment={**os.environ, "CODEX_HOME": str(home)},
                    scan_lock=self.scan_lock, command=lambda: self._command(home, cache / "scanner"))
                self.readers[scope] = reader
            value = reader.snapshot()
        data = value.get("data")
        if data is not None:
            data.update(accountId=account.get("accountId"), source="Codex profile logs",
                        scope="account", note="API-rate estimate", kind="api_estimate")
        return {**value, "accountKey": account_key}

    def _command(self, home, cache):
        self.pricing.wait_ready()
        self.pricing.write_scanner_copy(cache)
        return self.command_factory(home, cache)

    def close(self):
        with self.lock:
            self.closed = True
            for reader in self.readers.values():
                reader.close()


class ClaudeCostReader:
    """Cached estimates from one Claude Code configuration directory."""
    def __init__(self, state, config_dir, pricing, *, clock=time.time):
        self.state_path = Path(state) / "claude-costs.json"
        self.config_dir = Path(config_dir)
        self.pricing = pricing
        self.clock = clock
        self.lock = threading.RLock()
        self.files = {}
        self.data = None
        self.checked = 0
        self.error = None
        try:
            cached = json.loads(self.state_path.read_text())
            if cached.get("config") == str(self.config_dir):
                self.files = cached.get("files", {})
                self.data = cached.get("data")
                self.checked = cached.get("checked", 0)
        except (OSError, ValueError, TypeError):
            pass

    def snapshot(self):
        with self.lock:
            if self.clock() - self.checked >= 300:
                threading.Thread(target=self._refresh, name="claude-costs", daemon=True).start()
                self.checked = self.clock()
            return {"at": self.checked or None, "error": self.error,
                    "data": self.data, "refreshing": self.clock() - self.checked < 2,
                    "stale": self.data is None or self.error is not None}

    def _refresh(self):
        try:
            catalog = self.pricing.wait_ready()
            if not self.config_dir.is_dir():
                raise ValueError("Claude account logs are unavailable")
            previous = self.files
            files = {}
            now = self.clock()
            today = time.strftime("%Y-%m-%d", time.localtime(now))
            day_start = time.mktime(time.strptime(today, "%Y-%m-%d"))
            entries = []
            for path in self.config_dir.glob("projects/**/*.jsonl"):
                try:
                    stat = path.stat()
                    signature = [stat.st_size, stat.st_mtime_ns]
                    cached = previous.get(str(path))
                    if cached and cached.get("signature") == signature:
                        rows = cached.get("rows", [])
                    else:
                        rows = parse_claude_usage(path)
                    files[str(path)] = {"signature": signature, "rows": rows}
                    entries.extend(rows)
                except OSError:
                    continue
            unique = {}
            for row in entries:
                unique.setdefault(row["id"], row)
            totals = {"today": 0.0, "month": 0.0}
            unknown, groups = set(), {}
            priced_any = {"today": False, "month": False}
            for row in unique.values():
                period = ("today" if row["at"] >= day_start else
                          "month" if row["at"] >= now - 30 * 86400 else None)
                if not period:
                    continue
                cost, status, _ = price_usage(catalog, "anthropic", row["model"], row["usage"],
                                               context_tokens=row["usage"].get("inputTokens"))
                if cost is None:
                    unknown.add(row["model"])
                    continue
                totals[period] += cost
                priced_any[period] = True
                groups[row["model"]] = groups.get(row["model"], 0.0) + cost
            data = {"source": "Claude Code local logs", "scope": "account", "kind": "api_estimate",
                    "currency": "USD", "billedUSD": None,
                    "todayUSD": (totals["today"] if priced_any["today"] or not unknown else None) if unique else None,
                    "last30DaysUSD": (totals["month"] + totals["today"] if priced_any["month"] or priced_any["today"] or not unknown else None) if unique else None,
                    "coverage": "partial" if unknown else "reported" if unique else "unverified", "unknownModels": sorted(unknown),
                    "modelBreakdown": groups,
                    "note": "API-rate estimate. Uses base rates when request size is unavailable."}
            self.state_path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
            content_changed = self.data != data or {
                key: value.get("rows", []) for key, value in self.files.items()
            } != {key: value.get("rows", []) for key, value in files.items()}
            if content_changed:
                payload = {"config": str(self.config_dir), "files": files, "data": data, "checked": now}
                tmp = self.state_path.with_name("." + self.state_path.name + ".tmp")
                tmp.write_text(json.dumps(payload, separators=(",", ":")))
                os.chmod(tmp, 0o600)
                os.replace(tmp, self.state_path)
            with self.lock:
                self.files, self.data, self.error, self.checked = files, data, None, now
        except Exception as error:
            with self.lock:
                self.error = str(error)[:300]
