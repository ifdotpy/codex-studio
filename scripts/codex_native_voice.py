"""Native Codex v3 WebRTC. The account app-server owns voice and delegation."""
import concurrent.futures
import json
import threading
import time


class NativeVoice:
    def init_native(self):
        self.native_lock = threading.RLock()
        self.connections = {}
        with self.runtime.db() as db:
            columns = {r[1] for r in db.execute("PRAGMA table_info(voice_sessions)")}
            for name in ("native_thread", "account_key", "connection_id", "state", "error"):
                if name not in columns:
                    db.execute(f"ALTER TABLE voice_sessions ADD COLUMN {name} TEXT")
            db.execute("""CREATE TABLE IF NOT EXISTS voice_speech_requests (
                id TEXT PRIMARY KEY, session TEXT NOT NULL, record_id TEXT NOT NULL,
                state TEXT NOT NULL, error TEXT)""")
            # A process restart cannot restore a browser's microphone or peer.
            db.execute("UPDATE voice_sessions SET state='lost',ended=?,error=? WHERE state IS NOT NULL AND ended IS NULL",
                       (time.time(), "Voice connection ended. The transcript is saved."))

    def status(self, agent):
        self._agent(agent)
        return {"configured": True, "transport": "native", "auth": "chatgpt"}

    def session(self, agent, session_id):
        self._agent(agent)
        with self.runtime.db() as db:
            row = db.execute("SELECT id,state,answer,error,ended FROM voice_sessions WHERE id=? AND agent=?", (session_id, agent)).fetchone()
        if not row:
            raise ValueError("Unknown voice session")
        return {"session_id": row["id"], "state": row["state"], "sdp": row["answer"], "error": row["error"], "ended": row["ended"]}

    def _state(self, sid, state, error=None, ended=False):
        with self.runtime.db() as db:
            db.execute("UPDATE voice_sessions SET state=?,error=?,ended=CASE WHEN ? THEN COALESCE(ended,?) ELSE ended END WHERE id=?",
                       (state, error, ended, time.time(), sid))

    def start(self, agent, session_id, sdp):
        actor = self._agent(agent)
        if not isinstance(session_id, str) or not session_id or len(session_id) > 120:
            raise ValueError("Invalid voice session identity")
        if not isinstance(sdp, str) or not sdp.startswith("v=0") or len(sdp) > 100000:
            raise ValueError("Invalid WebRTC offer")
        with self.native_lock, self.runtime.db() as db:
            old = db.execute("SELECT * FROM voice_sessions WHERE id=?", (session_id,)).fetchone()
            if old:
                if old["agent"] != agent or (old["sdp"] is not None and old["sdp"] != sdp):
                    raise ValueError("Voice session identity conflicts with the original offer")
            else:
                if db.execute("SELECT 1 FROM voice_sessions WHERE agent=? AND state IS NOT NULL AND ended IS NULL", (agent,)).fetchone():
                    raise ValueError("End the existing voice session first")
                db.execute("INSERT INTO voice_sessions(id,agent,created,sdp,state) VALUES(?,?,?,?,?)",
                           (session_id, agent, time.time(), sdp, "connecting"))
                db.commit()
                self.connections[session_id] = {"agent": agent, "epoch": actor.get("epoch"), "cancel": False, "submitted": False, "stopping": False}
                # Preparation must not occupy the shared coordination executor.
                threading.Thread(target=self._start_native, args=(agent, session_id, sdp), daemon=True, name="native-voice-start").start()
        return self.session(agent, session_id)

    def _start_native(self, agent, sid, sdp):
        try:
            actor = self._agent(agent)
            account = actor.get("accountKey", "default")
            server = self.runtime.connect(account)
            auth = server.call("account/read", {"refreshToken": False}, timeout=10)
            if (auth.get("account") or {}).get("type") != "chatgpt":
                raise ValueError("Sign in to this account with ChatGPT to use voice")
            self.runtime.prepare(actor)
            with self.native_lock:
                context = self.connections[sid]
                if context["cancel"]:
                    return
                actor = self._agent(agent)
                connection = self.runtime.connection_ids.get(account)
                if actor.get("accountKey", "default") != account or self.runtime.connect(account) is not server:
                    raise ValueError("The chat account changed. Start voice again")
                context.update(server=server, account=account, connection=connection, thread=actor["threadId"])
                with self.runtime.db() as db:
                    db.execute("UPDATE voice_sessions SET native_thread=?,account_key=?,connection_id=? WHERE id=?",
                               (actor["threadId"], account, connection, sid))
                # A user starting voice authorizes native handoffs to this lead.
                with self.runtime.lock, self.runtime.db() as db:
                    current = self.runtime.agent(agent, db)
                    if current.get("deletedAt") or current["epoch"] != context["epoch"]:
                        raise ValueError("The orchestrator was stopped while voice connected")
                    current.update(autoWake=True, turnEpoch=current["epoch"])
                    self.runtime.put(db, "agents", current)
                self._submit_start(sid, sdp)
        except Exception as error:
            self._state(sid, "failed", str(error), ended=True)

    def _submit_start(self, sid, sdp, reloaded=False):
        context = self.connections[sid]
        if context["cancel"]:
            return
        context["submitted"] = True
        params = {"threadId": context["thread"], "version": "v3", "outputModality": "audio",
                  "realtimeSessionId": sid, "clientManagedHandoffs": False,
                  "flushTranscriptTailOnSessionEnd": False, "transport": {"type": "webrtc", "sdp": sdp}}

        def completed(future):
            try:
                future.result()  # Acceptance is not an SDP answer or a connected peer.
            except Exception as error:
                if not reloaded and "does not support realtime conversation" in str(error):
                    context["submitted"] = False
                    # This precise native rejection happens before submitting Core's start operation.
                    threading.Thread(target=self._reload_idle, args=(sid, sdp), daemon=True, name="native-voice-enable").start()
                else:
                    self._state(sid, "failed", str(error), ended=True)

        self._submit(context["server"], "thread/realtime/start", params, completed)

    @staticmethod
    def _submit(server, method, params, callback):
        try:
            submitted = server.submit(method, params)
        except Exception as error:
            submitted = getattr(error, "submitted", None)
            if submitted is None:
                raise
        server.on_result(submitted, callback)

    def _reload_idle(self, sid, sdp):
        try:
            with self.native_lock:
                context = self.connections[sid]
                if context["cancel"]:
                    self._state(sid, "ended", ended=True)
                    return
            agent, server, tid = context["agent"], context["server"], context["thread"]
            with self.runtime.lock:
                guard = self.runtime.prepare_locks.setdefault(agent, threading.Lock())
            with guard:
                with self.runtime.lock:
                    actor = self._agent(agent)
                    if actor.get("inFlight") or actor.get("status") in {"running", "starting", "approval"}:
                        raise ValueError("This chat needs a voice update. Start voice after its current turn finishes")
                native = server.call("thread/read", {"threadId": tid}, timeout=5)["thread"]
                jobs = server.call("thread/backgroundTerminals/list", {"threadId": tid}, timeout=5)
                if native.get("status", {}).get("type") != "idle" or jobs.get("data") or jobs.get("nextCursor"):
                    raise ValueError("Start voice after this chat's turn and background commands finish")
                server.call("thread/unsubscribe", {"threadId": tid}, timeout=5)
                with self.runtime.lock:
                    self.runtime.loaded.discard(agent)
                value = self.runtime.prepare_locked(self._agent(agent))
                if isinstance(value, concurrent.futures.Future):
                    value.result(60)
            with self.native_lock:
                if not self.runtime.connection_current(context["account"], context["connection"]):
                    raise ValueError("Codex disconnected during the voice update")
                self._submit_start(sid, sdp, reloaded=True)
        except Exception as error:
            self._state(sid, "failed", str(error), ended=True)

    def _save_transcript(self, sid, role, text, partial=False):
        context = self.connections[sid]
        if role not in {"user", "assistant"} or not isinstance(text, str) or not text:
            return
        sequence = context.get("transcript_sequence", 0) + 1
        self.record(context["agent"], sid, f"native:{sid}:transcript:{sequence}", role, text,
                    payload={"native": True, "partial": partial}, _internal=True)
        context["transcript_sequence"] = sequence

    def _flush_transcript(self, sid):
        context = self.connections[sid]
        with self.runtime.db() as db:
            present = db.execute("SELECT 1 FROM voice_sessions WHERE id=?", (sid,)).fetchone()
        if not present:
            context["transcript_tail"] = {}
            return
        for role, text in list(context.get("transcript_tail", {}).items()):
            self._save_transcript(sid, role, text, partial=True)
        context["transcript_tail"] = {}

    def native_notification(self, message, account, connection):
        method, params = message.get("method", ""), message.get("params", {})
        if not method.startswith("thread/realtime/"):
            return False
        with self.native_lock:
            candidates = [(sid, c) for sid, c in self.connections.items()
                          if c.get("thread") == params.get("threadId") and c.get("account") == account and c.get("connection") == connection]
            if not candidates:
                return True
            # Only one session can own this native thread at a time.
            sid, context = candidates[-1]
            with self.runtime.db() as db:
                if not db.execute("SELECT 1 FROM voice_sessions WHERE id=?", (sid,)).fetchone():
                    return True
            item = params.get("item") or {}
            event_session = item.get("realtimeSessionId") or params.get("realtimeSessionId")
            if event_session and event_session != sid:
                return True
            if method == "thread/realtime/item/started" and item.get("type") == "transcriptSegment":
                context["canonical"] = True
            if method == "thread/realtime/transcript/delta" and not context.get("canonical"):
                tail = context.setdefault("transcript_tail", {})
                role = params.get("role")
                tail[role] = tail.get(role, "") + params.get("delta", "")
            elif method == "thread/realtime/transcript/done" and not context.get("canonical"):
                role, text = params.get("role"), params.get("text", "")
                tail = context.setdefault("transcript_tail", {})
                completed = context.setdefault("transcript_completed", {})
                if tail.get(role) or completed.get(role) != text:
                    self._save_transcript(sid, role, text)
                completed[role] = text
                tail.pop(role, None)
            elif method == "thread/realtime/sdp":
                with self.runtime.db() as db:
                    db.execute("UPDATE voice_sessions SET answer=?,state=CASE WHEN ended IS NULL THEN 'ready' ELSE state END WHERE id=?",
                               (params["sdp"], sid))
                if context["cancel"]:
                    self._stop_native(sid)
            elif method == "thread/realtime/item/completed" and item.get("type") == "transcriptSegment":
                self.record(context["agent"], sid, "native:" + sid + ":" + item["id"],
                            "user" if item.get("role") == "user" else "assistant", item.get("text", ""),
                            item_id=item["id"], payload={"native": True}, _internal=True)
            elif method == "thread/realtime/error":
                self._state(sid, "failed", params.get("message") or "Voice failed")
                self._stop_native(sid)
            elif method == "thread/realtime/closed":
                self._flush_transcript(sid)
                # Preserve a preceding specific error.
                current = self.session(context["agent"], sid)
                if current["state"] == "failed":
                    self._state(sid, "failed", current["error"], ended=True)
                else:
                    reason = params.get("reason")
                    self._state(sid, "ended", "Voice disconnected. The saved transcript remains." if reason and reason != "requested" else None, ended=True)
                context["closed"] = True
        return True

    def disconnected_native(self, account, connection):
        with self.native_lock:
            for sid, context in self.connections.items():
                if context.get("account") == account and context.get("connection") == connection:
                    context["cancel"] = True
                    self._flush_transcript(sid)
                    self._state(sid, "lost", "Codex disconnected. The saved transcript remains.", ended=True)

    def end(self, agent, session_id):
        self._agent(agent)
        with self.native_lock:
            with self.runtime.db() as db:
                if not db.execute("SELECT 1 FROM voice_sessions WHERE id=?", (session_id,)).fetchone():
                    if not isinstance(session_id, str) or not session_id or len(session_id) > 120:
                        raise ValueError("Invalid voice session identity")
                    # Stop can arrive before a delayed HTTP start. This tombstone prevents late capture.
                    db.execute("INSERT INTO voice_sessions(id,agent,created,ended,state) VALUES(?,?,?,?,?)",
                               (session_id, agent, time.time(), time.time(), "ended"))
            current = self.session(agent, session_id)
            context = self.connections.get(session_id)
            if current["ended"] and not context:
                return current
            self._cancel(session_id)
        return self.session(agent, session_id)

    def end_active(self, agent):
        with self.native_lock:
            for sid, context in self.connections.items():
                if context["agent"] == agent and not context.get("closed"):
                    self._cancel(sid)

    def _cancel(self, sid):
        context = self.connections.get(sid)
        if context:
            context["cancel"] = True
        if context and context.get("submitted") and not context.get("closed"):
            current = self.session(context["agent"], sid) if not self.runtime.agent(context["agent"]).get("deletedAt") else None
            if not current or current["state"] != "failed":
                self._state(sid, "stopping")
            self._stop_native(sid)
        else:
            self._state(sid, "ended", ended=True)

    def _stop_native(self, sid):
        context = self.connections[sid]
        if context["stopping"]:
            return
        context["stopping"] = True
        if not self.runtime.connection_current(context["account"], context["connection"]):
            self._state(sid, "lost", "Voice connection ended", ended=True)
            return
        def completed(future):
            try:
                future.result()
                # Core closes voice asynchronously. Do not release ownership on its RPC ack.
            except Exception as error:
                self._state(sid, "unknown", str(error))
        try:
            self._submit(context["server"], "thread/realtime/stop", {"threadId": context["thread"]}, completed)
        except Exception as error:
            self._state(sid, "unknown", str(error))

    def speech(self, agent, record_id, session_id, request_id=None):
        self._agent(agent)
        request_id = request_id or session_id + ":" + record_id
        if not isinstance(request_id, str) or not request_id or len(request_id) > 480:
            raise ValueError("Invalid voice speech identity")
        with self.native_lock, self.runtime.db() as db:
            old = db.execute("SELECT * FROM voice_speech_requests WHERE id=?", (request_id,)).fetchone()
            if old:
                if old["session"] != session_id or old["record_id"] != record_id:
                    raise ValueError("Voice speech identity conflicts with its original content")
                if not db.execute("SELECT 1 FROM voice_sessions WHERE id=? AND agent=?", (session_id, agent)).fetchone():
                    raise ValueError("Unknown voice session")
                return dict(old)
            session = db.execute("SELECT * FROM voice_sessions WHERE id=? AND agent=? AND state='ready' AND ended IS NULL", (session_id, agent)).fetchone()
            context = self.connections.get(session_id)
            if not session or not context or context["cancel"]:
                raise ValueError("Start voice before audio playback")
            if not self.runtime.connection_current(context["account"], context["connection"]) or self._agent(agent).get("accountKey", "default") != context["account"]:
                raise ValueError("The voice account connection changed")
            row = db.execute("SELECT * FROM voice_records WHERE agent=? AND id=? AND kind='orchestrator'", (agent, record_id)).fetchone()
            if not row:
                raise ValueError("Unknown orchestrator speech")
            db.execute("INSERT INTO voice_speech_requests VALUES(?,?,?,'pending',NULL)", (request_id, session_id, record_id))
            db.commit()
            def completed(future):
                state, error = "submitted", None
                try:
                    future.result()
                except Exception as exc:
                    state, error = "failed", str(exc)
                with self.runtime.db() as writer:
                    writer.execute("UPDATE voice_speech_requests SET state=?,error=? WHERE id=?", (state, error, request_id))
            try:
                self._submit(context["server"], "thread/realtime/appendSpeech", {"threadId": context["thread"], "text": row["text"]}, completed)
            except Exception as error:
                db.execute("UPDATE voice_speech_requests SET state='unknown',error=? WHERE id=?", (str(error), request_id))
        return {"id": request_id, "state": "pending", "playback": "not confirmed"}
