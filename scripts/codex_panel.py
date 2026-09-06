"""Persistent, agent-owned HTML/CSS panels above the message composer."""

import json
import time


def panel_tools(tool, text):
    return [tool(
        "orchestration_panel",
        "Display HTML/CSS/SVG in your persistent 200px-high panel between the chat and composer. "
        "Use it for progress, diagrams, or a compact dashboard. set replaces the whole panel; "
        "get reads it; clear empties it. Supply html (fragment or document) and optional css. "
        "Updates appear in place without a chat message. Each agent owns its own panel. "
        "Scripts, navigation, and external resources are disabled; inline CSS and CSS animations work. "
        "Keep content responsive and readable within 200px. Update on meaningful changes, not by polling.",
        {
            "action": {"type": "string", "enum": ["get", "set", "clear"]},
            "html": {**text, "maxLength": 131072},
            "css": {**text, "maxLength": 32768},
        },
        ["action"],
    )]


class PanelMixin:
    def setup_panels(self, db):
        db.execute("CREATE TABLE IF NOT EXISTS runtime_panels (id TEXT PRIMARY KEY, record TEXT NOT NULL)")

    def panel(self, agent_id, db=None):
        if db is None:
            with self.lock, self.db() as connection:
                return self.panel(agent_id, connection)
        self.checked_actor(db, agent_id)
        row = db.execute("SELECT record FROM runtime_panels WHERE id=?", (agent_id,)).fetchone()
        return json.loads(row[0]) if row else {
            "id": agent_id, "agent": agent_id, "version": 0,
            "html": "", "css": "", "updated": None,
        }

    def panel_action(self, actor_id, data, key=None, epoch=None):
        with self.lock, self.db() as db:
            actor = self.checked_actor(db, actor_id, actor_id)
            if epoch is not None and actor["epoch"] != epoch:
                raise ValueError("The caller was stopped")
            if not isinstance(data, dict) or set(data) - {"action", "html", "css"}:
                raise ValueError("Supply action, html, and css only; the panel belongs to the calling agent")
            action = data.get("action")
            if action not in {"get", "set", "clear"}:
                raise ValueError("Unknown panel action")
            signature, prior = self.operation_receipt(db, key, {"actor": actor_id, "panel": data})
            if prior is not None:
                return prior
            panel = self.panel(actor_id, db)
            if action == "get":
                return panel
            if action == "set":
                html, css = data.get("html"), data.get("css", "")
                for name, value, maximum in [("html", html, 131072), ("css", css, 32768)]:
                    if not isinstance(value, str) or len(value.encode("utf-8")) > maximum:
                        raise ValueError(f"{name} must be text with at most {maximum} UTF-8 bytes")
            else:
                if set(data) != {"action"}:
                    raise ValueError("clear accepts no content")
                html, css = "", ""
            panel.update(html=html, css=css, version=panel["version"] + 1, updated=time.time())
            self.put(db, "panels", panel)
            actor["panelVersion"] = panel["version"]
            self.put(db, "agents", actor)
            result = {k: panel[k] for k in ("agent", "version", "updated")}
            self.save_receipt(db, key, signature, result)
            self.touch_ui(actor_id)
            return result
