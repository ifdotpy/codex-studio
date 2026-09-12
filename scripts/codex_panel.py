"""Plain progress display and retained legacy panel callbacks and feeds."""

import copy
import json
import re
from pathlib import Path
import time
import uuid
from codex_work import text_field


class PanelConflict(ValueError):
    """The displayed panel or one of its actions has changed."""


NAME = re.compile(r"[A-Za-z][A-Za-z0-9_-]{0,63}\Z")


def panel_tools(tool, text):
    """Legacy callbacks remain readable; new threads use PROGRESS.md."""
    return []


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
    def get_panel(self, agent_id):
        from codex_progress import read_progress
        with self.lock, self.db() as db:
            agent = self.checked_actor(db, agent_id)
            identity = agent["id"]
        # File access must not hold the shared runtime or database lock.
        return read_progress(self.root, identity)

    def setup_panels(self, db):
        db.execute("CREATE TABLE IF NOT EXISTS runtime_panels (id TEXT PRIMARY KEY, record TEXT NOT NULL)")
        db.execute("""CREATE TABLE IF NOT EXISTS runtime_panel_callbacks (
            agent TEXT NOT NULL, version INTEGER NOT NULL, callback TEXT NOT NULL,
            payload TEXT NOT NULL, result TEXT NOT NULL,
            PRIMARY KEY(agent,version,callback))""")
        # A restart must not silently replay an arbitrary collector command.
        for row in db.execute("SELECT record FROM runtime_panels").fetchall():
            panel = json.loads(row[0])
            if panel.get("feed", {}).get("status") in {"starting", "approval", "running", "stopping"}:
                panel["feed"].update(status="lost", error="The server restarted. Start a new feed to refresh this snapshot.")
                panel["dataVersion"] = panel.get("dataVersion", 0) + 1
                self.put(db, "panels", panel)
                actor = self.agent(panel["agent"], db)
                actor["panelDataVersion"] = panel["dataVersion"]
                self.put(db, "agents", actor)

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
            panel.pop("feed", None)
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
        old_feed = self.panel(actor["id"], db).get("feed")
        panel.pop("feed", None)
        panel["dataVersion"] = 0
        actor["panelDataVersion"] = 0
        panel["updated"] = time.time()
        self.put(db, "panels", panel)
        actor["panelVersion"] = panel["version"]
        self.put(db, "agents", actor)
        result = {k: panel[k] for k in ("agent", "version", "updated")}
        self.save_receipt(db, key, signature, result)
        self.touch_ui(actor["id"])
        if old_feed:
            # Cancellation runs outside this transaction. The changed template
            # already fences every frame from the previous command.
            self.pool.submit(self.cancel_panel_feed, actor["id"], old_feed)
        return result

    def cancel_panel_feed(self, actor_id, feed):
        with self.lock, self.db() as db:
            row = db.execute("SELECT record FROM runtime_monitors WHERE id=?", (feed["monitorId"],)).fetchone()
            if not row:
                # The lease can be revoked before command registration. Runtime
                # checks that lease before it creates or submits the command.
                return {"id": feed["monitorId"], "status": "cancelled"}
        return self.cancel_monitor(feed["monitorId"], owner=actor_id)

    @staticmethod
    def feed_slot(path):
        if (not isinstance(path, str) or not path.startswith("/")
                or not NAME.fullmatch(path[1:]) or path[1:] in {"constructor", "prototype"}):
            raise ValueError("statePath must name one top-level object, for example /live")
        return path[1:]

    def write_panel_data(self, db, actor, panel):
        panel["dataVersion"] = panel.get("dataVersion", 0) + 1
        self.put(db, "panels", panel)
        actor["panelDataVersion"] = panel["dataVersion"]
        # Snapshot replication carries this counter. Do not wake transcript
        # long-poll readers for data that never changed the conversation.
        db.execute("UPDATE runtime_agents SET record=? WHERE id=?", (json.dumps(actor), actor["id"]))

    def panel_feed_action(self, actor_id, data, key=None, epoch=None):
        if not isinstance(data, dict) or set(data) - {"action", "command", "statePath", "timeout_ms"}:
            raise ValueError("Supply action and optional command, statePath, timeout_ms")
        action = data.get("action")
        if action not in {"start", "get", "stop"}:
            raise ValueError("Choose start, get, or stop")
        if action != "start" and set(data) != {"action"}:
            raise ValueError("get and stop accept no command or settings")
        request = key or str(uuid.uuid4())
        monitor_id = str(uuid.uuid5(uuid.NAMESPACE_URL, request))
        with self.lock, self.db() as db:
            actor = self.checked_actor(db, actor_id, actor_id)
            if epoch is not None and actor["epoch"] != epoch:
                raise PanelConflict("The caller was stopped")
            signature, prior = self.operation_receipt(db, key, {"actor": actor_id, "panelFeed": data})
            if prior is not None:
                return prior
            panel = self.panel(actor_id, db)
            old_feed = panel.get("feed")
            if action == "get":
                return {"agent": actor_id, "version": panel["version"], "dataVersion": panel.get("dataVersion", 0), "feed": old_feed}
            if action == "stop":
                result = {"agent": actor_id, "monitorId": old_feed["monitorId"] if old_feed else None, "status": "stopping" if old_feed else "stopped"}
                if old_feed:
                    panel["feed"].update(status="stopping")
                    self.write_panel_data(db, actor, panel)
                self.save_receipt(db, key, signature, result)
            else:
                command = data.get("command")
                timeout = data.get("timeout_ms", 86400000)
                if not isinstance(command, str) or not 1 <= len(command.strip()) <= 12000:
                    raise ValueError("Supply a command with 1 to 12000 characters")
                if type(timeout) is not int or not 1000 <= timeout <= 86400000:
                    raise ValueError("Command timeout must be 1 second to 24 hours")
                if not actor["autoWake"]:
                    raise ValueError("Resume the agent before starting a feed")
                self.assert_workspace_available(db, actor)
                state_path = data.get("statePath", "/live")
                slot = self.feed_slot(state_path)
                if panel.get("format") != "json-render" or not isinstance(panel.get("spec", {}).get("state", {}).get(slot), dict):
                    raise ValueError("First set a structured panel with an initialized object at spec.state" + state_path)
                candidate = copy.deepcopy(panel)
                candidate["feed"] = {"monitorId": monitor_id, "statePath": state_path, "status": "starting",
                                     "epoch": actor["epoch"], "sequence": 0, "updated": None, "error": None}
                base_version, base_epoch = panel["version"], actor["epoch"]
        if action == "stop":
            if old_feed:
                stopped = self.cancel_panel_feed(actor_id, old_feed)
                if stopped["status"] in {"cancelled", "completed", "failed", "lost"}:
                    self.panel_feed_status(actor_id, old_feed["monitorId"], panel["version"],
                                           old_feed["statePath"], stopped["status"], old_feed.get("error"))
            return result
        # Validate the binding and all selectable layouts before command execution.
        self.capture_panel(candidate, strict_layout=True)
        with self.lock, self.db() as db:
            actor = self.checked_actor(db, actor_id, actor_id)
            panel = self.panel(actor_id, db)
            signature, prior = self.operation_receipt(db, key, {"actor": actor_id, "panelFeed": data})
            if prior is not None:
                return prior
            if (panel["version"] != base_version or actor["epoch"] != base_epoch or not actor["autoWake"]
                    or panel.get("feed", {}).get("monitorId") != (old_feed or {}).get("monitorId")):
                raise PanelConflict("The panel or its feed changed during validation")
            panel["feed"] = candidate["feed"]
            self.write_panel_data(db, actor, panel)
            result = {"agent": actor_id, "monitorId": monitor_id, "version": base_version,
                      "statePath": state_path, "status": "starting"}
            self.save_receipt(db, key, signature, result)
        try:
            if old_feed:
                self.cancel_panel_feed(actor_id, old_feed)
            self.monitor(actor_id, {"command": command, "timeout_ms": timeout}, key=request,
                                   approved=self.monitor_auto_approved(actor), epoch=base_epoch,
                                   panel_feed={"panelVersion": base_version, "statePath": state_path, "intervalMs": 1000})
            return result
        except Exception as error:
            self.panel_feed_status(actor_id, monitor_id, base_version, state_path, "failed", str(error))
            raise

    def panel_feed_status(self, actor_id, monitor_id, panel_version, state_path, status, error=None, sequence=None):
        with self.lock, self.db() as db:
            actor = self.agent(actor_id, db)
            if actor.get("deletedAt"):
                return False
            panel = self.panel(actor_id, db)
            feed = panel.get("feed", {})
            if panel["version"] != panel_version or feed.get("monitorId") != monitor_id or feed.get("statePath") != state_path:
                return False
            # Late callbacks cannot reopen or overwrite a terminal lease.
            terminal = {"cancelled", "lost", "failed", "completed"}
            current = feed.get("status")
            if current in terminal and status != current:
                return False
            if current == "stopping" and status not in terminal | {"stopping"}:
                return False
            if current == "running" and status in {"starting", "approval"}:
                return False
            if status in {"starting", "approval", "running"} and (not actor["autoWake"] or actor["epoch"] != feed.get("epoch")):
                return False
            next_error = str(error)[:1000] if error else None
            if feed.get("status") == status and feed.get("error") == next_error:
                return True
            feed.update(status=status, error=next_error)
            self.write_panel_data(db, actor, panel)
            return True

    def panel_feed_cancelled(self, db, monitor_id):
        row = db.execute("SELECT record FROM runtime_monitors WHERE id=?", (monitor_id,)).fetchone()
        monitor = json.loads(row[0]) if row else None
        return not monitor or monitor.get("cancelRequested") or monitor["status"] in {"cancelled", "lost"}

    def panel_feed_update(self, actor_id, monitor_id, panel_version, state_path, state, sequence):
        slot = self.feed_slot(state_path)
        if not isinstance(state, dict):
            raise ValueError("Each panel feed line must contain one JSON object")
        encoded = json.dumps(state, ensure_ascii=False, allow_nan=False).encode("utf-8")
        if len(encoded) > 65536:
            raise ValueError("A panel feed frame must contain at most 64 KiB")
        state = json.loads(encoded)
        with self.lock, self.db() as db:
            actor = self.agent(actor_id, db)
            panel = self.panel(actor_id, db)
            feed = panel.get("feed", {})
            if (panel["version"] != panel_version or feed.get("monitorId") != monitor_id or feed.get("statePath") != state_path
                    or not actor["autoWake"] or actor["epoch"] != feed.get("epoch") or self.panel_feed_cancelled(db, monitor_id)
                    or feed.get("status") not in {"starting", "running"} or sequence <= feed.get("sequence", 0)):
                return False
            candidate = copy.deepcopy(panel)
            candidate["spec"]["state"][slot] = state
            changed = candidate["spec"]["state"][slot] != panel["spec"]["state"][slot]
        if changed:
            self.capture_panel(candidate, strict_layout=True)
        with self.lock, self.db() as db:
            actor = self.agent(actor_id, db)
            panel = self.panel(actor_id, db)
            feed = panel.get("feed", {})
            if (panel["version"] != panel_version or feed.get("monitorId") != monitor_id or feed.get("statePath") != state_path
                    or not actor["autoWake"] or actor["epoch"] != feed.get("epoch") or self.panel_feed_cancelled(db, monitor_id)
                    or feed.get("status") not in {"starting", "running"} or sequence <= feed.get("sequence", 0)):
                return False
            panel["spec"]["state"][slot] = state
            feed.update(status="running", error=None, sequence=sequence, updated=time.time())
            self.write_panel_data(db, actor, panel)
            return True

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
