"""Cached local CodexBar estimates. Never interpret token estimates as a bill.

CodexBar owns token deltas, duplicate/fork reconciliation, and model pricing:
https://github.com/steipete/CodexBar/blob/main/docs/codex.md#cost-usage-local-log-scan
https://github.com/steipete/CodexBar/blob/main/docs/cli.md#cost-json-payload
No token totals from Runtime are added to the scanner totals.
"""

import copy
import hashlib
import json
import math
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import threading
import time


def amount(value):
    return (
        value
        if isinstance(value, (float, int))
        and not isinstance(value, bool)
        and math.isfinite(value)
        and value >= 0
        else None
    )


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
    if row.get("currencyCode", "USD") != "USD":
        raise ValueError("CodexBar returned an unsupported currency.")
    coverage = row.get("coverage") if isinstance(row.get("coverage"), dict) else {}
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
    # Older versions omit coverage. A claimed zero must not conceal missing rates.
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
        "coverage": status,
        "unknownModels": sorted(unknown),
        "historyDays": row.get("historyDays", 30),
        "note": "API-rate estimate, not your ChatGPT bill. CodexBar supplies pricing and cached-token accounting.",
    }


class CostReader:
    """One local scan at a time. GET requests return without waiting for the CLI."""

    def __init__(self, root, executable=None, interval=900, timeout=90):
        self.path = Path(root) / "local-costs.json"
        self.executable = executable or shutil.which("codexbar")
        if not self.executable:
            self.executable = next(
                (
                    p
                    for p in ("/opt/homebrew/bin/codexbar", "/usr/local/bin/codexbar")
                    if os.access(p, os.X_OK)
                ),
                None,
            )
        self.scope = hashlib.sha256(
            (
                str(
                    Path(os.environ.get("CODEX_HOME", "~/.codex"))
                    .expanduser()
                    .resolve()
                )
                + "\0"
                + str(self.executable)
            ).encode()
        ).hexdigest()
        self.interval, self.timeout = interval, timeout
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
            now = time.time()
            stale = (
                not amount(self.state.get("at"))
                or now - self.state["at"] >= self.interval
            )
            if (
                stale
                and not self.closed
                and not self.busy
                and now - self.attempt >= min(self.interval, 60)
            ):
                self.busy = True
                self.attempt = now
                threading.Thread(
                    target=self._refresh, name="local-costs", daemon=True
                ).start()
            return {
                **copy.deepcopy(self.state),
                "refreshing": self.busy,
                "stale": stale or bool(self.state.get("error")),
            }

    def _refresh(self):
        try:
            if not self.executable:
                raise ValueError("Install CodexBar CLI to read local cost estimates.")
            # The cost subcommand is local-only. Never invoke usage/login/cookie APIs.
            with tempfile.TemporaryFile() as output:
                with self.lock:
                    if self.closed:
                        return
                    self.process = subprocess.Popen(
                        [
                            self.executable,
                            "cost",
                            "--provider",
                            "codex",
                            "--format",
                            "json",
                        ],
                        stdout=output,
                        stderr=subprocess.DEVNULL,
                    )
                    process = self.process
                try:
                    code = process.wait(timeout=self.timeout)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
                    raise ValueError("CodexBar cost scan timed out.") from None
                if code:
                    raise ValueError(f"CodexBar cost scan failed (exit {code}).")
                if output.tell() > 32 * 1024 * 1024:
                    raise ValueError("CodexBar cost report exceeds 32 MiB.")
                output.seek(0)
                data = normalize(json.load(output))
            with self.lock:
                if not self.closed:
                    self.state = {"at": time.time(), "error": None, "data": data}
                    self._save()
        except (OSError, ValueError, TypeError) as error:
            with self.lock:
                self.state["error"] = str(error)[:300]
        finally:
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
