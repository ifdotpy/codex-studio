"""Read-only API price estimates for one managed team."""
from collections import OrderedDict
import hashlib
import json
import os
from pathlib import Path
import re
import sqlite3
import tempfile
import threading
import time

from codex_claude_costs import parse_claude_usage
from codex_pricing import price_usage


def provider_for(model):
    if not isinstance(model, str):
        return None
    if model.startswith("claude-"):
        return "anthropic"
    if model.startswith(("gpt-", "o1", "o3", "o4")):
        return "openai"
    return None


class SessionCostReader:
    CACHE_SECONDS = 30
    CACHE_ROOTS = 16

    def __init__(self, db_path, pricing, accounts=None, *, state_root=None, clock=time.time):
        self.db_path = Path(db_path)
        self.state_root = Path(state_root) if state_root else self.db_path.parent
        self.pricing = pricing
        self.accounts = accounts
        self.clock = clock
        self.cache = OrderedDict()
        self.file_cache = OrderedDict()
        self.refreshing = set()
        self.last_refresh_attempt = OrderedDict()
        self.lock = threading.RLock()

    def _cache_path(self, root):
        digest = hashlib.sha256(root.encode("utf-8")).hexdigest()
        return self.state_root / "session-costs" / ("cost-" + digest + ".json")

    def _valid_cached(self, root, value):
        return (isinstance(value, dict) and value.get("rootId") == root
                and value.get("pricingState") == "ready"
                and isinstance(value.get("unknownModels"), list)
                and isinstance(value.get("breakdown"), dict)
                and (value.get("totalUSD") is None or isinstance(value.get("totalUSD"), (int, float))))

    def _remember(self, root, result, cached_at=None):
        cached_at = self.clock() if cached_at is None else cached_at
        with self.lock:
            self.cache[root] = (cached_at, result)
            self.cache.move_to_end(root)
            while len(self.cache) > self.CACHE_ROOTS:
                self.cache.popitem(last=False)
        try:
            self._persist(root, result, cached_at)
        except OSError:
            pass

    def _persist(self, root, result, cached_at):
        path = self._cache_path(root)
        path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
        fd, temporary = tempfile.mkstemp(prefix=".session-cost-", dir=path.parent)
        try:
            with os.fdopen(fd, "w", encoding="utf-8") as output:
                json.dump({"version": 1, "rootId": root, "cachedAt": cached_at,
                           "result": result}, output, separators=(",", ":"))
                output.flush()
                os.fsync(output.fileno())
            os.chmod(temporary, 0o600)
            os.replace(temporary, path)
            try:
                cached_files = sorted(path.parent.glob("cost-*.json"), key=lambda item: item.stat().st_mtime)
                for old in cached_files[:-self.CACHE_ROOTS]:
                    old.unlink(missing_ok=True)
            except OSError:
                pass
        finally:
            if os.path.exists(temporary):
                os.unlink(temporary)

    def _load_persisted(self, root):
        path = self._cache_path(root)
        try:
            saved = json.loads(path.read_text())
            cached_at = saved.get("cachedAt")
            result = saved.get("result")
            if (saved.get("version") != 1 or saved.get("rootId") != root
                    or not isinstance(cached_at, (int, float)) or not self._valid_cached(root, result)):
                return None
            try:
                os.utime(path, None)
            except OSError:
                pass
            return cached_at, result
        except (OSError, ValueError, TypeError, AttributeError):
            return None

    def _start_refresh(self, agent_id, root):
        with self.lock:
            now = self.clock()
            last = self.last_refresh_attempt.get(root)
            if root in self.refreshing or (last is not None and now - last < self.CACHE_SECONDS):
                return False
            self.refreshing.add(root)
            self.last_refresh_attempt[root] = now
            self.last_refresh_attempt.move_to_end(root)
            while len(self.last_refresh_attempt) > self.CACHE_ROOTS:
                self.last_refresh_attempt.popitem(last=False)
        try:
            threading.Thread(target=self._background_refresh, args=(agent_id, root),
                             name="session-cost-refresh", daemon=True).start()
        except RuntimeError:
            with self.lock:
                self.refreshing.discard(root)
            return False
        return True

    def _background_refresh(self, agent_id, root):
        try:
            result = self._compute(agent_id, root)
            if result.get("pricingState") == "ready":
                self._remember(root, result)
        except Exception:
            pass
        finally:
            with self.lock:
                self.refreshing.discard(root)

    def snapshot(self, agent_id):
        db = sqlite3.connect(f"file:{self.db_path}?mode=ro", uri=True, timeout=3)
        db.row_factory = sqlite3.Row
        try:
            row = db.execute("SELECT record FROM analytics_agents WHERE id=?", (agent_id,)).fetchone()
            if not row:
                row = db.execute("SELECT record FROM runtime_agents WHERE id=?", (agent_id,)).fetchone()
            if not row:
                raise ValueError("Unknown chat")
            root = json.loads(row["record"]).get("rootId") or agent_id
        finally:
            db.close()
        with self.lock:
            cached = self.cache.get(root)
            if cached:
                self.cache.move_to_end(root)
        if not cached:
            cached = self._load_persisted(root)
            if cached:
                with self.lock:
                    self.cache[root] = cached
                    self.cache.move_to_end(root)
                    while len(self.cache) > self.CACHE_ROOTS:
                        self.cache.popitem(last=False)
        if cached:
            cached_at, result = cached
            age = max(0, self.clock() - cached_at)
            started = self._start_refresh(agent_id, root) if age >= self.CACHE_SECONDS else False
            with self.lock:
                refreshing = started or root in self.refreshing
            return {**result, "cacheAgeSeconds": round(age, 1), "refreshing": refreshing}
        result = self._compute(agent_id, root)
        if result.get("pricingState") == "ready":
            self._remember(root, result)
        return {**result, "cacheAgeSeconds": 0, "refreshing": False}

    def _account(self, key):
        if self.accounts is None:
            return {}
        guard = getattr(self.accounts, "lock", None)
        if guard:
            with guard:
                row = getattr(self.accounts, "data", {}).get("accounts", {}).get(key)
                return dict(row) if isinstance(row, dict) else {}
        accounts = self.accounts if isinstance(self.accounts, dict) else {}
        row = accounts.get(key)
        return dict(row) if isinstance(row, dict) else {}

    def _claude_profile(self, key):
        account = self._account(key)
        if account.get("provider") != "claude":
            return None
        options = account.get("claudeOptions") if isinstance(account.get("claudeOptions"), dict) else {}
        config = options.get("configDir") or account.get("home") or os.environ.get("CLAUDE_CONFIG_DIR") or str(Path.home() / ".claude")
        return Path(config).expanduser().resolve()

    def _thread_ids(self, config, account_key, thread_id):
        if not isinstance(thread_id, str) or not re.fullmatch(r"[A-Za-z0-9-]{1,100}", thread_id):
            return []
        if not isinstance(account_key, str) or not re.fullmatch(r"[A-Za-z0-9-]{1,100}", account_key):
            return []
        ids = {thread_id}
        server_root = self.state_root if account_key == "default" else self.state_root / "account-servers" / account_key
        state_file = server_root / "sessions" / (thread_id + ".json")
        try:
            if state_file.stat().st_size <= 16 * 1024 * 1024:
                data = json.loads(state_file.read_text())
                for value in (data.get("nativeId"),):
                    if isinstance(value, str) and re.fullmatch(r"[A-Za-z0-9-]{1,100}", value):
                        ids.add(value)
                for branch in data.get("historyBranches", []):
                    value = branch.get("nativeId") if isinstance(branch, dict) else None
                    if isinstance(value, str) and re.fullmatch(r"[A-Za-z0-9-]{1,100}", value):
                        ids.add(value)
                for control in data.get("controlRequests", {}).values():
                    if not isinstance(control, dict):
                        continue
                    for key in ("sourceNativeId", "targetNativeId"):
                        value = control.get(key)
                        if isinstance(value, str) and re.fullmatch(r"[A-Za-z0-9-]{1,100}", value):
                            ids.add(value)
        except (OSError, ValueError, TypeError, AttributeError):
            pass
        files = []
        projects = config / "projects"
        for session_id in ids:
            if not re.fullmatch(r"[A-Za-z0-9-]{1,100}", session_id):
                continue
            files.extend(projects.glob("*/" + session_id + ".jsonl"))
        return files

    def _log_rows(self, path):
        try:
            stat = path.stat()
        except OSError:
            return []
        key, signature = str(path), (stat.st_size, stat.st_mtime_ns)
        with self.lock:
            cached = self.file_cache.get(key)
            if cached and cached[0] == signature:
                self.file_cache.move_to_end(key)
                return cached[1]
        try:
            rows = parse_claude_usage(path)
        except OSError:
            return []
        with self.lock:
            self.file_cache[key] = (signature, rows)
            self.file_cache.move_to_end(key)
            while len(self.file_cache) > 4096:
                self.file_cache.popitem(last=False)
        return rows

    def _compute(self, agent_id, root):
        db = sqlite3.connect(f"file:{self.db_path}?mode=ro", uri=True, timeout=3)
        db.row_factory = sqlite3.Row
        try:
            catalog = self.pricing.snapshot()
            if catalog is None:
                return {"rootId": root, "totalUSD": None, "pricedSamples": 0,
                        "breakdown": {"providers": {}, "models": {}}, "unknownModels": [],
                        "estimated": True, "pricingState": "loading", "cacheAgeSeconds": 0,
                        "method": "Loading public API prices."}
            try:
                meta = db.execute("SELECT value FROM analytics_meta WHERE key='usageGeneration'").fetchone()
                generation = int(meta[0]) if meta else 0
            except sqlite3.OperationalError:
                generation = 0
            agents = []
            try:
                for entry in db.execute("SELECT id,record FROM analytics_agents WHERE id=? OR json_extract(record,'$.rootId')=?",
                                        (root, root)):
                    record = json.loads(entry["record"])
                    if record.get("rootId") == root or entry["id"] == root:
                        agents.append((entry["id"], record))
            except sqlite3.OperationalError:
                agents = [(agent_id, agent)]
            if not any(key == agent_id for key, _ in agents):
                agents.append((agent_id, agent))
            claude_agents = {}
            for member_id, member in agents:
                current_key = member.get("accountKey", "default")
                current_profile = self._claude_profile(current_key)
                sessions = set()
                if current_profile and isinstance(member.get("threadId"), str):
                    sessions.add((current_key, member["threadId"]))
                for history in member.get("accountHistory", []):
                    if not isinstance(history, dict) or history.get("provider") != "claude":
                        continue
                    old_key, old_thread = history.get("accountKey"), history.get("threadId")
                    if isinstance(old_key, str) and isinstance(old_thread, str) and self._claude_profile(old_key):
                        sessions.add((old_key, old_thread))
                if current_profile or sessions:
                    claude_agents[member_id] = (current_key, current_profile, sessions)
            try:
                usage_rows = db.execute("SELECT agent,thread,turn,record FROM analytics_usage WHERE root=? ORDER BY at,seq", (root,)).fetchall()
            except sqlite3.OperationalError:
                usage_rows = []
            turns = {(item["agent"], item["thread"], item["turn"])
                     for item in usage_rows if (json.loads(item["record"]).get("responseId")
                                                and item["agent"] not in claude_agents)}
            records = []
            for item in usage_rows:
                if item["agent"] in claude_agents:
                    continue
                record = json.loads(item["record"])
                if not record.get("responseId") and (item["agent"], item["thread"], item["turn"]) in turns:
                    continue
                records.append(record)
            cost_total, model_totals, unpriced, provider_totals = 0.0, {}, set(), {}
            priced_count = 0
            tier_used = False
            for record in records:
                model = record.get("model")
                provider = provider_for(model)
                if provider is None:
                    unpriced.add(str(model or "Unknown model"))
                    continue
                usage = record.get("delta") or {}
                if not all(isinstance(usage.get(field), (int, float)) for field in ("inputTokens", "outputTokens")):
                    usage = record.get("last") or {}
                cost, _, tier = price_usage(catalog, provider, model, usage,
                                            context_tokens=usage.get("inputTokens"))
                if cost is None:
                    unpriced.add(str(model))
                    continue
                priced_count += 1
                cost_total += cost
                model_totals[model] = model_totals.get(model, 0.0) + cost
                provider_totals[provider] = provider_totals.get(provider, 0.0) + cost
                tier_used |= tier
            claude_messages = {}
            for _, (current_key, current_profile, sessions) in claude_agents.items():
                for account_key, thread_id in sessions:
                    config = self._claude_profile(account_key)
                    if not config:
                        continue
                    for path in self._thread_ids(config, account_key, thread_id):
                        for message in self._log_rows(path):
                            claude_messages.setdefault(message["id"], message)
            for record in claude_messages.values():
                model = record["model"]
                cost, _, tier = price_usage(catalog, "anthropic", model, record["usage"],
                                            context_tokens=record["usage"].get("inputTokens"))
                if cost is None:
                    unpriced.add(model)
                    continue
                priced_count += 1
                cost_total += cost
                model_totals[model] = model_totals.get(model, 0.0) + cost
                provider_totals["anthropic"] = provider_totals.get("anthropic", 0.0) + cost
                tier_used |= tier
            result = {"rootId": root, "generation": generation,
                      "totalUSD": cost_total if priced_count else None,
                      "pricedSamples": priced_count,
                      "breakdown": {"providers": provider_totals, "models": model_totals},
                      "unknownModels": sorted(unpriced), "estimated": True, "pricingState": "ready",
                      "cacheAgeSeconds": 0,
                      "method": "API prices from models.dev; cached input rates are applied when reported." +
                                (" Published context tier applied where request size matched its threshold." if tier_used else " Base rates used when request size was unavailable or below the published threshold.")}
            with self.lock:
                self.cache[root] = (self.clock(), result)
                self.cache.move_to_end(root)
                while len(self.cache) > self.CACHE_ROOTS:
                    self.cache.popitem(last=False)
            return result
        finally:
            db.close()
