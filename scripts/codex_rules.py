"""Durable rules and interactive commands, without model calls during waits."""

import base64
import json
import os
from pathlib import Path
import threading
import time
import uuid
from codex_work import text_field


def rule_tools(tool, text):
    return [
        tool(
            "orchestration_watch",
            "Manage durable file or event watches and scheduled commands. No model runs while waiting. File watches wake after no change for stall_timeout_seconds (default 1800; 0 disables stall wakes). Set liveness_command to run a sandboxed check at the stall point; its result is included in the wake. A normal script rule wakes only on exit code 0, unless its final JSON line has wakeAgent:false. Use bounded schedules, not model polling. A lead can enable low_workers to receive one alert when active subagents stay below minimumWorkers (default 8) for durationMinutes (default 30). Recovery to the threshold arms it again. Active means starting, running, or executing a command monitor; queued or idle workers without active commands do not count. Panel feeds do not count.",
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
                "stallTimeoutSeconds": {"type": "integer", "minimum": 0, "maximum": 31536000},
                "livenessCommand": text,
                "stall_timeout_seconds": {"type": "integer", "minimum": 0, "maximum": 31536000},
                "liveness_command": text,
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
            if r.get("kind") == "file" and "fileActivityAt" not in r:
                r.update(fileActivityAt=time.time(), fileGeneration=0, stallWakeGeneration=-1,
                         stallTimeoutSeconds=1800, livenessCommand="")
                self.put(db, "rules", r)
            if r.get("kind") == "low_workers":
                # Offline time cannot prove a continuous shortage. Keep a sent alert latched.
                r.update(lowSince=None, nextAt=time.time())
                self.put(db, "rules", r)
            if r.get("inFlight"):
                if r.get("status") == "active":
                    r["restartCheck"] = {
                        "epoch": r["epoch"], "checks": r["checks"],
                        "monitorId": str(uuid.uuid5(uuid.NAMESPACE_URL,
                                                   "rule:" + r["id"] + ":" + str(r["checks"]))),
                    }
                else:
                    r.pop("restartCheck", None)
                r.update(
                    inFlight=False,
                    status="paused",
                    error="Server restarted during a check. Outcome unknown; check was not repeated.",
                )
                self.put(db, "rules", r)

    def rules(self, data=None, actor=None, epoch=None):
        result = self.rules_action(data, actor, epoch)
        if data and data.get("action") in {"pause", "delete"}:
            from codex_workspace import active_monitors
            with self.lock, self.db() as db:
                watches = [
                    m["id"]
                    for m in active_monitors(db)
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
                old.pop("restartCheck", None)
                if action == "delete":
                    db.execute("DELETE FROM runtime_rules WHERE id=?", (key,))
                    from codex_sync_entities import put as sync_entity_put
                    sync_entity_put(db, "rule", key, {}, deleted=True)
                    return {"deleted": key}
                if old.get("kind") == "low_workers":
                    if old["status"] == ("active" if action == "resume" else "paused") and old["epoch"] == a["epoch"]:
                        return old
                    old.update(lowSince=None, alerted=False)
                if action == "resume" and old.get("kind") == "file":
                    old["fileActivityAt"] = time.time()
                    old["fingerprint"] = self.file_fingerprint(old["path"])
                    old["fileGeneration"] = old.get("fileGeneration", 0) + 1
                    old["stallWakeGeneration"] = old["fileGeneration"] - 1
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
            stall_timeout = data.get("stallTimeoutSeconds", data.get("stall_timeout_seconds", 1800))
            if type(stall_timeout) is not int or not 0 <= stall_timeout <= 31536000:
                raise ValueError("stall_timeout_seconds must be 0 to 31536000")
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
            liveness_command = text_field(data.get("livenessCommand", data.get("liveness_command", "")), "a liveness command", 12000, empty=True)
            if liveness_command and kind != "file":
                raise ValueError("liveness_command is only supported for file watches")
            if stall_timeout and kind != "file":
                stall_timeout = 0
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
                "stallTimeoutSeconds": stall_timeout,
                "livenessCommand": liveness_command,
                "fileActivityAt": old.get("fileActivityAt", time.time()) if old else time.time(),
                "fileGeneration": old.get("fileGeneration", 0) if old else 0,
                "stallWakeGeneration": old.get("stallWakeGeneration", -1) if old else -1,
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
        if self.lock._is_owned():
            raise RuntimeError("Rule ticks must run outside the runtime lock")
        if self.closed:
            return
        now = time.time()
        launch = []

        def stalled(r):
            return (r["kind"] == "file" and r.get("stallTimeoutSeconds", 1800)
                    and now - r.get("fileActivityAt", r.get("created", now)) >= r["stallTimeoutSeconds"]
                    and r.get("fileGeneration", 0) > r.get("stallWakeGeneration", -1))

        owner_fields = ("epoch", "accountKey", "threadId", "autoWake", "deletedAt",
                        "restartRecovery", "disconnectRecovery", "nativeFailureHold", "approvalPolicy",
                        "sandbox", "profile", "role")

        def owner_reader(db):
            owners = {}
            generation = None

            def read(key):
                nonlocal generation
                current = (db.total_changes, self.__dict__.get("_agent_record_revision", 0))
                if current != generation:
                    owners.clear()
                    generation = current
                if key not in owners:
                    owners[key] = self.agent(key, db)
                return owners[key]

            return read

        snapshots = []
        with self.read_db() as db:
            read_owner = owner_reader(db)
            for r in self.records(db, "rules"):
                if r["status"] == "active" and not r.get("inFlight"):
                    a = read_owner(r["agent"])
                    snapshots.append((r, {field: a.get(field) for field in owner_fields}))
        # A slow filesystem must not hold the runtime lock or a SQLite writer.
        fingerprints = {}
        for r, owner in snapshots:
            if (r["kind"] == "file" and not owner["deletedAt"] and owner["autoWake"]
                    and owner["epoch"] == r["epoch"]
                    and not self.rule_owner_recovery_pending(owner, r["epoch"])
                    and (stalled(r) or r["nextAt"] <= now)):
                fingerprints[r["id"]] = self.file_fingerprint(r["path"])
        with self.lock, self.db() as db:
            if self.closed:
                return
            read_owner = owner_reader(db)
            current_rules = {r["id"]: r for r in self.records(db, "rules")}
            for snapshot, owner in snapshots:
                r = current_rules.get(snapshot["id"])
                # Another tick, edit, or stop invalidates the sampled configuration.
                if r != snapshot:
                    continue
                a = read_owner(r["agent"])
                if any(a.get(field) != owner[field] for field in owner_fields):
                    continue
                if (not a.get("deletedAt") and a.get("epoch") == r["epoch"]
                        and self.rule_owner_recovery_pending(a, r["epoch"])):
                    # Preserve the active watch while exact native recovery runs.
                    # Dispatch remains blocked by the agent's recovery state.
                    continue
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
                if r["kind"] == "event":
                    continue
                if stalled(r):
                    fingerprint = fingerprints[r["id"]]
                    if fingerprint != r.get("fingerprint"):
                        r.update(fingerprint=fingerprint, fileActivityAt=now,
                                 fileGeneration=r.get("fileGeneration", 0) + 1)
                        self.put(db, "rules", r)
                        continue
                    generation = r.get("fileGeneration", 0)
                    quiet = int(now - r.get("fileActivityAt", now))
                    stall_text = json.dumps({"rule": r["id"], "name": r["name"],
                        "message": "file unchanged", "path": r["path"], "quietSeconds": quiet})
                    if r.get("livenessCommand"):
                        r.update(inFlight=True, checks=r.get("checks", 0) + 1,
                                 stallProbe=True, stallEventKey=f"rule-stall:{r['id']}:{generation}",
                                 stallText=stall_text)
                        launch.append(r.copy())
                    else:
                        self.enqueue_recovery_event(db, a, "rule_stall", stall_text,
                            f"rule-stall:{r['id']}:{generation}")
                        r["stallWakeGeneration"] = generation
                    self.put(db, "rules", r)
                    continue
                if r["nextAt"] > now:
                    continue
                r["nextAt"] = now + r["intervalSeconds"]
                fire = True
                if r["kind"] == "file":
                    fingerprint = fingerprints[r["id"]]
                    fire = fingerprint != r.get("fingerprint")
                    if fire:
                        r["fileActivityAt"] = now
                        r["fileGeneration"] = r.get("fileGeneration", 0) + 1
                    r["fingerprint"] = fingerprint
                if fire:
                    r.update(inFlight=True, lastAt=now, checks=r.get("checks", 0) + 1)
                    launch.append(r.copy())
                self.put(db, "rules", r)
        for r in launch:
            self.pool.submit(self.run_rule, r)

    @staticmethod
    def rule_owner_recovery_pending(agent, epoch):
        restart = agent.get("restartRecovery") or {}
        if (restart.get("stage") == "pending" and restart.get("autoWake")
                and restart.get("epoch") == epoch
                and all(restart.get(field) == agent.get(field)
                        for field in ("epoch", "accountKey", "threadId"))):
            return True
        disconnect = agent.get("disconnectRecovery") or {}
        # Keep the receipt as evidence after recovery. Its old permission must
        # not freeze watches on an agent that can already continue.
        return bool((not agent.get("autoWake") or agent.get("nativeFailureHold"))
                    and disconnect.get("autoWake")
                    and disconnect.get("epoch") == epoch
                    and all(disconnect.get(source) == agent.get(target)
                            for source, target in (("epoch", "epoch"),
                                                   ("accountKey", "accountKey"),
                                                   ("threadId", "threadId"))))

    def low_workers_tick(self, db, rule, lead, now):
        from codex_workspace import active_monitors
        workers = [a for a in self.team_agents(db, lead["id"])
                   if not a.get("isLead")]
        commands = {m["agent"] for m in active_monitors(db)
                    if m.get("status") == "running" and not m.get("cancelRequested")}
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
            command = r.get("livenessCommand") if r.get("stallProbe") else r["command"]
            if command:
                a = self.prepare(a)
                approved = self.monitor_auto_approved(a)
                if r.get("stallProbe") and not approved:
                    self.rule_finished(r["id"], None,
                        "Liveness command not run because monitor approval is required.", "")
                    return
                self.monitor(
                    r["agent"],
                    {
                        "command": command,
                        "timeout_ms": min(
                            86400000, max(1000, r["intervalSeconds"] * 1000)
                        ),
                        "stall_timeout_seconds": 0,
                    },
                    "rule:" + r["id"] + ":" + str(r["checks"]),
                    approved=approved,
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
        if r.get("stallProbe"):
            generation = r.get("fileGeneration", 0)
            payload = {"rule": r["id"], "name": r["name"], "message": "file unchanged",
                       "path": r["path"], "quietSeconds": int(time.time() - r.get("fileActivityAt", time.time())),
                       "livenessResult": output[-12000:], "livenessExitCode": code,
                       "livenessError": error}
            restart = a.get("restartRecovery") or {}
            pending_recovery = (not a.get("autoWake") and a.get("status") != "paused"
                and not a.get("deletedAt") and restart.get("stage") == "pending" and restart.get("autoWake")
                and all(restart.get(field) == a.get(field) for field in ("epoch", "accountKey", "threadId")))
            if (r["status"] == "active" and a["epoch"] == r["epoch"]
                    and (a.get("autoWake") or pending_recovery) and not a.get("deletedAt")):
                self.enqueue_recovery_event(db, a, "rule_stall", json.dumps(payload),
                                             r.get("stallEventKey"))
            r.update(inFlight=False, stallProbe=False, stallWakeGeneration=generation,
                     lastStallFinished=time.time(), lastStallExitCode=code, lastStallError=error)
            self.put(db, "rules", r)
            return
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
        restart = a.get("restartRecovery") or {}
        pending_recovery = (not a.get("autoWake") and a.get("status") != "paused"
            and not a.get("deletedAt") and restart.get("stage") == "pending" and restart.get("autoWake")
            and all(restart.get(field) == a.get(field) for field in ("epoch", "accountKey", "threadId")))
        if (
            wake
            and r["status"] == "active"
            and (a["autoWake"] or pending_recovery)
            and a["epoch"] == r["epoch"]
        ):
            self.enqueue_recovery_event(
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
        from codex_native_errors import NativeRpcError
        from codex_runtime import SubmissionRejected

        def load(db):
            row = db.execute("SELECT record FROM runtime_monitors WHERE id=?", (key,)).fetchone()
            if not row:
                raise ValueError("Unknown monitor")
            monitor = json.loads(row[0])
            agent = self.agent(monitor["agent"], db)
            if owner and owner != monitor["agent"]:
                raise ValueError("This monitor belongs to another agent")
            if epoch is not None and agent["epoch"] != epoch:
                raise ValueError("The caller was stopped")
            if (self.closed or agent.get("deletedAt") or monitor["status"] != "running"
                    or monitor.get("cancelRequested") or not monitor.get("interactive")
                    or not agent["autoWake"] or agent["epoch"] != monitor["epoch"]):
                raise ValueError("This interactive monitor is not active")
            return monitor, agent

        def identity(monitor, agent):
            return (monitor["agent"], monitor["epoch"], monitor.get("operation"),
                    monitor.get("created"), monitor.get("command"),
                    agent["epoch"], agent.get("accountKey", "default"), agent.get("threadId"))

        with self.lock:
            guard = self.__dict__.setdefault("_monitor_input_locks", {}).setdefault(key, threading.Lock())
        # Serialize this monitor only. Connect and native writes can wait for I/O.
        with guard:
            with self.lock, self.db() as db:
                monitor, agent = load(db)
                expected = identity(monitor, agent)
                account_key = agent.get("accountKey", "default")
            server = self.connect(account_key)
            connection_id = self.connection_ids.get(account_key)
            with self.lock, self.db() as db:
                monitor, agent = load(db)
                if (identity(monitor, agent) != expected
                        or not connection_id or not self.connection_current(account_key, connection_id)
                        or self.servers.get(account_key) is not server or getattr(server, "closed", False)):
                    raise ValueError("The monitor changed before input submission")
                if monitor.get("inputAttempt"):
                    raise ValueError("The previous monitor input outcome is unknown; do not repeat it")
                close = False
                if "rows" in data or "cols" in data:
                    rows, cols = data.get("rows", 24), data.get("cols", 80)
                    if not all(isinstance(value, int) and 1 <= value <= 1000 for value in (rows, cols)):
                        raise ValueError("Terminal size must be 1 to 1000")
                    method = "command/exec/resize"
                    params = {"processId": key, "size": {"rows": rows, "cols": cols}}
                else:
                    text = data.get("text", "")
                    if not isinstance(text, str) or len(text) > 32000:
                        raise ValueError("Input must have at most 32000 characters")
                    if monitor.get("stdinClosed") or monitor.get("stdinCloseRequested"):
                        raise ValueError("This monitor stdin is closed or its close acknowledgement is pending")
                    close = bool(data.get("closeStdin", False))
                    method = "command/exec/write"
                    params = {"processId": key, "deltaBase64": base64.b64encode(text.encode()).decode(),
                              "closeStdin": close}
                attempt = {"id": str(uuid.uuid4()), "agent": agent["id"], "epoch": agent["epoch"],
                           "accountKey": account_key, "connectionId": connection_id,
                           "method": method, "params": params, "status": "submitted", "created": time.time()}
                monitor["inputAttempt"] = attempt
                if close:
                    monitor["stdinCloseRequested"] = attempt["id"]
                    monitor.pop("stdinError", None)
                self.put(db, "monitors", monitor)
                # This exact request is accepted before native I/O. Stop may now
                # cancel the command; it cannot make this request safe to replay.
                db.commit()

            def settle(error=None):
                # Transport failures can use any message. Only a native rejection
                # or proof that no bytes were submitted releases this receipt.
                unknown = error is not None and not isinstance(error, (NativeRpcError, SubmissionRejected))
                with self.lock, self.db() as db:
                    if (self.closed or not self.connection_current(account_key, connection_id)
                            or self.servers.get(account_key) is not server):
                        return
                    row = db.execute("SELECT record FROM runtime_monitors WHERE id=?", (key,)).fetchone()
                    if not row:
                        return
                    latest = json.loads(row[0])
                    current = self.agent(latest["agent"], db)
                    if (identity(latest, current) != expected or not current.get("autoWake")
                            or current.get("deletedAt") or latest.get("cancelRequested")
                            or latest.get("status") != "running"
                            or (latest.get("inputAttempt") or {}).get("id") != attempt["id"]):
                        return
                    receipt = {**attempt, "status": "uncertain" if unknown else "failed" if error else "acknowledged",
                               "finished": time.time(), **({"error": str(error)} if error else {})}
                    if unknown:
                        latest["inputAttempt"] = receipt
                    else:
                        latest.pop("inputAttempt", None)
                        latest["lastInputAttempt"] = receipt
                    if close and latest.get("stdinCloseRequested") == attempt["id"]:
                        if not unknown:
                            latest.pop("stdinCloseRequested", None)
                        if error:
                            latest["stdinError"] = str(error)
                        else:
                            latest["stdinClosed"] = True
                            latest.pop("stdinError", None)
                    self.put(db, "monitors", latest)

            def reconcile(future):
                try:
                    future.result()
                except Exception as error:
                    settle(error)
                else:
                    settle()

            try:
                submitted = self.submit_reserved(server, method, params,
                    operation_id="monitor-input:" + key + ":" + attempt["id"])
            except Exception as error:
                settle(error)
                raise
            server.on_result(submitted, reconcile)
            # The reader stays free to deliver output before the acknowledgement.
            try:
                result = server.wait(submitted)
            except Exception as error:
                settle(error)
                raise
            # The reply may arrive before its queued callback. Do not leave the
            # next input waiting behind unrelated streamed output.
            settle()
            return result
