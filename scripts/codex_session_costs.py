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
        self.inflight = {}
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
            self._compute_shared(agent_id, root, refresh=True)
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
        result = self._compute_shared(agent_id, root)
        return {**result, "cacheAgeSeconds": 0, "refreshing": False}

    def _compute_shared(self, agent_id, root, *, refresh=False):
        with self.lock:
            cached = self.cache.get(root)
            if cached is not None and not refresh:
                return cached[1]
            pending = self.inflight.get(root)
            if pending is None:
                pending = {"event": threading.Event(), "result": None, "error": None}
                self.inflight[root] = pending
                owner = True
            else:
                owner = False
        if not owner:
            pending["event"].wait()
            if pending["error"] is not None:
                raise pending["error"]
            return pending["result"]
        try:
            result = self._compute(agent_id, root)
            if result.get("pricingState") == "ready":
                self._remember(root, result)
            pending["result"] = result
            return result
        except BaseException as error:
            pending["error"] = error
            raise
        finally:
            with self.lock:
                self.inflight.pop(root, None)
                pending["event"].set()

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
                agents = []
            if not any(key == agent_id for key, _ in agents):
                # A new chat can reach the runtime before analytics records it.
                row = db.execute("SELECT record FROM runtime_agents WHERE id=?", (agent_id,)).fetchone()
                if not row:
                    raise ValueError("Unknown chat")
                agents.append((agent_id, json.loads(row["record"])))
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
            cost_total, model_totals, unpriced, provider_totals = 0.0, {}, set(), {}
            priced_count = 0
            tier_used = False
            db.execute("PRAGMA temp_store=FILE")
            db.execute("CREATE TEMP TABLE IF NOT EXISTS session_cost_excluded (agent TEXT PRIMARY KEY)")
            db.execute("DELETE FROM session_cost_excluded")
            db.executemany("INSERT OR IGNORE INTO session_cost_excluded VALUES (?)",
                           ((member_id,) for member_id in claude_agents))
            try:
                groups = db.execute("""
                  WITH usage AS MATERIALIZED (
                    SELECT seq, agent, thread, turn, at,
                           json_extract(record,'$.agentId') AS record_agent,
                           json_extract(record,'$.turnId') AS record_turn,
                           json_extract(record,'$.threadId') AS record_thread,
                           CASE WHEN json_type(record,'$.model')='text' THEN json_extract(record,'$.model') END AS model,
                           json_type(record,'$.model')='text' AS has_model,
                           json_extract(record,'$.responseId') AS response_id,
                           json_type(record,'$.responseId') AS response_type,
                           (json_type(record,'$.delta.inputTokens') IN ('integer','real','true','false')
                            AND json_type(record,'$.delta.outputTokens') IN ('integer','real','true','false')) AS delta_valid,
                           json_type(record,'$.delta.inputTokens') AS delta_input_type,
                           json_type(record,'$.delta.outputTokens') AS delta_output_type,
                           json_type(record,'$.delta.cachedInputTokens') AS delta_cached_type,
                           json_type(record,'$.delta.cacheWriteInputTokens') AS delta_write_type,
                           json_extract(record,'$.delta.inputTokens') AS delta_input,
                           json_extract(record,'$.delta.outputTokens') AS delta_output,
                           json_extract(record,'$.delta.cachedInputTokens') AS delta_cached,
                           json_extract(record,'$.delta.cacheWriteInputTokens') AS delta_write,
                           json_extract(record,'$.last.inputTokens') AS last_input,
                           json_extract(record,'$.last.outputTokens') AS last_output,
                           json_extract(record,'$.last.cachedInputTokens') AS last_cached,
                           json_extract(record,'$.last.cacheWriteInputTokens') AS last_write,
                           json_type(record,'$.last.inputTokens') AS last_input_type,
                           json_type(record,'$.last.outputTokens') AS last_output_type,
                           json_type(record,'$.last.cachedInputTokens') AS last_cached_type,
                           json_type(record,'$.last.cacheWriteInputTokens') AS last_write_type
                      FROM analytics_usage
                     WHERE root=? AND NOT EXISTS
                           (SELECT 1 FROM session_cost_excluded x WHERE x.agent=analytics_usage.agent)
                  ), inferred AS MATERIALIZED (
                    SELECT u.*,
                           CASE WHEN has_model THEN model ELSE COALESCE(
                             NULLIF((SELECT m.model FROM usage m
                                      WHERE m.record_agent IS u.record_agent AND m.record_turn IS u.record_turn
                                        AND m.has_model ORDER BY m.at DESC,m.seq DESC LIMIT 1),''),
                             (SELECT m.model FROM usage m
                               WHERE m.record_thread IS u.record_thread AND m.has_model
                               ORDER BY m.at DESC,m.seq DESC LIMIT 1)) END AS resolved_model,
                           CASE WHEN delta_valid AND delta_input_type IN ('integer','real') THEN delta_input
                                WHEN NOT delta_valid AND last_input_type IN ('integer','real') THEN last_input END AS input_tokens,
                           CASE WHEN delta_valid AND delta_output_type IN ('integer','real') THEN delta_output
                                WHEN NOT delta_valid AND last_output_type IN ('integer','real') THEN last_output END AS output_tokens,
                           CASE WHEN delta_valid AND delta_cached_type IN ('integer','real') THEN delta_cached
                                WHEN NOT delta_valid AND last_cached_type IN ('integer','real') THEN last_cached END AS cached_tokens,
                           CASE WHEN delta_valid AND delta_write_type IN ('integer','real') THEN delta_write
                                WHEN NOT delta_valid AND last_write_type IN ('integer','real') THEN last_write END AS write_tokens
                      FROM usage u
                  )
                  SELECT resolved_model,input_tokens,cached_tokens,write_tokens,output_tokens,COUNT(*)
                    FROM inferred u
                   WHERE (response_type NOT IN ('null','false')
                          AND (response_type NOT IN ('integer','real') OR response_id<>0)
                          AND (response_type<>'text' OR response_id<>'')) OR NOT EXISTS (
                         SELECT 1 FROM usage r WHERE r.agent=u.agent AND r.thread IS u.thread AND r.turn IS u.turn
                           AND r.response_type NOT IN ('null','false')
                           AND (r.response_type NOT IN ('integer','real') OR r.response_id<>0)
                           AND (r.response_type<>'text' OR r.response_id<>''))
                   GROUP BY resolved_model,input_tokens,cached_tokens,write_tokens,output_tokens
                """, (root,))
            except sqlite3.OperationalError as error:
                if "no such table: analytics_usage" not in str(error):
                    raise
                groups = ()
            for model, input_tokens, cached_tokens, write_tokens, output_tokens, count in groups:
                provider = provider_for(model)
                if provider is None:
                    unpriced.add(str(model or "Unknown model"))
                    continue
                usage = {"inputTokens": input_tokens, "cachedInputTokens": cached_tokens,
                         "cacheWriteInputTokens": write_tokens, "outputTokens": output_tokens}
                cost, status, tier = price_usage(catalog, provider, model, usage,
                                                 context_tokens=usage.get("inputTokens"))
                if cost is None:
                    unpriced.add(str(model))
                    if status == "unpriced":
                        self.pricing.refresh_missing()
                    continue
                priced_count += count
                cost_total += cost * count
                model_totals[model] = model_totals.get(model, 0.0) + cost * count
                provider_totals[provider] = provider_totals.get(provider, 0.0) + cost * count
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
            return result
        finally:
            db.close()
