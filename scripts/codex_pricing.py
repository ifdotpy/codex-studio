"""Cached public models.dev pricing for local usage estimates."""
import json
import copy
import math
import os
from pathlib import Path
import re
import tempfile
import threading
import time
import urllib.request
from urllib.error import HTTPError
from datetime import datetime, timezone

URL = "https://models.dev/api.json"
TTL = 24 * 60 * 60


def amount(value):
    return value if isinstance(value, (int, float)) and not isinstance(value, bool) and math.isfinite(value) and value >= 0 else None


def valid_catalog(value):
    if not isinstance(value, dict):
        return False
    providers = value.get("providers")
    if not isinstance(providers, dict) or any(not isinstance(providers.get(key), dict) for key in ("openai", "anthropic")):
        return False
    return all(
        isinstance(providers.get(provider, {}).get("models"), dict)
        and any(isinstance(model, dict) and isinstance(model.get("cost"), dict)
                and amount(model["cost"].get("input")) is not None
                and amount(model["cost"].get("output")) is not None
                for model in providers[provider]["models"].values())
        for provider in ("openai", "anthropic")
    )


class PricingCatalog:
    def __init__(self, state_dir, *, fetch=None, clock=time.time):
        self.path = Path(state_dir) / "model-pricing" / "models-dev-v1.json"
        self.attempt_path = self.path.with_name("refresh-state.json")
        self.fetch = fetch or self._fetch
        self.clock = clock
        self.lock = threading.RLock()
        self.refreshing = False
        self.artifact = self._read()
        self.last_attempt = self.artifact.get("lastAttemptAt", self.artifact.get("fetchedAtEpoch", 0)) if self.artifact else 0
        try:
            self.last_attempt = max(self.last_attempt, float(json.loads(self.attempt_path.read_text()).get("lastAttemptAt", 0)))
        except (OSError, ValueError, TypeError):
            pass

    @staticmethod
    def _fetch():
        request = urllib.request.Request(URL, headers={"Accept": "application/json", "User-Agent": "Codex-Studio-pricing-cache/1"}, method="GET")
        try:
            with urllib.request.urlopen(request, timeout=8) as response:
                if response.status != 200:
                    raise ValueError("Pricing catalog request failed")
                return json.load(response)
        except HTTPError as error:
            error.close()
            raise

    def _read(self):
        try:
            value = json.loads(self.path.read_text())
            if isinstance(value, dict) and value.get("version") == 1 and valid_catalog(value.get("catalog")):
                if not amount(value.get("fetchedAtEpoch")):
                    fetched = datetime.fromisoformat(value["fetchedAt"].replace("Z", "+00:00")).timestamp()
                    value["fetchedAtEpoch"] = fetched
                return value
        except (OSError, ValueError, TypeError):
            pass
        return None

    def snapshot(self):
        with self.lock:
            artifact = self.artifact
            stale = self.clock() - self.last_attempt >= TTL
            if stale and not self.refreshing:
                self.refreshing = True
                self.last_attempt = self.clock()
                self.attempt_path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
                attempt_tmp = self.attempt_path.with_name("." + self.attempt_path.name + ".tmp")
                attempt_tmp.write_text(json.dumps({"lastAttemptAt": self.last_attempt}))
                os.chmod(attempt_tmp, 0o600)
                os.replace(attempt_tmp, self.attempt_path)
                if artifact:
                    artifact["lastAttemptAt"] = self.last_attempt
                    self._save(artifact)
                threading.Thread(target=self._refresh, name="pricing-catalog", daemon=True).start()
            return artifact["catalog"] if artifact else None

    def wait_ready(self, timeout=8):
        catalog = self.snapshot()
        deadline = time.monotonic() + timeout
        while catalog is None and time.monotonic() < deadline:
            with self.lock:
                if not self.refreshing:
                    return None
            time.sleep(0.05)
            catalog = self.snapshot()
        return catalog

    def _refresh(self):
        try:
            raw = self.fetch()
            catalog = raw.get("providers", raw) if isinstance(raw, dict) else None
            if not valid_catalog({"providers": catalog}):
                raise ValueError("Pricing catalog is incomplete")
            fetched = self.clock()
            artifact = {"version": 1, "fetchedAt": datetime.fromtimestamp(fetched, timezone.utc).isoformat(),
                        "fetchedAtEpoch": fetched, "catalog": {"providers": catalog}}
            with self.lock:
                artifact["lastAttemptAt"] = fetched
                self._save(artifact)
                self.artifact = artifact
        except Exception:
            # Keep the last valid file after network or catalog errors.
            pass
        finally:
            with self.lock:
                self.refreshing = False

    def _save(self, artifact):
        self.path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
        fd, temporary = tempfile.mkstemp(prefix=".models-dev-", dir=self.path.parent)
        try:
            with os.fdopen(fd, "w") as stream:
                json.dump(artifact, stream, separators=(",", ":"))
                stream.flush()
                os.fsync(stream.fileno())
            os.chmod(temporary, 0o600)
            os.replace(temporary, self.path)
        finally:
            if os.path.exists(temporary):
                os.unlink(temporary)

    def write_scanner_copy(self, cache_root):
        with self.lock:
            artifact = self.artifact
        if not artifact:
            return
        path = Path(cache_root) / "model-pricing" / "models-dev-v1.json"
        path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
        temporary = path.with_name("." + path.name + ".tmp")
        scanner = copy.deepcopy(artifact)
        for provider in scanner["catalog"]["providers"].values():
            for model in provider.get("models", {}).values():
                cost = model.get("cost") or {}
                tiers = cost.get("tiers") or []
                context_tiers = [tier for tier in tiers
                                 if isinstance(tier, dict)
                                 and isinstance(tier.get("tier"), dict)
                                 and tier["tier"].get("type") == "context"]
                if tiers:
                    if len(context_tiers) == 1 and amount(context_tiers[0]["tier"].get("size")) == 200_000:
                        cost["context_over_200k"] = {key: context_tiers[0].get(key)
                                                      for key in ("input", "output", "cache_read", "cache_write")
                                                      if amount(context_tiers[0].get(key)) is not None}
                    else:
                        # The vendored parser only represents a 200k threshold.
                        cost.pop("context_over_200k", None)
                    model["cost"] = cost
        temporary.write_text(json.dumps(scanner, separators=(",", ":")))
        os.chmod(temporary, 0o600)
        os.replace(temporary, path)


def lookup(catalog, provider, model):
    if not isinstance(catalog, dict) or not isinstance(model, str):
        return None
    providers = catalog.get("providers", {})
    models = providers.get(provider, {}).get("models", {})
    record = models.get(model)
    if record is None:
        dated = re.fullmatch(r"(.+)-\d{8}", model)
        if dated:
            record = models.get(dated.group(1))
    if not isinstance(record, dict):
        return None
    cost = record.get("cost")
    if not isinstance(cost, dict):
        return None
    input_rate, output_rate = amount(cost.get("input")), amount(cost.get("output"))
    if input_rate is None or output_rate is None:
        return None
    limit = record.get("limit") or {}
    tiers = cost.get("tiers")
    threshold = None
    over = {}
    parsed = []
    if isinstance(tiers, list):
        parsed = []
        for tier in tiers:
            condition = tier.get("tier") if isinstance(tier, dict) else None
            size = amount(condition.get("size")) if isinstance(condition, dict) and condition.get("type") == "context" else None
            if size is not None:
                parsed.append((size, tier))
        if parsed:
            parsed.sort(key=lambda value: value[0])
            threshold = parsed[0][0]
    legacy = cost.get("context_over_200k") or {}
    if threshold is None and legacy:
        threshold, over = 200_000, legacy
    return {"input": input_rate, "output": output_rate,
            "cache_read": amount(cost.get("cache_read")), "cache_write": amount(cost.get("cache_write")),
            "context": limit.get("context") if isinstance(limit, dict) else None,
            "threshold": threshold,
            "over": {key: amount(over.get(key)) for key in ("input", "output", "cache_read", "cache_write")},
            "tiers": parsed,
            "inclusive": bool(parsed),
            "tiered": bool(tiers or legacy)}


def price_usage(catalog, provider, model, usage, *, context_tokens=None):
    rates = lookup(catalog, provider, model)
    if not rates:
        return None, "unpriced", False
    usage = usage if isinstance(usage, dict) else {}
    input_tokens = amount(usage.get("inputTokens"))
    cached = amount(usage.get("cachedInputTokens"))
    cache_write = amount(usage.get("cacheWriteInputTokens"))
    output = amount(usage.get("outputTokens"))
    if input_tokens is None or output is None:
        return None, "incomplete", False
    if rates["cache_read"] is not None and cached is None:
        return None, "incomplete", False
    if rates["cache_write"] is not None and cache_write is None:
        return None, "incomplete", False
    cached = min(input_tokens, cached or 0)
    cache_write = min(max(0, input_tokens - cached), cache_write or 0)
    base = max(0, input_tokens - cached - cache_write)
    tier_size = amount(context_tokens)
    use_tier = rates["threshold"] is not None and tier_size is not None and (
        tier_size >= rates["threshold"] if rates["inclusive"] else tier_size > rates["threshold"])
    if rates["tiers"] and use_tier:
        eligible = [tier for size, tier in rates["tiers"] if tier_size >= size]
        over = eligible[-1] if eligible else {}
    else:
        over = rates["over"]
    if use_tier:
        base_rate = over["input"] if over["input"] is not None else rates["input"]
        out_rate = over["output"] if over["output"] is not None else rates["output"]
        read_rate = amount(over.get("cache_read")) if over.get("cache_read") is not None else rates["cache_read"]
        write_rate = amount(over.get("cache_write")) if over.get("cache_write") is not None else rates["cache_write"]
    else:
        base_rate, out_rate = rates["input"], rates["output"]
        read_rate, write_rate = rates["cache_read"], rates["cache_write"]
    if cached and read_rate is None or cache_write and write_rate is None:
        return None, "incomplete", use_tier
    total = (base * base_rate + output * out_rate + cached * (read_rate or 0) + cache_write * (write_rate or 0)) / 1_000_000
    return total, "priced", use_tier
