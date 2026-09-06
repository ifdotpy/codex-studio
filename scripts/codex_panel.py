"""Persistent, agent-owned HTML/CSS panels above the message composer."""

import json
import re
import time
from codex_work import text_field


class PanelConflict(ValueError):
    """The displayed panel or one of its actions has changed."""


NAME = re.compile(r"[A-Za-z][A-Za-z0-9_-]{0,63}\Z")


def panel_tools(tool, text):
    return [tool(
        "orchestration_panel",
        "Display HTML/CSS/SVG in your persistent 150px-high panel between the chat and composer. "
        "Design a compact visual instrument, not another chat message: use CSS grids, segmented progress bars, "
        "stage tracks, small SVG diagrams, or measured counters with short labels. Avoid paragraphs and repeated chat summaries. "
        "Studio supplies colors and basic controls, not a layout. Build the layout with html and css. "
        "For progress, draw connected stage segments and separate measured counters; text with arrows alone is insufficient. "
        "Show the current phase and one bottleneck at a glance. Use real counts only; never invent percentages or fake progress. "
        "set replaces the whole panel; "
        "get reads it; clear empties it. Supply html (fragment or document) and optional css. "
        "Updates appear in place without a chat message. Each agent owns its own panel. "
        "Codex Studio styles semantic HTML by default; override with css only when useful. "
        "For buttons/forms, declare callbacks [{id,label,fields:[fieldNames]}] and use data-callback=id on the button or form. "
        "Named form fields are sent to you as arrays of strings after a real user click/submit. "
        "Each action is accepted once per panel version and wakes you after your final answer; publish a new panel to enable it again. "
        "set and get return a rendered PNG of that panel revision at 1000x150 CSS pixels. Inspect it and fix clipped or unreadable content. "
        "Also check whether shape, position or size communicates the state; if it is only rows of text, revise the composition. "
        "If rendering fails, the saved document remains; use a new get call to retry the image. "
        "Use button type=button for standalone actions and form data-callback plus button type=submit for forms. "
        "Your own scripts, navigation, and external resources are disabled; the trusted host handles callbacks. "
        "Keep content responsive and readable within 150px. Update on meaningful changes, not by polling.",
        {
            "action": {"type": "string", "enum": ["get", "set", "clear"]},
            "html": {**text, "maxLength": 131072},
            "css": {**text, "maxLength": 32768},
            "callbacks": {"type": "array", "maxItems": 16, "items": {
                "type": "object", "properties": {
                    "id": {"type": "string", "pattern": "^[A-Za-z][A-Za-z0-9_-]{0,63}$"},
                    "label": {"type": "string", "maxLength": 100},
                    "fields": {"type": "array", "maxItems": 32, "items": {"type": "string"}},
                }, "required": ["id", "label"], "additionalProperties": False,
            }},
        },
        ["action"],
    )]


class PanelMixin:
    def setup_panels(self, db):
        db.execute("CREATE TABLE IF NOT EXISTS runtime_panels (id TEXT PRIMARY KEY, record TEXT NOT NULL)")
        db.execute("""CREATE TABLE IF NOT EXISTS runtime_panel_callbacks (
            agent TEXT NOT NULL, version INTEGER NOT NULL, callback TEXT NOT NULL,
            payload TEXT NOT NULL, result TEXT NOT NULL,
            PRIMARY KEY(agent,version,callback))""")

    def panel(self, agent_id, db=None):
        if db is None:
            with self.lock, self.db() as connection:
                return self.panel(agent_id, connection)
        self.checked_actor(db, agent_id)
        row = db.execute("SELECT record FROM runtime_panels WHERE id=?", (agent_id,)).fetchone()
        panel = json.loads(row[0]) if row else {
            "id": agent_id, "agent": agent_id, "version": 0,
            "html": "", "css": "", "updated": None,
        }
        panel.setdefault("callbacks", [])
        panel["submittedCallbacks"] = [r[0] for r in db.execute(
            "SELECT callback FROM runtime_panel_callbacks WHERE agent=? AND version=?", (agent_id, panel["version"])
        )]
        return panel

    @staticmethod
    def validate_callbacks(value):
        if not isinstance(value, list) or len(value) > 16:
            raise ValueError("Supply at most 16 callbacks")
        callbacks, seen = [], set()
        for entry in value:
            if not isinstance(entry, dict) or set(entry) - {"id", "label", "fields"}:
                raise ValueError("Supply callback id, label, and optional fields")
            name = entry.get("id")
            if not isinstance(name, str) or not NAME.fullmatch(name) or name in seen:
                raise ValueError("Callback ids must be unique names of 1 to 64 characters")
            fields = entry.get("fields", [])
            if (not isinstance(fields, list) or len(fields) > 32
                or any(not isinstance(v, str) or not NAME.fullmatch(v) for v in fields)
                or len(set(fields)) != len(fields)):
                raise ValueError("Supply up to 32 unique field names")
            callbacks.append({"id": name, "label": text_field(entry.get("label"), "a callback label", 100), "fields": fields})
            seen.add(name)
        return callbacks

    def panel_action(self, actor_id, data, key=None, epoch=None):
        with self.lock, self.db() as db:
            actor = self.checked_actor(db, actor_id, actor_id)
            if epoch is not None and actor["epoch"] != epoch:
                raise ValueError("The caller was stopped")
            if not isinstance(data, dict) or set(data) - {"action", "html", "css", "callbacks"}:
                raise ValueError("Supply action, html, css, and callbacks only; the panel belongs to the calling agent")
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
                callbacks = self.validate_callbacks(data.get("callbacks", []))
                html, css = data.get("html"), data.get("css", "")
                for name, value, maximum in [("html", html, 131072), ("css", css, 32768)]:
                    if not isinstance(value, str) or len(value.encode("utf-8")) > maximum:
                        raise ValueError(f"{name} must be text with at most {maximum} UTF-8 bytes")
            else:
                if set(data) != {"action"}:
                    raise ValueError("clear accepts no content")
                html, css = "", ""
                callbacks = []
            panel.pop("submittedCallbacks", None)
            panel.update(html=html, css=css, callbacks=callbacks, version=panel["version"] + 1, updated=time.time())
            self.put(db, "panels", panel)
            actor["panelVersion"] = panel["version"]
            self.put(db, "agents", actor)
            result = {k: panel[k] for k in ("agent", "version", "updated")}
            self.save_receipt(db, key, signature, result)
            self.touch_ui(actor_id)
            return result

    def panel_callback(self, data):
        if not isinstance(data, dict) or set(data) != {"id", "agent", "version", "callback", "values"}:
            raise ValueError("Supply id, agent, version, callback, and values")
        request = text_field(data.get("id"), "a request id", 200)
        with self.lock, self.db() as db:
            owner = self.checked_actor(db, data.get("agent"))
            signature, prior = self.operation_receipt(db, request, {"panelCallback": data})
            if prior is not None:
                return prior
            panel = self.panel(owner["id"], db)
            if type(data["version"]) is not int or panel["version"] != data["version"]:
                raise PanelConflict("This panel changed. Use its current controls")
            if not owner["autoWake"]:
                raise ValueError("The agent is stopped. Resume it before using panel actions")
            callback = next((v for v in panel["callbacks"] if v["id"] == data["callback"]), None)
            if callback is None:
                raise ValueError("This callback is not declared in the current panel")
            values = data["values"]
            if (not isinstance(values, dict) or set(values) - set(callback["fields"])
                or any(not isinstance(v, list) or len(v) > 16
                       or any(not isinstance(item, str) or len(item) > 2000 for item in v) for v in values.values())
                or len(json.dumps(values, ensure_ascii=False).encode("utf-8")) > 16384):
                raise ValueError("Supply only declared fields as bounded arrays of strings (16 KiB total)")
            payload = json.dumps(values, sort_keys=True, ensure_ascii=False)
            existing = db.execute("SELECT payload,result FROM runtime_panel_callbacks WHERE agent=? AND version=? AND callback=?",
                                  (owner["id"], panel["version"], callback["id"])).fetchone()
            if existing:
                if existing["payload"] != payload:
                    raise PanelConflict("This action was already submitted. Wait for the agent to update the panel")
                result = {**json.loads(existing["result"]), "id": request}
                return self.save_receipt(db, request, signature, result)
            event = self.enqueue(db, owner, "panel_callback", json.dumps({
                "source": "user_panel_interaction", "panelVersion": panel["version"],
                "callback": callback["id"], "label": callback["label"], "values": values,
                "at": time.time(),
                "instruction": "The user activated your panel control. Respond to this action and its form values. Publish an updated panel when useful; do not infer permission for unrelated actions.",
            }, ensure_ascii=False), "panel-callback:" + request)
            result = {"id": request, "agent": owner["id"], "version": panel["version"],
                      "callback": callback["id"], "eventId": event, "status": "pending"}
            db.execute("INSERT INTO runtime_panel_callbacks VALUES (?,?,?,?,?)",
                       (owner["id"], panel["version"], callback["id"], payload, json.dumps(result)))
            self.touch_ui(owner["id"])
            return self.save_receipt(db, request, signature, result)
