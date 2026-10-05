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

from codex_sqlite import connect as sqlite_connect
from codex_claude_costs import parse_claude_usage
from codex_pricing import price_usage


def provider_for(model):
    if not isinstance(model, str):
        return None
    if model.startswith("claude-") or model.removesuffix("[1m]") in {"opus", "sonnet", "haiku", "fable"}:
        return "anthropic"
    if model.startswith(("gpt-", "o1", "o3", "o4")):
        return "openai"
    return None


def _publish_session_cost(state_dir: str | Path, agent_id: str) -> None:
    """Invalidate the requested chat after its async estimate settles."""
    from studio_api.sync.resources.hub import publish_resources
    from studio_api.sync.resources.models import ResourceRef, SessionCostResource

    publish_resources(state_dir, ResourceRef(SessionCostResource(kind="session-cost", agentId=agent_id)))


class SessionCostReader:
    CACHE_ROOTS = 16
    MAX_QUEUED_REFRESHES = 16

    def __init__(self, db_path, pricing, accounts=None, *, state_root=None, clock=time.time):
        self.db_path = Path(db_path)
        self.state_root = Path(state_root) if state_root else self.db_path.parent
        self.analytics_path = self.state_root / "analytics.sqlite3"
        self._separate_analytics = self.analytics_path.exists()
        self._runtime_prefix = "canvas." if self._separate_analytics else ""
        self.pricing = pricing
        self.accounts = accounts
        self.clock = clock
        self.cache = OrderedDict()
        self.file_cache = OrderedDict()
        self.refreshing = set()
        self.refresh_waiters = OrderedDict()
        self.refresh_queue = OrderedDict()
        self.active_refresh_root = None
        self.inflight = {}
        self.lock = threading.RLock()

    def _cache_path(self, root):
        digest = hashlib.sha256(root.encode("utf-8")).hexdigest()
        return self.state_root / "session-costs" / ("cost-" + digest + ".json")

    def _connect(self):
        path = self.analytics_path if self._separate_analytics else self.db_path
        db = sqlite_connect(f"file:{path}?mode=ro", uri=True, timeout=3,
                            site="SessionCostReader.analytics")
        db.row_factory = sqlite3.Row
        # Set storage before creating legacy views or opening a read snapshot.
        # Changing it later either rejects the transaction or deletes the views.
        db.execute("PRAGMA temp_store=FILE")
        if self._separate_analytics:
            db.execute("ATTACH DATABASE ? AS canvas", (self.db_path.absolute().as_uri() + "?mode=ro",))
            from codex_analytics_storage import install_legacy_read_views
            install_legacy_read_views(db)
        return db

    def _valid_cached(self, root, value):
        return (isinstance(value, dict) and value.get("rootId") == root
                and value.get("pricingState") == "ready"
                and isinstance(value.get("unknownModels"), list)
                and isinstance(value.get("breakdown"), dict)
                and (value.get("totalUSD") is None or isinstance(value.get("totalUSD"), (int, float))))

    @staticmethod
    def _catalog_signature(catalog):
        if catalog is None:
            return None
        encoded = json.dumps(catalog, sort_keys=True, separators=(",", ":"), ensure_ascii=False)
        return hashlib.sha256(encoded.encode("utf-8")).hexdigest()

    @staticmethod
    def _usage_state(db, root):
        try:
            row = db.execute("SELECT COALESCE(MAX(seq),0) FROM analytics_usage WHERE root=?", (root,)).fetchone()
            max_seq = row[0]
        except sqlite3.OperationalError as error:
            if "no such table: analytics_usage" not in str(error):
                raise
            max_seq = 0
        try:
            row = db.execute("SELECT generation FROM analytics_usage_roots WHERE root=?", (root,)).fetchone()
            generation = int(row[0]) if row else 0
        except sqlite3.OperationalError as error:
            if "no such table: analytics_usage_roots" not in str(error):
                raise
            generation = SessionCostReader._global_usage_generation(db)
        return {"maxSeq": max_seq, "generation": generation}

    @staticmethod
    def _global_usage_generation(db):
        try:
            row = db.execute("SELECT value FROM analytics_meta WHERE key='usageGeneration'").fetchone()
            return int(row[0]) if row else 0
        except sqlite3.OperationalError:
            return 0

    def _root_agents(self, db, agent_id, root):
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
            row = db.execute(f"SELECT record FROM {self._runtime_prefix}runtime_agents WHERE id=?", (agent_id,)).fetchone()
            if not row:
                raise ValueError("Unknown chat")
            agents.append((agent_id, json.loads(row["record"])))
        return agents

    def _claude_agents(self, agents):
        result = {}
        for member_id, member in agents:
            current_key = member.get("accountKey", "default")
            current_profile = self._claude_profile(current_key)
            sessions = set()
            if current_profile and isinstance(member.get("threadId"), str):
                sessions.add((current_key, member["threadId"]))
            for history in (member.get("accountHistory") or []):
                if not isinstance(history, dict) or history.get("provider") != "claude":
                    continue
                old_key, old_thread = history.get("accountKey"), history.get("threadId")
                if isinstance(old_key, str) and isinstance(old_thread, str) and self._claude_profile(old_key):
                    sessions.add((old_key, old_thread))
            if current_profile or sessions:
                result[member_id] = (current_key, current_profile, sessions)
        return result

    def _claude_signature(self, claude_agents):
        files = {}
        configs = []
        for current_key, current_profile, sessions in claude_agents.values():
            if current_profile:
                configs.append((current_key, str(current_profile)))
            for account_key, thread_id in sessions:
                config = self._claude_profile(account_key)
                if not config:
                    continue
                configs.append((account_key, str(config)))
                for path in self._thread_ids(config, account_key, thread_id):
                    try:
                        stat = path.stat()
                        files[str(path)] = (stat.st_dev, stat.st_ino, stat.st_size,
                                            stat.st_mtime_ns, stat.st_ctime_ns)
                    except OSError:
                        files[str(path)] = (None, None)
        encoded = json.dumps({"configs": sorted(set(configs)), "files": sorted(files.items())}, separators=(",", ":"))
        return hashlib.sha256(encoded.encode("utf-8")).hexdigest()

    @staticmethod
    def _agent_signature(agents):
        identities = [(key, {field: record.get(field) for field in
                             ("rootId", "accountKey", "provider", "threadId", "accountHistory")})
                      for key, record in agents]
        encoded = json.dumps(sorted(identities, key=lambda value: value[0]), sort_keys=True,
                             separators=(",", ":"), ensure_ascii=False)
        return hashlib.sha256(encoded.encode("utf-8")).hexdigest()

    def _remember(self, root, result, source, cached_at=None):
        cached_at = self.clock() if cached_at is None else cached_at
        with self.lock:
            self.cache[root] = (cached_at, result, source)
            self.cache.move_to_end(root)
            while len(self.cache) > self.CACHE_ROOTS:
                self.cache.popitem(last=False)
        try:
            self._persist(root, result, source, cached_at)
        except OSError:
            pass

    def _persist(self, root, result, source, cached_at):
        path = self._cache_path(root)
        path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
        fd, temporary = tempfile.mkstemp(prefix=".session-cost-", dir=path.parent)
        try:
            with os.fdopen(fd, "w", encoding="utf-8") as output:
                json.dump({"version": 2, "rootId": root, "cachedAt": cached_at,
                           "source": source, "result": result}, output, separators=(",", ":"))
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

    def _load_persisted(self, root, usage_state, pricing_signature, agent_signature, claude_signature):
        path = self._cache_path(root)
        try:
            saved = json.loads(path.read_text())
            cached_at = saved.get("cachedAt")
            source = saved.get("source")
            result = saved.get("result")
            if (saved.get("version") != 2 or saved.get("rootId") != root
                    or not isinstance(cached_at, (int, float)) or not self._valid_cached(root, result)):
                return None
            if (not isinstance(source, dict) or source.get("usage") != usage_state
                    or source.get("pricingSignature") != pricing_signature
                    or source.get("agentsSignature") != agent_signature
                    or source.get("claudeSignature") != claude_signature):
                return None
            try:
                os.utime(path, None)
            except OSError:
                pass
            return cached_at, result, source
        except (OSError, ValueError, TypeError, AttributeError):
            return None

    def _start_refresh(self, agent_id, root, *, min_interval=0):
        start_now = False
        notify_overflow = False
        with self.lock:
            # A root stays refreshing while queued or active. The queue is
            # bounded; waiter maps exist only for these transient refreshes.
            if root in self.refreshing:
                waiters = self.refresh_waiters.setdefault(root, OrderedDict())
                waiters[agent_id] = None
                waiters.move_to_end(agent_id)
                return False
            checked = self.__dict__.setdefault("refresh_checks", OrderedDict()).get(root)
            if checked is not None and self.clock() - checked < min_interval:
                return False
            if (self.active_refresh_root is not None
                    and len(self.refresh_queue) >= self.MAX_QUEUED_REFRESHES):
                error = RuntimeError("Session cost refresh queue is full")
                errors = self.__dict__.setdefault("refresh_errors", OrderedDict())
                errors[root] = error
                errors.move_to_end(root)
                while len(errors) > self.CACHE_ROOTS:
                    errors.popitem(last=False)
                checks = self.__dict__.setdefault("refresh_checks", OrderedDict())
                checks[root] = self.clock()
                checks.move_to_end(root)
                while len(checks) > self.CACHE_ROOTS:
                    checks.popitem(last=False)
                notify_overflow = True
            else:
                self.refreshing.add(root)
                self.refresh_waiters[root] = OrderedDict(((agent_id, None),))
                self.refresh_waiters.move_to_end(root)
                if self.active_refresh_root is None:
                    self.active_refresh_root = root
                    start_now = True
                else:
                    self.refresh_queue[root] = None
                    self.refresh_queue.move_to_end(root)
        if notify_overflow:
            _publish_session_cost(self.state_root, agent_id)
            return False
        if not start_now:
            return True
        return self._launch_refresh(root, agent_id)

    def _launch_refresh(self, root, agent_id):
        try:
            threading.Thread(target=self._background_refresh, args=(agent_id, root),
                             name="session-cost-refresh", daemon=True).start()
        except RuntimeError:
            with self.lock:
                errors = self.__dict__.setdefault("refresh_errors", OrderedDict())
                errors[root] = RuntimeError("Could not start session cost refresh")
                errors.move_to_end(root)
                while len(errors) > self.CACHE_ROOTS:
                    errors.popitem(last=False)
                checks = self.__dict__.setdefault("refresh_checks", OrderedDict())
                checks[root] = self.clock()
                checks.move_to_end(root)
                while len(checks) > self.CACHE_ROOTS:
                    checks.popitem(last=False)
                self.refreshing.discard(root)
                waiters = tuple(self.refresh_waiters.pop(root, ()))
                if self.active_refresh_root == root:
                    self.active_refresh_root = None
                next_refresh = self._take_queued_refresh_locked()
            for waiter_id in waiters or (agent_id,):
                _publish_session_cost(self.state_root, waiter_id)
            if next_refresh is not None:
                self._launch_refresh(*next_refresh)
            return False
        return True

    def _take_queued_refresh_locked(self):
        if self.active_refresh_root is not None or not self.refresh_queue:
            return None
        root, _ = self.refresh_queue.popitem(last=False)
        waiters = self.refresh_waiters.get(root)
        if not waiters:
            self.refreshing.discard(root)
            return self._take_queued_refresh_locked()
        self.active_refresh_root = root
        return root, next(iter(waiters))

    def _background_refresh(self, agent_id, root):
        with self.lock:
            previous_cache = self.cache.get(root)
            previous_error = self.__dict__.setdefault("refresh_errors", OrderedDict()).get(root)
        try:
            self._compute_shared(agent_id, root, refresh=True)
            with self.lock:
                self.__dict__.setdefault("refresh_errors", OrderedDict()).pop(root, None)
        except Exception as error:
            with self.lock:
                errors = self.__dict__.setdefault("refresh_errors", OrderedDict())
                errors[root] = error
                errors.move_to_end(root)
                while len(errors) > self.CACHE_ROOTS:
                    errors.popitem(last=False)
        finally:
            with self.lock:
                checks = self.__dict__.setdefault("refresh_checks", OrderedDict())
                checks[root] = self.clock()
                checks.move_to_end(root)
                while len(checks) > self.CACHE_ROOTS:
                    checks.popitem(last=False)
                self.refreshing.discard(root)
                waiters = tuple(self.refresh_waiters.pop(root, ()))
                if self.active_refresh_root == root:
                    self.active_refresh_root = None
                current_cache = self.cache.get(root)
                current_error = self.__dict__.setdefault("refresh_errors", OrderedDict()).get(root)
                next_refresh = self._take_queued_refresh_locked()
            previous_error_key = (type(previous_error), str(previous_error)) if previous_error else None
            current_error_key = (type(current_error), str(current_error)) if current_error else None
            changed = (current_cache != previous_cache
                       or current_error_key != previous_error_key)
            if waiters or changed:
                publish_ids = waiters or ((agent_id,) if changed else ())
                for waiter_id in publish_ids:
                    _publish_session_cost(self.state_root, waiter_id)
            if next_refresh is not None:
                self._launch_refresh(*next_refresh)

    def snapshot(self, agent_id, *, wait=False):
        db = self._connect()
        try:
            db.execute("BEGIN")
            row = db.execute("SELECT record FROM analytics_agents WHERE id=?", (agent_id,)).fetchone()
            if not row:
                row = db.execute(f"SELECT record FROM {self._runtime_prefix}runtime_agents WHERE id=?", (agent_id,)).fetchone()
            if not row:
                raise ValueError("Unknown chat")
            root = json.loads(row["record"]).get("rootId") or agent_id
            usage_state = self._usage_state(db, root)
            agents = self._root_agents(db, agent_id, root)
        finally:
            db.rollback()  # End the explicit read snapshot before instrumented close.
            db.close()
        pricing_signature = self._catalog_signature(self.pricing.snapshot())
        agent_signature = self._agent_signature(agents)
        if not wait:
            return self._display_snapshot(agent_id, root, usage_state, pricing_signature,
                                          agent_signature)
        claude_agents = self._claude_agents(agents)
        claude_signature = self._claude_signature(claude_agents)
        with self.lock:
            cached = self.cache.get(root)
            if cached:
                self.cache.move_to_end(root)
        if not cached:
            cached = self._load_persisted(root, usage_state, pricing_signature,
                                          agent_signature, claude_signature)
            if cached:
                with self.lock:
                    self.cache[root] = cached
                    self.cache.move_to_end(root)
                    while len(self.cache) > self.CACHE_ROOTS:
                        self.cache.popitem(last=False)
        if cached:
            cached_at, result, source = cached
            age = max(0, self.clock() - cached_at)
            valid = (source.get("usage") == usage_state
                     and source.get("pricingSignature") == pricing_signature
                     and source.get("agentsSignature") == agent_signature
                     and source.get("claudeSignature") == claude_signature)
            changed = not valid
            started = self._start_refresh(agent_id, root, min_interval=10) if changed else False
            with self.lock:
                refreshing = root in self.refreshing
            return {**result, "cacheAgeSeconds": round(age, 1), "refreshing": refreshing}
        result = self._compute_shared(agent_id, root)
        return {**result, "cacheAgeSeconds": 0, "refreshing": False}

    def _display_snapshot(self, agent_id, root, usage_state, pricing_signature, agent_signature):
        # Claude log discovery and parsing run in the worker, outside HTTP requests.
        with self.lock:
            cached = self.cache.get(root)
            if cached:
                self.cache.move_to_end(root)
            error = self.__dict__.setdefault("refresh_errors", OrderedDict()).get(root)
            checked = self.__dict__.setdefault("refresh_checks", OrderedDict()).get(root)
        if cached:
            cached_at, result, source = cached
            changed = (source.get("usage") != usage_state
                       or source.get("pricingSignature") != pricing_signature
                       or source.get("agentsSignature") != agent_signature)
            due = checked is None or self.clock() - checked >= 10
            if changed or due:
                self._start_refresh(agent_id, root, min_interval=10)
            with self.lock:
                refreshing = root in self.refreshing
            return {**result, "cacheAgeSeconds": round(max(0, self.clock() - cached_at), 1),
                    "refreshing": refreshing}
        self._start_refresh(agent_id, root, min_interval=10)
        if error is not None:
            raise error
        return {"rootId": root, "totalUSD": None, "pricedSamples": 0,
                "breakdown": {"providers": {}, "models": {}}, "unknownModels": [],
                "estimated": True, "pricingState": "loading", "cacheAgeSeconds": 0,
                "refreshing": True, "method": "Calculating the session estimate."}

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
            source = result.pop("_cacheSource", {})
            if result.get("pricingState") == "ready":
                self._remember(root, result, source)
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
        key = str(path)
        signature = (stat.st_dev, stat.st_ino, stat.st_size, stat.st_mtime_ns, stat.st_ctime_ns)
        with self.lock:
            cached = self.file_cache.get(key)
            if cached and cached[0] == signature:
                self.file_cache.move_to_end(key)
                return cached[1]
        try:
            from codex_claude_costs import read_claude_usage_tail
            parsed = read_claude_usage_tail(path, cached[2] if cached and len(cached) > 2 else None)
            rows = parsed["rows"]
        except OSError:
            return []
        with self.lock:
            self.file_cache[key] = (signature, rows, parsed)
            self.file_cache.move_to_end(key)
            while len(self.file_cache) > 4096:
                self.file_cache.popitem(last=False)
        return rows

    def _cost_usage_groups(self, db, root):
        # Store only the selected token object and response value. The outer
        # projection reuses them without parsing the full record again.
        extract = "jsonb_extract" if sqlite3.sqlite_version_info >= (3, 45, 0) else "json_extract"
        db.execute(f"""
          CREATE TEMP TABLE session_cost_usage AS
            WITH records AS MATERIALIZED (
              SELECT seq,agent,thread,turn,at,
                     json_extract(record,'$.agentId') AS record_agent,
                     json_extract(record,'$.threadId') AS record_thread,
                     json_extract(record,'$.turnId') AS record_turn,
                     json_type(record,'$.responseId') AS response_type,
                     {extract}(record,'$.responseId') AS response,
                     CASE WHEN json_type(record,'$.model')='text' THEN json_extract(record,'$.model') END AS model,
                     COALESCE(json_extract(record,'$.accountKey'),'default') AS account_key,
                     CASE WHEN json_type(record,'$.delta.inputTokens') IN ('integer','real','true','false')
                            AND json_type(record,'$.delta.outputTokens') IN ('integer','real','true','false')
                          THEN {extract}(record,'$.delta')
                          WHEN json_type(record,'$.last')='object' THEN {extract}(record,'$.last') END AS tokens,
                     CASE WHEN json_extract(record,'$.inputTokensAreUncached')=1 THEN 1 ELSE 0 END AS input_uncached
                FROM analytics_usage
               WHERE root=? AND NOT EXISTS
                     (SELECT 1 FROM session_cost_claude_messages l
                       WHERE l.account_key IS json_extract(analytics_usage.record,'$.accountKey')
                         AND l.thread_id IS analytics_usage.thread
                         AND l.response_id IS json_extract(analytics_usage.record,'$.responseId'))
            )
            SELECT seq,agent,thread,turn,at,record_agent,record_thread,record_turn,
                   CASE WHEN response_type IS NULL OR response_type IN ('null','false')
                       OR (response_type IN ('integer','real') AND response=0)
                       OR (response_type='text' AND response='')
                       OR (response_type='array' AND json_array_length(response)=0)
                       OR (response_type='object' AND NOT EXISTS
                           (SELECT 1 FROM json_each(records.response)))
                      THEN 0 ELSE 1 END AS has_response,
                   model,account_key,
                   CASE WHEN json_type(tokens,'$.inputTokens') IN ('integer','real') THEN json_extract(tokens,'$.inputTokens') END AS input_tokens,
                   CASE WHEN json_type(tokens,'$.cachedInputTokens') IN ('integer','real') THEN json_extract(tokens,'$.cachedInputTokens') END AS cached_tokens,
                   CASE WHEN json_type(tokens,'$.cacheWriteInputTokens') IN ('integer','real') THEN json_extract(tokens,'$.cacheWriteInputTokens') END AS write_tokens,
                   CASE WHEN json_type(tokens,'$.outputTokens') IN ('integer','real') THEN json_extract(tokens,'$.outputTokens') END AS output_tokens,
                   input_uncached
              FROM records
        """, (root,))
        status = db.execute("SELECT MAX(model IS NULL),MAX(NOT has_response) FROM session_cost_usage").fetchone()
        missing_models, missing_responses = (bool(value) for value in status)
        if missing_responses:
            db.execute("""
              CREATE TEMP TABLE session_cost_responded_turns AS
                SELECT agent,thread,turn FROM session_cost_usage WHERE has_response
                 GROUP BY agent,thread,turn
            """)
            db.execute("CREATE INDEX session_cost_responded_turns_key ON session_cost_responded_turns(agent,thread,turn)")
        if missing_models:
            dedupe = """AND (has_response OR NOT EXISTS
                (SELECT 1 FROM session_cost_responded_turns rt WHERE rt.agent=session_cost_usage.agent
                  AND rt.thread IS session_cost_usage.thread AND rt.turn IS session_cost_usage.turn))""" if missing_responses else ""
            db.execute(f"""
              CREATE TEMP TABLE session_cost_model_source AS
                SELECT seq,at,record_agent,record_turn,record_thread,model
                  FROM session_cost_usage WHERE model IS NOT NULL {dedupe}
            """)
            db.execute("""
              CREATE TEMP TABLE session_cost_turn_models AS
                SELECT record_agent,record_turn,model FROM (
                  SELECT record_agent,record_turn,model,
                         ROW_NUMBER() OVER (PARTITION BY record_agent,record_turn ORDER BY at DESC,seq DESC) AS rank
                    FROM session_cost_model_source)
                 WHERE rank=1
            """)
            db.execute("CREATE INDEX session_cost_turn_models_key ON session_cost_turn_models(record_agent,record_turn)")
            db.execute("""
              CREATE TEMP TABLE session_cost_thread_models AS
                SELECT record_agent,record_thread,model FROM (
                  SELECT record_agent,record_thread,model,
                         ROW_NUMBER() OVER (PARTITION BY record_agent,record_thread ORDER BY at DESC,seq DESC) AS rank
                    FROM session_cost_model_source)
                 WHERE rank=1
            """)
            db.execute("CREATE INDEX session_cost_thread_models_key ON session_cost_thread_models(record_agent,record_thread)")
        model_joins = ("LEFT JOIN session_cost_turn_models tm ON tm.record_agent IS u.record_agent AND tm.record_turn IS u.record_turn "
                       "LEFT JOIN session_cost_thread_models hm ON hm.record_agent IS u.record_agent AND hm.record_thread IS u.record_thread") if missing_models else ""
        model = "COALESCE(u.model,NULLIF(tm.model,''),hm.model)" if missing_models else "u.model"
        response_join = ("LEFT JOIN session_cost_responded_turns rt ON rt.agent=u.agent AND rt.thread IS u.thread AND rt.turn IS u.turn"
                         if missing_responses else "")
        response_filter = "(u.has_response OR rt.agent IS NULL)" if missing_responses else "1"
        # The final query uses only the completed temporary tables. Release the
        # history snapshot before opening its cursor and calculating prices.
        db.commit()
        return db.execute(f"""
          WITH priced AS MATERIALIZED (
            SELECT {model} AS model,u.account_key,u.input_tokens,u.cached_tokens,u.write_tokens,u.output_tokens,u.input_uncached
              FROM session_cost_usage u {model_joins} {response_join}
             WHERE {response_filter}
          )
          SELECT model,account_key,input_tokens,cached_tokens,write_tokens,output_tokens,input_uncached,COUNT(*)
            FROM priced GROUP BY model,account_key,input_tokens,cached_tokens,write_tokens,output_tokens,input_uncached
        """)

    def _compute(self, agent_id, root):
        db = self._connect()
        try:
            catalog = self.pricing.snapshot()
            if catalog is None:
                return {"rootId": root, "totalUSD": None, "pricedSamples": 0,
                        "breakdown": {"providers": {}, "models": {}}, "unknownModels": [],
                        "estimated": True, "pricingState": "loading", "cacheAgeSeconds": 0,
                        "method": "Loading public API prices."}
            catalog_signature = self._catalog_signature(catalog)
            db.execute("BEGIN")
            usage_state = self._usage_state(db, root)
            generation = self._global_usage_generation(db)
            agents = self._root_agents(db, agent_id, root)
            claude_agents = self._claude_agents(agents)
            provider_by_account = {}
            for _, member in agents:
                account_key = member.get("accountKey", "default")
                provider = member.get("provider") or self._account(account_key).get("provider")
                if provider:
                    provider_by_account[account_key] = provider
                for history in (member.get("accountHistory") or []):
                    if isinstance(history, dict) and isinstance(history.get("accountKey"), str) and history.get("provider"):
                        provider_by_account[history["accountKey"]] = history["provider"]
            claude_history_present = any(
                member.get("provider") == "claude" or any(
                    isinstance(history, dict) and history.get("provider") == "claude"
                    for history in (member.get("accountHistory") or []))
                for _, member in agents)
            cache_source = {"usage": usage_state, "pricingSignature": catalog_signature,
                            "agentsSignature": self._agent_signature(agents),
                            "claudeSignature": self._claude_signature(claude_agents)}
            with self.lock:
                cached = self.cache.get(root)
            if not cached:
                cached = self._load_persisted(root, usage_state, catalog_signature,
                                              cache_source["agentsSignature"], cache_source["claudeSignature"])
            if cached and "claudeUsageSignature" in cached[2]:
                cache_source["claudeUsageSignature"] = cached[2]["claudeUsageSignature"]
            if cached and cached[2] == cache_source:
                return {**cached[1], "_cacheSource": cache_source}
            cost_total, model_totals, unpriced, provider_totals = 0.0, {}, set(), {}
            priced_count = 0
            tier_used = False
            claude_messages = {}
            claude_usage_digest = hashlib.sha256()
            for _, (current_key, current_profile, sessions) in claude_agents.items():
                for account_key, thread_id in sessions:
                    config = self._claude_profile(account_key)
                    if not config:
                        continue
                    for path in self._thread_ids(config, account_key, thread_id):
                        for message in self._log_rows(path):
                            key = (account_key, thread_id, message["id"])
                            claude_messages.setdefault(key, message)
                            claude_usage_digest.update(json.dumps([key, message], sort_keys=True,
                                separators=(",", ":"), ensure_ascii=False).encode("utf-8"))
                            claude_usage_digest.update(b"\n")
            cache_source["claudeUsageSignature"] = claude_usage_digest.hexdigest()
            # Log metadata can change without changing usage or receipt exclusions.
            if (cached and "claudeUsageSignature" in cached[2]
                    and {**cached[2], "claudeSignature": cache_source["claudeSignature"]} == cache_source):
                return {**cached[1], "_cacheSource": cache_source}
            db.execute("CREATE TEMP TABLE session_cost_claude_messages (account_key TEXT, thread_id TEXT, response_id TEXT, PRIMARY KEY(account_key,thread_id,response_id))")
            db.executemany("INSERT OR IGNORE INTO session_cost_claude_messages VALUES (?,?,?)",
                           claude_messages.keys())
            try:
                groups = self._cost_usage_groups(db, root)
            except sqlite3.OperationalError as error:
                if "no such table: analytics_usage" not in str(error):
                    raise
                db.commit()
                groups = ()
            for model, account_key, input_tokens, cached_tokens, write_tokens, output_tokens, input_uncached, count in groups:
                if model == "<synthetic>":
                    # Claude Code records local notices under this name with zero usage.
                    continue
                provider = provider_for(model) or provider_by_account.get(account_key)
                if provider is None:
                    unpriced.add(str(model or "Unknown model"))
                    continue
                usage = {"inputTokens": input_tokens, "cachedInputTokens": cached_tokens,
                         "cacheWriteInputTokens": write_tokens, "outputTokens": output_tokens}
                context_tokens = usage.get("inputTokens")
                if input_uncached and context_tokens is not None:
                    context_tokens += (usage.get("cachedInputTokens") or 0) + (usage.get("cacheWriteInputTokens") or 0)
                cost, status, tier = price_usage(catalog, provider, model, usage,
                                                 context_tokens=context_tokens,
                                                 input_tokens_are_uncached=bool(input_uncached))
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
            for record in claude_messages.values():
                model = record["model"]
                if model == "<synthetic>":
                    continue
                context_tokens = record["usage"].get("inputTokens", 0)
                if record.get("inputTokensAreUncached"):
                    context_tokens += record["usage"].get("cachedInputTokens", 0) + record["usage"].get("cacheWriteInputTokens", 0)
                cost, _, tier = price_usage(catalog, "anthropic", model, record["usage"],
                                            context_tokens=context_tokens,
                                            input_tokens_are_uncached=record.get("inputTokensAreUncached", False))
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
                      "claudeHistoryIncomplete": claude_history_present,
                      "cacheAgeSeconds": 0,
                      "method": "API prices from models.dev; cached input rates are applied when reported." +
                                (" Earlier Claude totals can be incomplete because earlier responses may not have separate saved usage rows." if claude_history_present else "") +
                                (" Published context tier applied where request size matched its threshold." if tier_used else " Base rates used when request size was unavailable or below the published threshold.")}
            result["_cacheSource"] = cache_source
            return result
        finally:
            db.rollback()
            db.close()
