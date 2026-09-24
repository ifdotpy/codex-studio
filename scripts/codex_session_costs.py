"""Read-only API price estimates for one managed team."""
import json
import sqlite3
import time

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
    def __init__(self, db_path, pricing):
        self.db_path = db_path
        self.pricing = pricing
        self.cache = {}

    def snapshot(self, agent_id):
        db = sqlite3.connect(f"file:{self.db_path}?mode=ro", uri=True, timeout=3)
        db.row_factory = sqlite3.Row
        try:
            row = db.execute("SELECT record FROM analytics_agents WHERE id=?", (agent_id,)).fetchone()
            if not row:
                row = db.execute("SELECT record FROM runtime_agents WHERE id=?", (agent_id,)).fetchone()
            if not row:
                raise ValueError("Unknown chat")
            agent = json.loads(row["record"])
            root = agent.get("rootId") or agent_id
            try:
                row = db.execute("SELECT value FROM analytics_meta WHERE key='usageGeneration'").fetchone()
                generation = int(row[0]) if row else 0
            except sqlite3.OperationalError:
                generation = 0
            key = (root, generation)
            if key in self.cache:
                return self.cache[key]
            try:
                rows = db.execute("SELECT agent,thread,turn,record FROM analytics_usage WHERE root=? ORDER BY at,seq", (root,)).fetchall()
            except sqlite3.OperationalError:
                rows = []
            turns = {(r["agent"], r["thread"], r["turn"]) for r in rows
                     if (record := json.loads(r["record"])).get("responseId")}
            usage = []
            for row in rows:
                record = json.loads(row["record"])
                if not record.get("responseId") and (row["agent"], row["thread"], row["turn"]) in turns:
                    continue
                usage.append(record)
            catalog = self.pricing.snapshot()
            total, priced_models, unpriced, notes = 0.0, {}, set(), set()
            provider_totals = {}
            priced_count = 0
            for record in usage:
                model = record.get("model")
                provider = provider_for(model)
                if provider is None:
                    unpriced.add(str(model or "Unknown model"))
                    continue
                last = record.get("last") or {}
                usage_row = record.get("delta") or {}
                if not all(isinstance(usage_row.get(field), (int, float)) for field in ("inputTokens", "outputTokens")):
                    usage_row = last
                context_size = usage_row.get("inputTokens")
                cost, status, tier = price_usage(catalog, provider, model, usage_row, context_tokens=context_size)
                if cost is None:
                    unpriced.add(model)
                    continue
                priced_count += 1
                total += cost
                priced_models[model] = priced_models.get(model, 0.0) + cost
                provider_totals[provider] = provider_totals.get(provider, 0.0) + cost
                if tier:
                    notes.add("published context tier applied")
            result = {"rootId": root, "generation": generation, "totalUSD": total if priced_count else None,
                      "pricedSamples": priced_count, "breakdown": {"providers": provider_totals, "models": priced_models},
                      "unknownModels": sorted(unpriced), "estimated": True,
                      "method": "API prices from models.dev; cached input rates are applied when reported." +
                                (" Published context tier applied where request size matched its threshold." if notes else " Base rates used when request size was unavailable or below the published threshold.")}
            self.cache = {key: result}
            return result
        finally:
            db.close()
