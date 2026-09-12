"""Shared work, result acceptance, search, and conversation controls."""

import hashlib
import json
import time
import uuid

from codex_agent_management import management_tools


def text_field(value, name, maximum=32000, empty=False):
    if (
        not isinstance(value, str)
        or len(value) > maximum
        or (not empty and not value.strip())
    ):
        raise ValueError(
            f"Supply {name} with {'0' if empty else '1'} to {maximum} characters"
        )
    return value.strip()


def work_tools(tool, text):
    return management_tools(tool, text) + [
        tool(
            "orchestration_task",
            "Manage team assignments and review decisions. list returns brief items with nextCursor; get reads one task; history pages earlier evidence. "
            "claim reserves ready work atomically. submit saves evidence, sets review, and notifies the lead. "
            "Only the lead reviews results. accept records approval and releases dependent work. "
            "reject takes the reason and required corrections in result, sets ready, and delivers those instructions to the owner as work_decision. "
            "The owner continues from that event when automatic continuation is enabled. Explicit stops and native failure holds remain in effect. "
            "Mutations return brief receipts; the server owns state changes and event delivery.",
            {
                "action": {
                    "type": "string",
                    "enum": [
                        "list",
                        "get",
                        "history",
                        "create",
                        "claim",
                        "update",
                        "submit",
                        "accept",
                        "reject",
                    ],
                },
                "task_id": text,
                "limit": {"type": "integer", "minimum": 1, "maximum": 50},
                "cursor": text,
                "state": text,
                "title": text,
                "description": text,
                "owner": text,
                "dependencies": {"type": "array", "items": text},
                "status": {"type": "string", "enum": ["ready", "blocked"]},
                "result": text,
                "checks": text,
                "revision": text,
                "files": {"type": "array", "items": text},
                "version": {"type": "integer"},
            },
            ["action"],
        ),
        tool(
            "orchestration_result",
            "Submit evidence for lead review, or read earlier evidence. submit saves result, checks, revision, and files; sets the task to review; and notifies the lead. The lead records acceptance or required corrections with orchestration_task.",
            {
                "action": {"type": "string", "enum": ["submit", "read"]},
                "task_id": text,
                "result": text,
                "checks": text,
                "revision": text,
                "files": {"type": "array", "items": text},
            },
            ["action", "task_id"],
        ),
        tool(
            "orchestration_search",
            "Search your own messages and tool results, plus shared team work and complaints. Private rooms remain visible only to their participants. Results include exact record references.",
            {"query": text, "limit": {"type": "integer", "minimum": 1, "maximum": 100}},
            ["query"],
        ),
    ]


class WorkMixin:
    def setup_work(self, db):
        db.executescript("""
            CREATE TABLE IF NOT EXISTS runtime_work (id TEXT PRIMARY KEY, record TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS runtime_plans (id TEXT PRIMARY KEY, record TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS runtime_annotations (id TEXT PRIMARY KEY, record TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS runtime_operation_receipts (id TEXT PRIMARY KEY, signature TEXT NOT NULL, result TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS runtime_event_meta (id TEXT PRIMARY KEY, record TEXT NOT NULL);
            CREATE VIRTUAL TABLE IF NOT EXISTS runtime_search USING fts5(id UNINDEXED, agent UNINDEXED, kind UNINDEXED, body, tokenize='unicode61');
            CREATE TABLE IF NOT EXISTS runtime_search_indexed (id TEXT PRIMARY KEY);
        """)
        self.setup_search_rows(db)
        # Backfill once. Later writes update the index in the same transaction.
        for row in db.execute(
            "SELECT i.id,i.agent,i.record FROM runtime_items i LEFT JOIN runtime_search_indexed s ON i.id=s.id WHERE s.id IS NULL"
        ).fetchall():
            item = json.loads(row["record"])
            self.index_item(
                db, row["id"], row["agent"], item.get("title", ""), item.get("text", "")
            )

    def setup_search_rows(self, db):
        # FTS UNINDEXED columns cannot support an equality lookup. Keep the
        # document address in an ordinary indexed table, including legacy rows.
        if db.execute("SELECT 1 FROM sqlite_master WHERE name='runtime_search_rows'").fetchone():
            return
        db.execute("SAVEPOINT search_rows_migration")
        try:
            db.execute("CREATE TABLE runtime_search_rows (id TEXT PRIMARY KEY, search_rowid INTEGER NOT NULL UNIQUE)")
            db.execute("INSERT INTO runtime_search_rows SELECT id,rowid FROM runtime_search")
            db.execute("RELEASE search_rows_migration")
        except BaseException:
            db.execute("ROLLBACK TO search_rows_migration")
            db.execute("RELEASE search_rows_migration")
            raise

    def index_item(self, db, key, agent, kind, body):
        row = db.execute("SELECT search_rowid FROM runtime_search_rows WHERE id=?", (key,)).fetchone()
        if row:
            db.execute("DELETE FROM runtime_search WHERE rowid=?", (row[0],))
        cursor = db.execute(
            "INSERT INTO runtime_search(id,agent,kind,body) VALUES (?,?,?,?)",
            (key, agent, kind, body),
        )
        db.execute("INSERT INTO runtime_search_rows VALUES (?,?) ON CONFLICT(id) DO UPDATE SET search_rowid=excluded.search_rowid",
                   (key, cursor.lastrowid))
        db.execute("INSERT OR IGNORE INTO runtime_search_indexed VALUES (?)", (key,))

    def checked_actor(self, db, agent_id, actor=None):
        a = self.agent(agent_id, db)
        if a.get("deletedAt"):
            raise ValueError("This agent was deleted")
        if actor:
            caller = self.agent(actor, db)
            if caller.get("deletedAt") or not caller["autoWake"]:
                raise ValueError("The caller is stopped")
            if caller["rootId"] != a["rootId"]:
                raise ValueError("This record belongs to another team")
        return a

    def operation_receipt(self, db, key, body):
        signature = hashlib.sha256(
            json.dumps(body, sort_keys=True).encode()
        ).hexdigest()
        row = (
            db.execute(
                "SELECT signature,result FROM runtime_operation_receipts WHERE id=?",
                (key,),
            ).fetchone()
            if key
            else None
        )
        if row and row["signature"] != signature:
            raise ValueError("This request id has different content")
        return signature, json.loads(row["result"]) if row else None

    def save_receipt(self, db, key, signature, result):
        if key:
            db.execute(
                "INSERT INTO runtime_operation_receipts VALUES (?,?,?)",
                (key, signature, json.dumps(result)),
            )
        return result

    def work_action(self, agent_id, data, key=None, actor=None, epoch=None):
        with self.lock, self.db() as db:
            a = self.checked_actor(db, agent_id, actor)
            if epoch is not None and self.agent(actor, db)["epoch"] != epoch:
                raise ValueError("The caller was stopped")
            signature, previous = self.operation_receipt(
                db, key, {"agent": agent_id, "actor": actor, "body": data}
            )
            if previous is not None:
                return previous
            action = data.get("action", "list")
            works = [w for w in self.records(db, "work") if w["rootId"] == a["rootId"]]
            if action == "list":
                return {
                    "items": [self.work_view(w, works) for w in works],
                    "tasks": [self.work_view(w, works) for w in works],
                }
            if action == "create":
                if actor and a["id"] != a["rootId"]:
                    raise ValueError("Only the lead can create work")
                w = {
                    "id": str(uuid.uuid4()),
                    "rootId": a["rootId"],
                    "title": text_field(data.get("title"), "a title", 160),
                    "description": text_field(
                        data.get("description", ""), "a description", empty=True
                    ),
                    "owner": None,
                    "dependencies": [],
                    "status": "ready",
                    "created": time.time(),
                    "updated": time.time(),
                    "version": 0,
                    "results": [],
                    "decisions": [],
                }
            else:
                w = next((w for w in works if w["id"] == data.get("task_id")), None)
                if not w:
                    raise ValueError("Unknown work item")
                if data.get("version") is not None and data["version"] != w["version"]:
                    raise ValueError("This work item changed. Reload before editing")
            leader = not actor or self.agent(actor, db)["id"] == a["rootId"]
            if action in {"create", "update"}:
                if not leader:
                    raise ValueError("Only the lead can change work assignments")
                if w["status"] == "accepted":
                    raise ValueError("Cannot edit accepted work. Create a follow-up task with this task ID and the new evidence")
                if w["status"] == "review":
                    raise ValueError("Work under review cannot be reassigned; reject its result before editing")
                if "title" in data:
                    w["title"] = text_field(data["title"], "a title", 160)
                if "description" in data:
                    w["description"] = text_field(
                        data["description"], "a description", empty=True
                    )
                if "owner" in data:
                    owner = data["owner"] or None
                    if owner and self.checked_actor(db, owner)["rootId"] != a["rootId"]:
                        raise ValueError("Owner belongs to another team")
                    w["owner"] = owner
                if "status" in data:
                    if data["status"] not in {"ready", "blocked"}:
                        raise ValueError("Choose ready or blocked")
                    w["status"] = data["status"]
                if "dependencies" in data:
                    deps = data["dependencies"]
                    if (
                        not isinstance(deps, list)
                        or len(deps) > 100
                        or any(not isinstance(d, str) for d in deps)
                    ):
                        raise ValueError("Supply up to 100 dependency ids")
                    w["dependencies"] = list(dict.fromkeys(deps))
                    graph = {t["id"]: t["dependencies"] for t in works}
                    graph[w["id"]] = w["dependencies"]
                    visited = set()

                    def visit(node, path):
                        if node in path:
                            raise ValueError("Task dependencies contain a cycle")
                        if node in visited:
                            return
                        if node not in graph:
                            raise ValueError("Dependency is outside this team")
                        for child in graph[node]:
                            visit(child, path | {node})
                        visited.add(node)

                    visit(w["id"], set())
            elif action == "claim":
                claimant = actor or data.get("owner") or a["id"]

                if self.checked_actor(db, claimant)["rootId"] != a["rootId"]:
                    raise ValueError("Claimant belongs to another team")
                if (
                    w["status"] not in {"ready", "running"}
                    or self.work_view(w, works)["blockedBy"]
                ):
                    raise ValueError("This work item is not ready")
                if w["owner"] and w["owner"] != claimant:
                    raise ValueError("Another agent owns this work item")
                w.update(owner=claimant, status="running")
            elif action == "submit":
                if w["status"] not in {"running", "ready", "blocked", "review"}:
                    raise ValueError("This result is already accepted")
                if actor and w["owner"] != actor:
                    raise ValueError("Only the assigned worker can submit its result")
                if self.work_view(w, works)["blockedBy"]:
                    raise ValueError("Dependencies have not been accepted")
                files = data.get("files", [])
                if (
                    not isinstance(files, list)
                    or len(files) > 50
                    or any(not isinstance(f, str) or len(f) > 4096 for f in files)
                ):
                    raise ValueError("Supply up to 50 file paths")
                submitter = w.get("owner") or a["id"]
                for path in files:
                    self.workspace_path(submitter, path)
                result = {
                    "id": str(uuid.uuid4()),
                    "agent": submitter,
                    "text": text_field(data.get("result"), "a result"),
                    "checks": text_field(data.get("checks"), "test evidence"),
                    "revision": text_field(
                        data.get("revision"), "a source revision", 200
                    ),
                    "files": files,
                    "created": time.time(),
                }
                w["results"].append(result)
                w["status"] = "review"
                self.enqueue(
                    db,
                    self.agent(a["rootId"], db),
                    "work_review",
                    json.dumps(
                        {"task": w["id"], "title": w["title"], "result": result}
                    ),
                    "work-result:" + result["id"],
                )
            elif action in {"accept", "reject"}:
                if not leader:
                    raise ValueError("Only the lead can accept or reject a result")
                if w["status"] == "accepted":
                    raise ValueError("This work is already accepted. Create a follow-up task with this task ID and the new evidence")
                if w["status"] != "review" or not w["results"]:
                    raise ValueError("Submit a result before review")
                reason = text_field(data.get("result"), "a review decision")
                w["decisions"].append(
                    {
                        "resultId": w["results"][-1]["id"],
                        "decision": action,
                        "reason": reason,
                        "by": actor or "user",
                        "created": time.time(),
                    }
                )
                w["status"] = "accepted" if action == "accept" else "ready"
                if w.get("owner"):
                    self.enqueue(
                        db,
                        self.agent(w["owner"], db),
                        "work_decision",
                        json.dumps(
                            {"task": w["id"], "decision": action, "reason": reason}
                        ),
                        "work-decision:" + w["id"] + ":" + str(w["version"]),
                    )
                if action == "accept":
                    for other in works:
                        if w["id"] in other["dependencies"] and other.get("owner"):
                            updated = [w if v["id"] == w["id"] else v for v in works]
                            if not self.work_view(other, updated)["blockedBy"]:
                                self.enqueue(
                                    db,
                                    self.agent(other["owner"], db),
                                    "work_ready",
                                    json.dumps(
                                        {"task": other["id"], "title": other["title"]}
                                    ),
                                    "work-ready:" + other["id"] + ":" + w["id"],
                                )
            else:
                raise ValueError("Unknown work action")
            w.update(version=w["version"] + 1, updated=time.time())
            self.put(db, "work", w)
            return self.save_receipt(
                db,
                key,
                signature,
                self.work_view(w, [t for t in works if t["id"] != w["id"]] + [w]),
            )

    @staticmethod
    def work_view(w, works):
        statuses = {t["id"]: t["status"] for t in works}
        blocked = [d for d in w["dependencies"] if statuses.get(d) != "accepted"]
        return {
            **w,
            "blockedBy": blocked,
            "displayStatus": (
                "blocked"
                if blocked and w["status"] in {"ready", "running"}
                else w["status"]
            ),
        }

    def search_work(self, query, agent_id=None, limit=50):
        query = text_field(query, "a search query", 500)
        limit = max(1, min(100, int(limit)))
        # Quote each word; user input never becomes FTS operators or SQL.
        match = " AND ".join(
            '"' + word.replace('"', '""') + '"' for word in query.split()
        )
        with self.lock, self.db() as db:
            caller = self.checked_actor(db, agent_id) if agent_id else None
            allowed = {
                a["id"]
                for a in self.records(db, "agents")
                if not a.get("deletedAt")
                and (not caller or a["rootId"] == caller["rootId"])
            }
            found = []
            for row in db.execute(
                "SELECT runtime_search.id,runtime_search.agent,runtime_search.kind,snippet(runtime_search,3,'','',' … ',30) AS excerpt FROM runtime_search JOIN runtime_items i ON i.id=runtime_search.id WHERE runtime_search MATCH ? AND json_extract(i.record,'$.afterRestore') IS NULL ORDER BY rank LIMIT 1000",
                (match,),
            ):
                if row["agent"] in allowed and (
                    not caller or row["agent"] == caller["id"]
                ):
                    found.append(
                        {**dict(row), "type": "message", "reference": row["id"]}
                    )
                if len(found) >= limit:
                    break
            needle = query.casefold()
            for table, kind, field in [
                ("work", "work", "title"),
                ("complaints", "complaint", "title"),
                ("plans", "plan", "text"),
            ]:
                for row in self.records(db, table):
                    root = row.get("rootId") or row.get("leadId") or row.get("id")
                    if caller and root != caller["rootId"]:
                        continue
                    if root not in allowed:
                        continue
                    body = json.dumps(row, ensure_ascii=False)
                    if needle in body.casefold():
                        found.append(
                            {
                                "id": row["id"],
                                "agent": root,
                                "type": kind,
                                "kind": kind,
                                "excerpt": str(row.get(field, ""))[:500],
                                "reference": row["id"],
                            }
                        )
            readable = {
                r["id"]
                for r in self.chat_rooms(db)
                if not caller or caller["id"] in r["members"]
            }
            for row in db.execute(
                "SELECT id,room,sender,text FROM runtime_chat_messages WHERE instr(lower(text),lower(?))>0 ORDER BY seq DESC LIMIT 1000",
                (query,),
            ):
                if row["room"] in readable:
                    found.append(
                        {
                            "id": row["id"],
                            "agent": row["sender"],
                            "room": row["room"],
                            "type": "room",
                            "kind": "Agent chat",
                            "excerpt": row["text"][:500],
                            "reference": row["id"],
                        }
                    )
            return {
                "results": [{**r, "text": r.get("excerpt", "")} for r in found[:limit]],
                "query": query,
                "limit": limit,
            }

    def chat_organization(self, key, data):
        with self.lock, self.db() as db:
            a = self.checked_actor(db, key)
            if 'read_state' in data:
                from codex_chat_read_state import read_state
                return read_state(self, db, a, data)
            if 'project_folder' in data:
                from codex_project_folders import folder_for
                if not a.get('isLead') or data.get('project_path') != a.get('cwd'):
                    raise ValueError('Select a chat in this project')
                desired = folder_for(self, db, a['cwd'], data['project_folder'])
                revision = data.get('expected_revision')
                current = a.get('projectFolderRevision', 0)
                if type(revision) is not int or revision < 0:
                    raise ValueError('Supply the current chat folder revision')
                replay = a.get('projectFolder') == desired and revision in (current, current - 1)
                if not replay and (revision != current or data.get('expected_folder') != a.get('projectFolder')):
                    raise ValueError('The chat folder changed. Reload it before moving')
                if a.get('projectFolder') != desired:
                    a['projectFolderRevision'] = current + 1
                a['projectFolder'] = desired
            for field in ("pinned", "archived"):
                if field in data:
                    if not isinstance(data[field], bool):
                        raise ValueError("Supply a boolean for " + field)
                    if (
                        field == "archived"
                        and data[field]
                        and (
                            a.get("inFlight")
                            or any(
                                m["agent"] == key
                                and m["status"] in {"running", "approval", "starting"}
                                for m in self.records(db, "monitors")
                            )
                        )
                    ):
                        raise ValueError(
                            "Stop active work before archiving this conversation"
                        )
                    a[field] = data[field]
            if "project" in data:
                a["project"] = text_field(
                    data["project"], "a project name", 100, empty=True
                )
            self.put(db, "agents", a)
            return a

    def plan_action(self, key, data=None):
        with self.lock, self.db() as db:
            a = self.checked_actor(db, key)
            row = db.execute(
                "SELECT record FROM runtime_plans WHERE id=?", (key,)
            ).fetchone()
            plan = (
                json.loads(row[0])
                if row
                else {
                    "id": key,
                    "rootId": a["rootId"],
                    "text": "",
                    "version": 0,
                    "updated": None,
                    "steps": [],
                }
            )
            if data is None:
                return plan
            if data.get("version") != plan["version"]:
                raise ValueError("The plan changed. Reload before saving")
            plan.update(
                text=text_field(data.get("text"), "a plan", 64000, empty=True),
                version=plan["version"] + 1,
                updated=time.time(),
            )
            self.put(db, "plans", plan)
            self.enqueue(
                db,
                a,
                "plan_update",
                "The user updated the shared plan:\n" + plan["text"],
                "plan:" + key + ":" + str(plan["version"]),
            )
            return plan

    def queue_action(self, agent_id, data=None):
        import math

        with self.lock, self.db() as db:
            a = self.checked_actor(db, agent_id)

            def snapshot():
                rows = []
                for stored in db.execute(
                    "SELECT e.id,e.text,e.kind,e.status,e.created,m.record AS metadata "
                    "FROM runtime_events e LEFT JOIN runtime_event_meta m ON m.id=e.id "
                    "WHERE e.agent=? AND e.epoch=? AND e.status='pending' "
                    "AND e.kind IN ('user','followup') ORDER BY e.created,e.rowid",
                    (agent_id, a["epoch"]),
                ):
                    row = dict(stored)
                    metadata = json.loads(row.pop("metadata") or "{}")
                    row.update(
                        assets=[self.asset_view(self.asset_record(v)) for v in metadata.get("assets", [])],
                        delivery=metadata.get("delivery", "queue"),
                        requestedDelivery=metadata.get("requestedDelivery", metadata.get("delivery", "queue")),
                    )
                    if "acceptedAt" in metadata:
                        row["acceptedAt"] = metadata["acceptedAt"]
                    rows.append(row)
                revision = hashlib.sha256(json.dumps(
                    [a["epoch"], a.get("queueMutationRevision", 0), rows], sort_keys=True,
                ).encode()).hexdigest()
                return {"items": rows, "revision": revision,
                        "capabilities": {"reorder": True, "receipts": True}}

            if data is None:
                return snapshot()
            action = data.get("action")
            request_id = data.get("request_id")
            if request_id is not None:
                request_id = text_field(request_id, "a request ID", 200)
            if action == "reorder" and not request_id:
                raise ValueError("Supply a request ID to reorder the queue")
            receipt_key = "queue:" + agent_id + ":" + request_id if request_id else None
            signature, previous = self.operation_receipt(
                db, receipt_key, {"agent": agent_id, "body": data},
            )
            if previous is not None:
                return previous
            current = snapshot()
            if (("expected_revision" in data or action == "reorder")
                    and data.get("expected_revision") != current["revision"]):
                raise ValueError("This queue changed. Reload before editing or reordering")
            rows = current["items"]
            if action == "reorder":
                ordered = data.get("ordered_ids")
                if (not isinstance(ordered, list) or any(not isinstance(v, str) for v in ordered)
                        or len(ordered) != len(rows) or len(set(ordered)) != len(ordered)
                        or set(ordered) != {r["id"] for r in rows}):
                    raise ValueError("Supply every current queued message ID exactly once")
                # Keep system events in their slots. Unique queue timestamps also
                # make dispatch deterministic when old messages have equal clocks.
                pending = db.execute(
                    "SELECT id,kind,created FROM runtime_events WHERE agent=? AND epoch=? "
                    "AND status='pending' ORDER BY created,rowid", (agent_id, a["epoch"]),
                ).fetchall()
                slots = []
                run = []
                lower = -math.inf
                for event in [*pending, {"kind": None, "created": math.inf}]:
                    if event["kind"] in {"user", "followup"}:
                        run.append(event["created"])
                        continue
                    upper = event["created"]
                    values = []
                    last = lower
                    for original in run:
                        last = max(original, math.nextafter(last, math.inf))
                        values.append(last)
                    if values and values[-1] >= upper:
                        values = []
                        last = upper
                        for original in reversed(run):
                            last = min(original, math.nextafter(last, -math.inf))
                            values.append(last)
                        values.reverse()
                    if values and (values[0] <= lower or values[-1] >= upper
                                   or any(not math.isfinite(v) for v in values)):
                        raise ValueError("Queue timestamps overlap. The queue cannot be reordered safely")
                    slots.extend(values)
                    run = []
                    lower = upper
                for message_id, created in zip(ordered, slots):
                    db.execute("UPDATE runtime_events SET created=? WHERE id=?", (created, message_id))
            else:
                row = next((r for r in rows
                            if r["id"] == (data.get("message_id") or data.get("id"))), None)
                if not row:
                    raise ValueError("This message already left the queue")
                if data.get("expectedText") is not None and data["expectedText"] != row["text"]:
                    raise ValueError("This queued message changed")
                if action == "cancel":
                    db.execute("UPDATE runtime_events SET status='cancelled' WHERE id=?", (row["id"],))
                elif action == "edit":
                    text = text_field(data.get("text"), "a message", empty=bool(row["assets"]))
                    db.execute("UPDATE runtime_events SET text=? WHERE id=?", (text, row["id"]))
                    if row["text"] != text:
                        saved = db.execute(
                            "SELECT record FROM runtime_event_meta WHERE id=?", (row["id"],),
                        ).fetchone()
                        metadata = json.loads(saved[0]) if saved else {}
                        metadata["acceptedAt"] = time.time()
                        db.execute("INSERT OR REPLACE INTO runtime_event_meta VALUES (?,?)",
                                   (row["id"], json.dumps(metadata)))
                elif action == "first":
                    first = min(r["created"] for r in rows)
                    db.execute("UPDATE runtime_events SET created=? WHERE id=?", (first - 0.001, row["id"]))
                else:
                    raise ValueError("Choose edit, cancel, first, or reorder")
            a["queueMutationRevision"] = a.get("queueMutationRevision", 0) + 1
            self.put(db, "agents", a)
            updated = snapshot()
            result = self.save_receipt(db, receipt_key, signature, {
                "status": "updated", "revision": updated["revision"],
                "capabilities": updated["capabilities"],
            })
            self.changed.set()
            return result

    def annotate(self, agent_id, data):
        with self.lock, self.db() as db:
            a = self.checked_actor(db, agent_id)
            signature, previous = self.operation_receipt(
                db, data.get("id"), {"agent": agent_id, **data}
            )
            if previous is not None:
                return previous
            path = text_field(data.get("path"), "a file path", 4096)
            self.workspace_path(agent_id, path)
            line = data.get("line")
            if not isinstance(line, int) or line < 1:
                raise ValueError("Supply a positive line number")
            note = {
                "id": data.get("id") or str(uuid.uuid4()),
                "agent": agent_id,
                "rootId": a["rootId"],
                "path": path,
                "line": line,
                "text": text_field(data.get("text"), "a comment"),
                "created": time.time(),
            }
            turn_id = data.get("turnId")
            if turn_id is not None:
                note["turnId"] = text_field(turn_id, "a turn ID", 200)
            self.put(db, "annotations", note)
            self.enqueue(
                db,
                a,
                "user",
                f"Review comment at {path}:{line}"
                + (f" (reported turn {note['turnId']})" if turn_id is not None else "")
                + f"\n{note['text']}",
                "annotation:" + note["id"],
            )
            return self.save_receipt(db, data.get("id"), signature, note)

    def search_item(self, key):
        with self.lock, self.db() as db:
            row = db.execute(
                "SELECT agent,record FROM runtime_items WHERE id=?", (key,)
            ).fetchone()
            if row:
                a = self.checked_actor(db, row["agent"])
                item = json.loads(row["record"])
                if item.get("afterRestore"):
                    raise ValueError("This item belongs to history before restore")
                full = db.execute(
                    "SELECT body FROM runtime_search WHERE id=?", (key,)
                ).fetchone()
                return {
                    **item,
                    "text": full[0] if full else item["text"],
                    "agent": a["id"],
                    "kind": "message",
                }
            row = db.execute(
                "SELECT * FROM runtime_chat_messages WHERE id=?", (key,)
            ).fetchone()
            if row:
                room = next(
                    (r for r in self.chat_rooms(db) if r["id"] == row["room"]), None
                )
                if not room:
                    raise ValueError("This chat is unavailable")
                return {**dict(row), "kind": "room", "agent": row["sender"]}
            for table in ("work", "plans", "complaints"):
                row = db.execute(
                    f"SELECT record FROM runtime_{table} WHERE id=?", (key,)
                ).fetchone()
                if row:
                    item = json.loads(row[0])
                    owner = item.get("rootId") or item.get("leadId") or item["id"]
                    self.checked_actor(db, owner)
                    return {
                        **item,
                        "agent": owner,
                        "kind": {
                            "work": "work",
                            "plans": "plan",
                            "complaints": "complaint",
                        }[table],
                        "text": item.get("text")
                        or json.dumps(item, indent=2, ensure_ascii=False),
                    }
            raise ValueError("This search result is unavailable")
