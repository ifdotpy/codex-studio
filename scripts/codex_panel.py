"""Persistent, agent-owned structured and HTML panels above the composer."""

import copy
import json
import re
from pathlib import Path
import time
from codex_work import text_field


class PanelConflict(ValueError):
    """The displayed panel or one of its actions has changed."""


NAME = re.compile(r"[A-Za-z][A-Za-z0-9_-]{0,63}\Z")


def panel_tools(tool, text):
    return [tool(
        "orchestration_panel",
        "Display an interactive, persistent 150px-high panel between the chat and composer. "
        "Prefer structured json-render UI: first call action=catalog for the supported components, props, state, and actions; "
        "then action=set with spec={root,elements,state?}. Studio renders its own components and theme. "
        "Compose compact visual progress, steps, measured counters, forms, and useful controls. "
        "Show the current phase and one bottleneck; avoid paragraphs and repeated chat summaries. Never invent progress. "
        "For a visualization the catalog cannot express, set html and optional css instead of spec. "
        "Do not mix spec with html/css. The HTML canvas shares the chat background and has no outer margin; "
        "include all padding and borders inside its 150px height. "
        "set replaces the whole panel; get reads it; clear empties it. Each agent owns its own panel. "
        "Declare callbacks [{id,label,fields:[fieldNames]}] for user actions. For structured UI, follow the catalog action contract. "
        "For HTML, use data-callback=id on a button or form; use type=button for standalone buttons and type=submit inside forms. "
        "Declared form values arrive as arrays of strings after a real user click or submit. "
        "Each callback is accepted once per panel version and wakes you after your final answer; publish a new panel to enable it again. "
        "set and get return a rendered PNG of the exact revision at 1000x150 CSS pixels. Inspect it and revise unreadable content. "
        "set validates the components and layout at widths 320, 640, and 1000px before saving. "
        "Invalid content, overflow, clipping, or renderer failure rejects set and preserves the previous panel and callbacks. "
        "Use the error details to revise the content; get can still read an older panel. "
        "Scripts, navigation, and external resources are disabled; the trusted host handles callbacks. "
        "Keep the panel responsive within 150px. Update on meaningful changes, not by polling.",
        {
            "action": {"type": "string", "enum": ["catalog", "get", "set", "clear"]},
            "spec": {"type": "object", "properties": {
                "root": {"type": "string"}, "elements": {"type": "object", "additionalProperties": True},
                "state": {"type": "object", "additionalProperties": True},
            }, "required": ["root", "elements"], "additionalProperties": False},
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


CATALOG_FILE = Path(__file__).resolve().parent.parent / "web/dist/panel-catalog.json"


def validate_panel_spec(value):
    """Check transport shape and size; the renderer validates the catalog contract."""
    if (not isinstance(value, dict) or set(value) - {"root", "elements", "state"}
        or not isinstance(value.get("root"), str) or not value["root"]
        or not isinstance(value.get("elements"), dict) or not value["elements"]
        or ("state" in value and not isinstance(value["state"], dict))):
        raise ValueError("spec requires a nonempty root string, an elements object, and optional state object")
    try:
        encoded = json.dumps(value, ensure_ascii=False, allow_nan=False).encode("utf-8")
    except (TypeError, ValueError, UnicodeError) as error:
        raise ValueError("spec must contain valid finite JSON values") from error
    if len(encoded) > 131072:
        raise ValueError("spec must contain at most 131072 UTF-8 bytes")
    return value


def panel_catalog():
    try:
        catalog = json.loads(CATALOG_FILE.read_text(encoding="utf-8"))
    except (OSError, ValueError) as error:
        raise ValueError("The panel catalog is unavailable. Build or update Codex Studio") from error
    if not isinstance(catalog, dict) or not catalog:
        raise ValueError("The panel catalog is invalid. Build or update Codex Studio")
    return catalog


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
        panel.setdefault("format", "html")
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

    def panel_action(self, actor_id, data, key=None, epoch=None, capture=None):
        data = copy.deepcopy(data)
        with self.lock, self.db() as db:
            actor = self.checked_actor(db, actor_id, actor_id)
            if epoch is not None and actor["epoch"] != epoch:
                raise ValueError("The caller was stopped")
            if not isinstance(data, dict) or set(data) - {"action", "spec", "html", "css", "callbacks"}:
                raise ValueError("Supply action, spec or html/css, and callbacks only; the panel belongs to the calling agent")
            action = data.get("action")
            if action not in {"catalog", "get", "set", "clear"}:
                raise ValueError("Unknown panel action")
            if action == "catalog":
                if set(data) != {"action"}:
                    raise ValueError("catalog accepts no content")
                return panel_catalog()
            signature, prior = self.operation_receipt(db, key, {"actor": actor_id, "panel": data})
            if prior is not None:
                return prior
            panel = self.panel(actor_id, db)
            if action == "get":
                return panel
            if action == "set":
                callbacks = self.validate_callbacks(data.get("callbacks", []))
                spec = None
                if "spec" in data:
                    if "html" in data or "css" in data:
                        raise ValueError("Supply spec or html/css, not both")
                    spec = validate_panel_spec(data["spec"])
                    html, css = "", ""
                else:
                    html, css = data.get("html"), data.get("css", "")
                    for name, value, maximum in [("html", html, 131072), ("css", css, 32768)]:
                        if not isinstance(value, str) or len(value.encode("utf-8")) > maximum:
                            raise ValueError(f"{name} must be text with at most {maximum} UTF-8 bytes")
            else:
                if set(data) != {"action"}:
                    raise ValueError("clear accepts no content")
                html, css, spec = "", "", None
                callbacks = []
            base_version = panel["version"]
            identity = tuple(actor.get(k) for k in ("epoch", "threadId", "turnId", "accountKey"))
            panel.pop("submittedCallbacks", None)
            panel.update(html=html, css=css, callbacks=callbacks, version=base_version + 1,
                         format="json-render" if spec is not None else "html")
            panel.pop("spec", None)
            if spec is not None:
                panel["spec"] = spec
            if action == "clear":
                return self.commit_panel(db, actor, panel, key, signature)

        # Chromium can take seconds. Keep database and runtime locks free.
        rendered = self.capture_panel(panel, strict_layout=True)
        with self.lock, self.db() as db:
            actor = self.checked_actor(db, actor_id, actor_id)
            if tuple(actor.get(k) for k in ("epoch", "threadId", "turnId", "accountKey")) != identity:
                raise PanelConflict("The caller changed or was stopped. The panel was not saved")
            signature, prior = self.operation_receipt(db, key, {"actor": actor_id, "panel": data})
            if prior is not None:
                return prior
            if self.panel(actor_id, db)["version"] != base_version:
                raise PanelConflict("The panel changed during validation. Read the current panel before another update")
            result = self.commit_panel(db, actor, panel, key, signature)
        if capture is not None:
            capture.update(rendered)
        return result

    def commit_panel(self, db, actor, panel, key, signature):
        panel["updated"] = time.time()
        self.put(db, "panels", panel)
        actor["panelVersion"] = panel["version"]
        self.put(db, "agents", actor)
        result = {k: panel[k] for k in ("agent", "version", "updated")}
        self.save_receipt(db, key, signature, result)
        self.touch_ui(actor["id"])
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
