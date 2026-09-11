"""Durable rules and interactive commands, without model calls during waits."""

import base64
import json
import os
from pathlib import Path
import subprocess
import sys
import threading
import time
import uuid
from codex_state import board_dir
from codex_work import text_field


def rule_tools(tool, text):
    return [
        tool(
            "orchestration_watch",
            "Manage durable file or event watches and scheduled commands. No model runs while waiting. A script exit other than zero does not wake you. A final JSON line with wakeAgent:false also suppresses a wake. Use bounded schedules, not model polling. A lead can enable low_workers to receive one alert when active subagents stay below minimumWorkers (default 8) for durationMinutes (default 30). Recovery to the threshold arms it again. Active means starting, running, or executing a command monitor; queued or idle workers without active commands do not count. Panel feeds do not count.",
            {
                "action": {
                    "type": "string",
                    "enum": ["list", "save", "pause", "resume", "delete"],
                },
                "id": text,
                "name": text,
                "kind": {
                    "type": "string",
                    "enum": ["interval", "once", "file", "event", "low_workers"],
                },
                "intervalSeconds": {"type": "integer", "minimum": 10},
                "at": {"type": "number"},
                "path": text,
                "event": {
                    "type": "string",
                    "enum": [
                        "worker_completed",
                        "monitor_exit",
                        "work_review",
                        "complaint",
                    ],
                },
                "minimumWorkers": {"type": "integer", "minimum": 1, "maximum": 255},
                "durationMinutes": {"type": "integer", "minimum": 1, "maximum": 525600},
                "command": text,
                "text": text,
            },
            ["action"],
        ),
        tool(
            "orchestration_resource",
            "Use the existing codex-board resource registry. Claims are atomic. Busy claims return the holder; never assume a stale owner is dead. Owner is always your agent id.",
            {
                "action": {
                    "type": "string",
                    "enum": ["show", "claim", "renew", "release"],
                },
                "resource": text,
                "note": text,
            },
            ["action"],
        ),
        tool(
            "orchestration_monitor_input",
            "Write to your interactive monitor session, resize its terminal, or close stdin. Only command/exec monitor ids are supported.",
            {
                "monitor_id": text,
                "text": text,
                "closeStdin": {"type": "boolean"},
                "rows": {"type": "integer"},
                "cols": {"type": "integer"},
            },
            ["monitor_id"],
        ),
    ]


class RulesMixin:
    def setup_rules(self, db):
        db.execute(
            "CREATE TABLE IF NOT EXISTS runtime_rules (id TEXT PRIMARY KEY, record TEXT NOT NULL)"
        )
        for r in self.records(db, "rules"):
            if r.get("kind") == "low_workers":
                # Offline time cannot prove a continuous shortage. Keep a sent alert latched.
                r.update(lowSince=None, nextAt=time.time())
                self.put(db, "rules", r)
            if r.get("inFlight"):
                r.update(
                    inFlight=False,
                    status="paused",
                    error="Server restarted during a check. Outcome unknown; check was not repeated.",
                )
                self.put(db, "rules", r)

    def rules(self, data=None, actor=None, epoch=None):
        result = self.rules_action(data, actor, epoch)
        if data and data.get("action") in {"pause", "delete"}:
            with self.lock, self.db() as db:
                watches = [
                    m["id"]
                    for m in self.records(db, "monitors")
                    if m.get("ruleId") == data.get("id")
                    and m["status"] in {"running", "starting", "approval"}
                ]
            for watch in watches:
                self.cancel_monitor(watch)
        return result

    def rules_action(self, data=None, actor=None, epoch=None):
        with self.lock, self.db() as db:
            if actor and epoch is not None and self.agent(actor, db)["epoch"] != epoch:
                raise ValueError("The caller was stopped")
            if data is None or data.get("action") == "list":
                root = self.agent(actor, db)["rootId"] if actor else None
                return {
                    "rules": [
                        r
                        for r in self.records(db, "rules")
                        if (not root or r["rootId"] == root)
                        and not self.agent(r["agent"], db).get("deletedAt")
                    ]
                }
            a = self.checked_actor(db, actor or data.get("agent"), actor)
            key = data.get("id") or str(uuid.uuid4())
            old = next((r for r in self.records(db, "rules") if r["id"] == key), None)
            if old and old["agent"] != a["id"]:
                raise ValueError("This rule belongs to another agent")
            action = data.get("action", "save")
            if action in {"pause", "resume", "delete"}:
                if not old:
                    raise ValueError("Unknown rule")
                if action == "resume" and old.get("inFlight"):
                    raise ValueError("Wait for the current rule check before resuming")
                if action == "delete":
                    db.execute("DELETE FROM runtime_rules WHERE id=?", (key,))
                    return {"deleted": key}
                if old.get("kind") == "low_workers":
                    if old["status"] == ("active" if action == "resume" else "paused") and old["epoch"] == a["epoch"]:
                        return old
                    old.update(lowSince=None, alerted=False)
                old.update(
                    status="active" if action == "resume" else "paused",
                    epoch=a["epoch"],
                    nextAt=time.time() + old.get("intervalSeconds", 10),
                    error=None,
                )
                if action == "resume" and not a["autoWake"]:
                    raise ValueError("Resume the agent before its rules")
                self.put(db, "rules", old)
                return old
            if action != "save":
                raise ValueError("Unknown rule action")
            if old and old.get("inFlight"):
                raise ValueError(
                    "Pause the rule and wait for its current check before editing"
                )
            kind = data.get("kind", "interval")
            if kind not in {"interval", "once", "file", "event", "low_workers"}:
                raise ValueError("Choose interval, once, file, event, or low_workers")
            minimum, duration = data.get("minimumWorkers", 8), data.get("durationMinutes", 30)
            if kind == "low_workers":
                if not a.get("isLead"):
                    raise ValueError("Only an orchestrator can own a low-worker alert")
                if type(minimum) is not int or not 1 <= minimum <= 255:
                    raise ValueError("Minimum subagents must be 1 to 255")
                if type(duration) is not int or not 1 <= duration <= 525600:
                    raise ValueError("Duration must be 1 to 525600 minutes")
                if data.get("command"):
                    raise ValueError("A low-worker alert does not run a command")
            interval = data.get("intervalSeconds", 60)
            if not isinstance(interval, int) or not 10 <= interval <= 31536000:
                raise ValueError("Interval must be 10 seconds to one year")
            at = data.get("at", time.time() + interval)
            if (
                not isinstance(at, (int, float))
                or not time.time() - 1 <= at <= time.time() + 31536000
            ):
                raise ValueError("Choose a date within the next year")
            path = None
            if kind == "file":
                path = str(
                    self.workspace_path(
                        a["id"], text_field(data.get("path"), "a path", 4096)
                    )
                )
            event = data.get("event", "worker_completed")
            if event not in {
                "worker_completed",
                "monitor_exit",
                "work_review",
                "complaint",
            }:
                raise ValueError("Unknown event type")
            command = text_field(
                data.get("command", ""), "a command", 12000, empty=True
            )
            rule = {
                "id": key,
                "agent": a["id"],
                "rootId": a["rootId"],
                "epoch": a["epoch"],
                "name": text_field(data.get("name"), "a rule name", 120),
                "kind": kind,
                "intervalSeconds": interval,
                "nextAt": at,
                "at": at,
                "path": path,
                "event": event,
                "command": command,
                "text": text_field(
                    data.get(
                        "text", "Check this event and continue the original task."
                    ),
                    "an instruction",
                ),
                "status": "active" if a["autoWake"] else "paused",
                "created": old["created"] if old else time.time(),
                "inFlight": False,
                "checks": old.get("checks", 0) if old else 0,
                "wakes": old.get("wakes", 0) if old else 0,
                "fingerprint": self.file_fingerprint(path) if path else None,
            }
            if kind == "low_workers":
                rule.update(minimumWorkers=minimum, durationMinutes=duration,
                            lowSince=None, alerted=False, nextAt=time.time())
                fields = ("kind", "name", "text", "minimumWorkers", "durationMinutes", "epoch")
                if old and all(old.get(k) == rule.get(k) for k in fields):
                    return old
            self.put(db, "rules", rule)
            self.changed.set()
            return rule

    @staticmethod
    def file_fingerprint(path):
        try:
            p = Path(path)
            s = p.stat()
            return [s.st_mtime_ns, s.st_size, s.st_ino]
        except FileNotFoundError:
            return None

    def rules_tick(self):
        now = time.time()
        launch = []
        with self.lock, self.db() as db:
            for r in self.records(db, "rules"):
                if r["status"] != "active" or r.get("inFlight"):
                    continue
                a = self.agent(r["agent"], db)
                if a.get("deletedAt") or not a["autoWake"] or a["epoch"] != r["epoch"]:
                    r.update(
                        status="paused",
                        error="The agent was stopped. Resume this rule explicitly.",
                    )
                    self.put(db, "rules", r)
                    continue
                if r["kind"] == "low_workers":
                    self.low_workers_tick(db, r, a, now)
                    continue
                if r["kind"] == "event" or r["nextAt"] > now:
                    continue
                r["nextAt"] = now + r["intervalSeconds"]
                fire = True
                if r["kind"] == "file":
                    fingerprint = self.file_fingerprint(r["path"])
                    fire = fingerprint != r.get("fingerprint")
                    r["fingerprint"] = fingerprint
                if fire:
                    r.update(inFlight=True, lastAt=now, checks=r.get("checks", 0) + 1)
                    launch.append(r.copy())
                self.put(db, "rules", r)
        for r in launch:
            self.pool.submit(self.run_rule, r)

    def low_workers_tick(self, db, rule, lead, now):
        workers = [a for a in self.records(db, "agents")
                   if a["rootId"] == lead["id"] and not a.get("isLead") and not a.get("deletedAt")]
        commands = {m["agent"] for m in self.records(db, "monitors")
                    if m.get("status") == "running" and not m.get("cancelRequested") and not m.get("panelFeed")}
        count = sum(a["status"] in {"starting", "running"} or a["id"] in commands for a in workers)
        previous = (rule.get("lowSince"), rule.get("alerted"), rule.get("activeWorkers"))
        rule["activeWorkers"] = count
        if count >= rule["minimumWorkers"]:
            rule.update(lowSince=None, alerted=False)
        else:
            if rule.get("lowSince") is None or now < rule["lowSince"]:
                rule["lowSince"] = now
            elapsed = now - rule["lowSince"]
            if not rule.get("alerted") and elapsed > rule["durationMinutes"] * 60:
                # Store the event and latch in one transaction. No command or model polls.
                rule.update(alerted=True, lastAt=now, checks=rule.get("checks", 0) + 1)
                output = json.dumps({"activeSubagents": count, "minimumSubagents": rule["minimumWorkers"],
                                     "belowThresholdSince": rule["lowSince"], "elapsedMinutes": elapsed / 60})
                self.put(db, "rules", rule)
                self.rule_finished(rule["id"], 0, None, output, db=db)
                return
        current = (rule.get("lowSince"), rule.get("alerted"), rule.get("activeWorkers"))
        if previous != current:
            self.put(db, "rules", rule)

    def rule_event(self, db, a, kind, text, event_key):
        # Rules are same-agent subscriptions. Rule wakes never re-enter this hook.
        alias = {
            "child_completed": "worker_completed",
            "child_result": "worker_completed",
            "complaint_submitted": "complaint",
        }.get(kind, kind)
        for r in self.records(db, "rules"):
            if (
                r["agent"] == a["id"]
                and r["kind"] == "event"
                and r["event"] == alias
                and r["status"] == "active"
                and r["epoch"] == a["epoch"]
                and not r.get("inFlight")
                and r.get("lastEvent") != event_key
            ):
                r.update(
                    inFlight=True,
                    lastEvent=event_key,
                    lastAt=time.time(),
                    checks=r.get("checks", 0) + 1,
                    eventText=text[:12000],
                )
                self.put(db, "rules", r)
                self.pool.submit(self.run_rule, r.copy())

    def run_rule(self, r):
        try:
            with self.lock, self.db() as db:
                row = db.execute(
                    "SELECT record FROM runtime_rules WHERE id=?", (r["id"],)
                ).fetchone()
                if not row:
                    return
                current = json.loads(row[0])
                a = self.agent(r["agent"], db)
                if (
                    current["status"] != "active"
                    or not a["autoWake"]
                    or a["epoch"] != r["epoch"]
                ):
                    current["inFlight"] = False
                    self.put(db, "rules", current)
                    return
            if r["command"]:
                a = self.prepare(a)
                self.monitor(
                    r["agent"],
                    {
                        "command": r["command"],
                        "timeout_ms": min(
                            86400000, max(1000, r["intervalSeconds"] * 1000)
                        ),
                    },
                    "rule:" + r["id"] + ":" + str(r["checks"]),
                    approved=self.monitor_auto_approved(a),
                    epoch=r["epoch"],
                    rule=r,
                )
            else:
                self.rule_finished(r["id"], 0, None, r.get("eventText", ""))
        except Exception as error:
            from codex_runtime import PreparationPending
            if isinstance(error, PreparationPending):
                self.defer_preparation(error, lambda: self.run_rule(r),
                    lambda cause: self.rule_finished(r["id"], None, str(cause), ""))
                return
            self.rule_finished(r["id"], None, str(error), "")

    def rule_finished(self, key, code, error, output, db=None):
        if db is None:
            with self.lock, self.db() as connection:
                return self.rule_finished(key, code, error, output, connection)
        row = db.execute(
            "SELECT record FROM runtime_rules WHERE id=?", (key,)
        ).fetchone()
        if not row:
            return
        r = json.loads(row[0])
        a = self.agent(r["agent"], db)
        wake = code == 0 and not error
        try:
            payload = json.loads(output.strip().splitlines()[-1])
            if isinstance(payload, dict) and payload.get("wakeAgent") is False:
                wake = False
        except (ValueError, IndexError):
            pass
        r.update(
            inFlight=False,
            lastExitCode=code,
            error=error,
            lastOutput=output[-12000:],
            lastFinished=time.time(),
        )
        if (
            wake
            and r["status"] == "active"
            and a["autoWake"]
            and a["epoch"] == r["epoch"]
        ):
            self.enqueue(
                db,
                a,
                "rule",
                json.dumps(
                    {
                        "rule": r["id"],
                        "name": r["name"],
                        "instruction": r["text"],
                        "output": output[-12000:],
                    }
                ),
                "rule-wake:" + r["id"] + ":" + str(r["checks"]),
            )
            r["wakes"] = r.get("wakes", 0) + 1
        if r["kind"] == "once" and r["status"] == "active":
            r["status"] = "completed"
        self.put(db, "rules", r)

    def monitor_input(self, key, data, owner=None, epoch=None):
        close_operation = None
        with self.lock, self.db() as db:
            row = db.execute(
                "SELECT record FROM runtime_monitors WHERE id=?", (key,)
            ).fetchone()
            if not row:
                raise ValueError("Unknown monitor")
            m = json.loads(row[0])
            a = self.agent(m["agent"], db)
            if owner and owner != m["agent"]:
                raise ValueError("This monitor belongs to another agent")
            if epoch is not None and a["epoch"] != epoch:
                raise ValueError("The caller was stopped")
            if (
                m["status"] != "running"
                or m.get("cancelRequested")
                or not m.get("interactive")
                or not a["autoWake"]
                or a["epoch"] != m["epoch"]
            ):
                raise ValueError("This interactive monitor is not active")
            server = self.connect(a.get("accountKey", "default"))
            close = False
            if "rows" in data or "cols" in data:
                rows, cols = data.get("rows", 24), data.get("cols", 80)
                if not all(isinstance(v, int) and 1 <= v <= 1000 for v in (rows, cols)):
                    raise ValueError("Terminal size must be 1 to 1000")
                submitted = server.submit(
                    "command/exec/resize",
                    {"processId": key, "size": {"rows": rows, "cols": cols}},
                )
            else:
                text = data.get("text", "")
                if not isinstance(text, str) or len(text) > 32000:
                    raise ValueError("Input must have at most 32000 characters")
                if m.get("stdinClosed") or m.get("stdinCloseRequested"):
                    raise ValueError("This monitor stdin is closed or its close acknowledgement is pending")
                close = bool(data.get("closeStdin", False))
                if close:
                    close_operation = str(uuid.uuid4())
                    account_key = a.get("accountKey", "default")
                    connection_id = self.connection_ids[account_key]
                    m.update(stdinCloseRequested=close_operation)
                    m.pop("stdinError", None)
                    self.put(db, "monitors", m)
                    # A write failure does not prove that the server received no bytes.
                    db.commit()
                submitted = self.submit_reserved(server,
                    "command/exec/write",
                    {
                        "processId": key,
                        "deltaBase64": base64.b64encode(text.encode()).decode(),
                        "closeStdin": close,
                    },
                )
        if close_operation:
            def reconcile(future):
                try:
                    future.result()
                    error = None
                except Exception as cause:
                    error = str(cause)
                with self.lock:
                    if self.closed or not self.connection_current(account_key, connection_id):
                        return
                    with self.db() as db:
                        row = db.execute("SELECT record FROM runtime_monitors WHERE id=?", (key,)).fetchone()
                        if not row:
                            return
                        latest = json.loads(row[0])
                        if latest.get("stdinCloseRequested") != close_operation:
                            return
                        if not error or "outcome unknown" not in error:
                            latest.pop("stdinCloseRequested", None)
                        if error:
                            latest["stdinError"] = error
                        else:
                            latest["stdinClosed"] = True
                            latest.pop("stdinError", None)
                        self.put(db, "monitors", latest)
            server.on_result(submitted, reconcile)
        # The app-server reader must stay free to deliver output before the acknowledgement.
        return server.wait(submitted)

    def resource_action(self, data=None, actor=None, epoch=None):
        with self.lock:
            if actor and epoch is not None and self.agent(actor)["epoch"] != epoch:
                raise ValueError("The caller was stopped")
            return self.resource_locked(data, actor)

    def resource_locked(self, data=None, actor=None):
        path = board_dir()
        if data is None or data.get("action") == "show":
            try:
                state = json.loads((path / "codex-board.json").read_text())
            except FileNotFoundError:
                state = {"claims": {}, "queue": {}, "notes": []}
            return {"path": str(path / "codex-board.json"), "state": state}
        a = self.checked_actor_in_own_db(actor or data.get("agent"), actor)
        action = data.get("action")
        if action not in {"claim", "renew", "release"}:
            raise ValueError("Choose claim, renew, or release")
        resource = text_field(data.get("resource"), "a resource name", 200)
        args = [
            sys.executable,
            "-B",
            str(Path(__file__).with_name("codex-board")),
            action,
            resource,
            a["id"],
        ]
        if action == "claim":
            args.append(text_field(data.get("note", ""), "a note", 2000, empty=True))
        result = subprocess.run(args, capture_output=True, text=True, timeout=10)
        snapshot = self.resource_action()
        holder = snapshot["state"].get("claims", {}).get(resource, {}).get("worker")
        return {
            **snapshot,
            "ok": result.returncode == 0,
            "exitCode": result.returncode,
            "message": (result.stdout + result.stderr).strip(),
            "action": action, "resource": resource, "agent": a["id"],
            "holder": holder,
            "ownsResource": holder == a["id"],
            # Exit 1 is the board's explicit claim/renew/release refusal.
            # A process error or timeout does not prove no board write occurred.
            "outcome": ("applied" if result.returncode == 0 else
                        "not_applied" if result.returncode == 1 else "unknown"),
        }
