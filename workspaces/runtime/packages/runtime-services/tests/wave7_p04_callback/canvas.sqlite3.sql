/* WARNING: Script requires that SQLITE_DBCONFIG_DEFENSIVE be disabled */
PRAGMA foreign_keys=OFF;
BEGIN TRANSACTION;
CREATE TABLE runtime_access_alias_requests (
          id TEXT PRIMARY KEY, server TEXT NOT NULL, alias TEXT NOT NULL);
CREATE TABLE runtime_access_audit (
          seq INTEGER PRIMARY KEY AUTOINCREMENT, client TEXT NOT NULL, actor TEXT NOT NULL,
          action TEXT NOT NULL, created REAL NOT NULL);
CREATE TABLE runtime_access_clients (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_access_invites (
          id TEXT PRIMARY KEY, request_id TEXT NOT NULL UNIQUE, hash TEXT NOT NULL, record TEXT NOT NULL);
CREATE TABLE runtime_access_local_alias (
          id INTEGER PRIMARY KEY CHECK(id=1), alias TEXT NOT NULL);
CREATE TABLE runtime_access_name_requests (
          id TEXT PRIMARY KEY, label TEXT NOT NULL, actor TEXT NOT NULL, applied INTEGER NOT NULL);
CREATE TABLE runtime_access_name_targets (
          id TEXT PRIMARY KEY, server TEXT NOT NULL, label TEXT NOT NULL);
CREATE TABLE runtime_access_nonces (
          client TEXT NOT NULL, nonce TEXT NOT NULL, created INTEGER NOT NULL, PRIMARY KEY(client,nonce));
CREATE TABLE runtime_access_outbound (
          server TEXT NOT NULL, request_id TEXT NOT NULL, hash TEXT NOT NULL,
          record TEXT NOT NULL, receipt_state TEXT GENERATED ALWAYS AS (json_extract(record,'$.outcome')) VIRTUAL NOT NULL CHECK("receipt_state" IN ('unknown','complete','redacted')), PRIMARY KEY(server,request_id));
CREATE TABLE runtime_account_transfers(id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_agent_moves(id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_agent_record_generation (
                    id INTEGER PRIMARY KEY CHECK(id=1), value INTEGER NOT NULL);
INSERT INTO runtime_agent_record_generation VALUES(1,148);
CREATE TABLE runtime_annotations (id TEXT PRIMARY KEY, record TEXT NOT NULL, rev INTEGER NOT NULL DEFAULT 1);
CREATE TABLE runtime_assets (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_budget (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_capacity_retries (id TEXT PRIMARY KEY, agent TEXT NOT NULL, record TEXT NOT NULL);
CREATE TABLE runtime_chat_messages (
                  seq INTEGER PRIMARY KEY AUTOINCREMENT, id TEXT UNIQUE NOT NULL,
                  room TEXT NOT NULL, sender TEXT NOT NULL, text TEXT NOT NULL,
                  created REAL NOT NULL, deliveries TEXT NOT NULL, rev INTEGER NOT NULL DEFAULT 1);
CREATE TABLE runtime_checkpoints (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_compactions (id TEXT PRIMARY KEY, agent TEXT NOT NULL);
CREATE TABLE runtime_complaints (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_completed_turns (id TEXT PRIMARY KEY);
CREATE TABLE runtime_context_manifests (
                  agent TEXT NOT NULL, thread_id TEXT NOT NULL, compactions INTEGER NOT NULL,
                  sequence INTEGER NOT NULL, event_id TEXT NOT NULL, record TEXT NOT NULL,
                  PRIMARY KEY(agent,thread_id,compactions));
CREATE TABLE runtime_context_reminders (
                  agent TEXT NOT NULL, version TEXT NOT NULL, event_id TEXT NOT NULL,
                  PRIMARY KEY(agent,version));
CREATE TABLE runtime_event_meta (id TEXT PRIMARY KEY, record TEXT NOT NULL);
INSERT INTO runtime_event_meta VALUES('monitor:6a1cbd51-e035-54ce-af5c-7fd9850e3065','{"delivery":"current_turn","remoteRequestId":null}');
CREATE TABLE runtime_events (
                  id TEXT PRIMARY KEY, agent TEXT NOT NULL, kind TEXT NOT NULL,
                  text TEXT NOT NULL, status TEXT NOT NULL, created REAL NOT NULL,
                  epoch INTEGER NOT NULL, turn_id TEXT, error TEXT, receipt_enum_guard INTEGER GENERATED ALWAYS AS (NULL) VIRTUAL CHECK("kind" IN ('user','followup','agent_message','child_result','child_stopped','complaint','complaint_response','chat_review','work_review','work_decision','work_ready','monitor_exit','monitor_stall','rule','rule_missed','rule_stall','radio_turn','browser_declined','browser_joined','browser_outcome','browser_unanswered','event_wake','monitor_cancelled','monitor_output','plan_update','shell_policy','work_released','work_result_invalid','context_repair_wait','monitor_correction','rule_hold')) CHECK("status" IN ('stored_only','pending','reserved','dispatching','delivered','uncertain','cancelled','failed')));
INSERT INTO runtime_events VALUES('monitor:6a1cbd51-e035-54ce-af5c-7fd9850e3065','abf56fbb-f8dc-4241-ad30-3960b0afa460','monitor_exit','{"id": "6a1cbd51-e035-54ce-af5c-7fd9850e3065", "command": "printf fixture", "status": "lost", "exitCode": null, "error": "Server restarted. Command outcome unknown; not rerun.", "tail": "", "log": "__CALLBACK_STATE__/monitor-logs/6a1cbd51-e035-54ce-af5c-7fd9850e3065.log", "bytes": 0}','pending',1792684447.798625,3,NULL,NULL);
CREATE TABLE runtime_execution_attempts (id TEXT PRIMARY KEY, run TEXT NOT NULL, record TEXT NOT NULL, submission TEXT GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN record->>'$.submission' END) VIRTUAL CHECK("submission" IN ('unsent','pending','accepted','observed','rejected','unknown')), turn_status TEXT GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN record->>'$.turnStatus' END) VIRTUAL CHECK("turn_status" IN ('unsent','pending','accepted','observed','running','unknown','rejected','completed','failed','interrupted','cancelled')));
CREATE TABLE runtime_execution_effects (id TEXT PRIMARY KEY, run TEXT NOT NULL, kind TEXT NOT NULL, reference TEXT NOT NULL, record TEXT NOT NULL, status TEXT GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN record->>'$.status' END) VIRTUAL CHECK("status" IN ('unknown','idle','queued','starting','running','waiting','waiting_on_monitor','approval','completed','interrupted','failed','blocked','paused','parked','abandoned','capacity-retry','pending','cancelled','answering','answered','uncertain','resolved','lost','expired','deleted')), receipt_enum_guard INTEGER GENERATED ALWAYS AS (NULL) VIRTUAL CHECK("kind" IN ('spawn','tool_requests','requests','monitors','task_submit','unrecognized')));
INSERT INTO runtime_execution_effects VALUES('requests:answer-0','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','answer-0','{"id": "requests:answer-0", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "answer-0", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:answer-1','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','answer-1','{"id": "requests:answer-1", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "answer-1", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:answer-2','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','answer-2','{"id": "requests:answer-2", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "answer-2", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:answer-3','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','answer-3','{"id": "requests:answer-3", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "answer-3", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:answer-4','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','answer-4','{"id": "requests:answer-4", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "answer-4", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:answer-5','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','answer-5','{"id": "requests:answer-5", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "answer-5", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:answer-6','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','answer-6','{"id": "requests:answer-6", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "answer-6", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:answer-7','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','answer-7','{"id": "requests:answer-7", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "answer-7", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:answer-8','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','answer-8','{"id": "requests:answer-8", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "answer-8", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:answer-9','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','answer-9','{"id": "requests:answer-9", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "answer-9", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:answer-10','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','answer-10','{"id": "requests:answer-10", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "answer-10", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:answer-11','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','answer-11','{"id": "requests:answer-11", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "answer-11", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:answer-12','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','answer-12','{"id": "requests:answer-12", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "answer-12", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:answer-13','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','answer-13','{"id": "requests:answer-13", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "answer-13", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:answer-14','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','answer-14','{"id": "requests:answer-14", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "answer-14", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:answer-15','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','answer-15','{"id": "requests:answer-15", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "answer-15", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:answer-16','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','answer-16','{"id": "requests:answer-16", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "answer-16", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:answer-17','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','answer-17','{"id": "requests:answer-17", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "answer-17", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:answer-18','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','answer-18','{"id": "requests:answer-18", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "answer-18", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:answer-19','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','answer-19','{"id": "requests:answer-19", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "answer-19", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:answer-20','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','answer-20','{"id": "requests:answer-20", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "answer-20", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:answer-21','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','answer-21','{"id": "requests:answer-21", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "answer-21", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:answer-22','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','answer-22','{"id": "requests:answer-22", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "answer-22", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:answer-23','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','answer-23','{"id": "requests:answer-23", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "answer-23", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:answer-24','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','answer-24','{"id": "requests:answer-24", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "answer-24", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:answer-25','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','answer-25','{"id": "requests:answer-25", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "answer-25", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:answer-26','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','answer-26','{"id": "requests:answer-26", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "answer-26", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:answer-27','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','answer-27','{"id": "requests:answer-27", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "answer-27", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:answer-28','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','answer-28','{"id": "requests:answer-28", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "answer-28", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:answer-29','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','answer-29','{"id": "requests:answer-29", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "answer-29", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('tool_requests:thread-0:alias-0','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','tool_requests','thread-0:alias-0','{"id": "tool_requests:thread-0:alias-0", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "tool_requests", "referenceId": "thread-0:alias-0", "attemptId": null, "requestId": "alias-0", "status": "interrupted", "outcome": "unknown", "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('tool_requests:thread-0:spawn:request-1','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','tool_requests','thread-0:spawn:request-1','{"id": "tool_requests:thread-0:spawn:request-1", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "tool_requests", "referenceId": "thread-0:spawn:request-1", "attemptId": null, "requestId": "request-1", "status": "cancelled", "outcome": "not_applied", "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('tool_requests:thread-0:send:request-2','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','tool_requests','thread-0:send:request-2','{"id": "tool_requests:thread-0:send:request-2", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "tool_requests", "referenceId": "thread-0:send:request-2", "attemptId": null, "requestId": "request-2", "status": "cancelled", "outcome": "not_applied", "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('tool_requests:thread-0:review:request-3','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','tool_requests','thread-0:review:request-3','{"id": "tool_requests:thread-0:review:request-3", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "tool_requests", "referenceId": "thread-0:review:request-3", "attemptId": null, "requestId": "request-3", "status": "cancelled", "outcome": "not_applied", "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('tool_requests:thread-0:server:request-4','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','tool_requests','thread-0:server:request-4','{"id": "tool_requests:thread-0:server:request-4", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "tool_requests", "referenceId": "thread-0:server:request-4", "attemptId": null, "requestId": "request-4", "status": "cancelled", "outcome": "not_applied", "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('tool_requests:thread-0:move:request-5','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','tool_requests','thread-0:move:request-5','{"id": "tool_requests:thread-0:move:request-5", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "tool_requests", "referenceId": "thread-0:move:request-5", "attemptId": null, "requestId": "request-5", "status": "cancelled", "outcome": "not_applied", "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('tool_requests:thread-0:move:request-6','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','tool_requests','thread-0:move:request-6','{"id": "tool_requests:thread-0:move:request-6", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "tool_requests", "referenceId": "thread-0:move:request-6", "attemptId": null, "requestId": "request-6", "status": "cancelled", "outcome": "not_applied", "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('tool_requests:thread-0:warmup','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','tool_requests','thread-0:warmup','{"id": "tool_requests:thread-0:warmup", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "tool_requests", "referenceId": "thread-0:warmup", "attemptId": null, "requestId": "warmup", "status": "completed", "outcome": "applied", "resultReference": "thread-0:warmup"}');
INSERT INTO runtime_execution_effects VALUES('tool_requests:thread-0:dynamic-0','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','tool_requests','thread-0:dynamic-0','{"id": "tool_requests:thread-0:dynamic-0", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "tool_requests", "referenceId": "thread-0:dynamic-0", "attemptId": null, "requestId": "dynamic-0", "status": "completed", "outcome": "applied", "resultReference": "thread-0:dynamic-0"}');
INSERT INTO runtime_execution_effects VALUES('tool_requests:thread-0:dynamic-1','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','tool_requests','thread-0:dynamic-1','{"id": "tool_requests:thread-0:dynamic-1", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "tool_requests", "referenceId": "thread-0:dynamic-1", "attemptId": null, "requestId": "dynamic-1", "status": "completed", "outcome": "applied", "resultReference": "thread-0:dynamic-1"}');
INSERT INTO runtime_execution_effects VALUES('tool_requests:thread-0:dynamic-2','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','tool_requests','thread-0:dynamic-2','{"id": "tool_requests:thread-0:dynamic-2", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "tool_requests", "referenceId": "thread-0:dynamic-2", "attemptId": null, "requestId": "dynamic-2", "status": "completed", "outcome": "applied", "resultReference": "thread-0:dynamic-2"}');
INSERT INTO runtime_execution_effects VALUES('spawn:aaaaaaaa-1111-4111-8111-111111111111','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','spawn','aaaaaaaa-1111-4111-8111-111111111111','{"id": "spawn:aaaaaaaa-1111-4111-8111-111111111111", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "spawn", "referenceId": "aaaaaaaa-1111-4111-8111-111111111111", "attemptId": null, "status": "running"}');
INSERT INTO runtime_execution_effects VALUES('spawn:aaaaaaaa-2222-4222-8222-222222222222','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','spawn','aaaaaaaa-2222-4222-8222-222222222222','{"id": "spawn:aaaaaaaa-2222-4222-8222-222222222222", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "spawn", "referenceId": "aaaaaaaa-2222-4222-8222-222222222222", "attemptId": null, "status": "running"}');
INSERT INTO runtime_execution_effects VALUES('spawn:bbbbbbbb-2222-4222-8222-222222222222','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','spawn','bbbbbbbb-2222-4222-8222-222222222222','{"id": "spawn:bbbbbbbb-2222-4222-8222-222222222222", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "spawn", "referenceId": "bbbbbbbb-2222-4222-8222-222222222222", "attemptId": null, "status": "running"}');
INSERT INTO runtime_execution_effects VALUES('spawn:cccccccc-1111-4111-8111-111111111111','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','spawn','cccccccc-1111-4111-8111-111111111111','{"id": "spawn:cccccccc-1111-4111-8111-111111111111", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "spawn", "referenceId": "cccccccc-1111-4111-8111-111111111111", "attemptId": null, "status": "running"}');
INSERT INTO runtime_execution_effects VALUES('requests:8b359a1f-85cd-4d54-adfd-733c9c049f3c','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','8b359a1f-85cd-4d54-adfd-733c9c049f3c','{"id": "requests:8b359a1f-85cd-4d54-adfd-733c9c049f3c", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "8b359a1f-85cd-4d54-adfd-733c9c049f3c", "attemptId": null, "requestId": null, "status": "pending", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:resolve-card','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','resolve-card','{"id": "requests:resolve-card", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "resolve-card", "attemptId": null, "requestId": null, "status": "resolved", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:python-written-card','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','python-written-card','{"id": "requests:python-written-card", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "python-written-card", "attemptId": null, "requestId": null, "status": "pending", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('monitors:6a1cbd51-e035-54ce-af5c-7fd9850e3065','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','monitors','6a1cbd51-e035-54ce-af5c-7fd9850e3065','{"id": "monitors:6a1cbd51-e035-54ce-af5c-7fd9850e3065", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "monitors", "referenceId": "6a1cbd51-e035-54ce-af5c-7fd9850e3065", "attemptId": null, "requestId": "managed-monitor", "status": "lost", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:roundtrip-0','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','roundtrip-0','{"id": "requests:roundtrip-0", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "roundtrip-0", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:roundtrip-1','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','roundtrip-1','{"id": "requests:roundtrip-1", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "roundtrip-1", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:roundtrip-2','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','roundtrip-2','{"id": "requests:roundtrip-2", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "roundtrip-2", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:roundtrip-3','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','roundtrip-3','{"id": "requests:roundtrip-3", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "roundtrip-3", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:roundtrip-4','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','roundtrip-4','{"id": "requests:roundtrip-4", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "roundtrip-4", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:roundtrip-5','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','roundtrip-5','{"id": "requests:roundtrip-5", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "roundtrip-5", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:roundtrip-6','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','roundtrip-6','{"id": "requests:roundtrip-6", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "roundtrip-6", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:roundtrip-7','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','roundtrip-7','{"id": "requests:roundtrip-7", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "roundtrip-7", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:roundtrip-8','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','roundtrip-8','{"id": "requests:roundtrip-8", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "roundtrip-8", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:roundtrip-9','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','roundtrip-9','{"id": "requests:roundtrip-9", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "roundtrip-9", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:roundtrip-10','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','roundtrip-10','{"id": "requests:roundtrip-10", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "roundtrip-10", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:roundtrip-11','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','roundtrip-11','{"id": "requests:roundtrip-11", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "roundtrip-11", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:roundtrip-12','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','roundtrip-12','{"id": "requests:roundtrip-12", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "roundtrip-12", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:roundtrip-13','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','roundtrip-13','{"id": "requests:roundtrip-13", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "roundtrip-13", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:roundtrip-14','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','roundtrip-14','{"id": "requests:roundtrip-14", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "roundtrip-14", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:roundtrip-15','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','roundtrip-15','{"id": "requests:roundtrip-15", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "roundtrip-15", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:roundtrip-16','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','roundtrip-16','{"id": "requests:roundtrip-16", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "roundtrip-16", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:roundtrip-17','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','roundtrip-17','{"id": "requests:roundtrip-17", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "roundtrip-17", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:roundtrip-18','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','roundtrip-18','{"id": "requests:roundtrip-18", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "roundtrip-18", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:roundtrip-19','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','roundtrip-19','{"id": "requests:roundtrip-19", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "roundtrip-19", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:roundtrip-20','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','roundtrip-20','{"id": "requests:roundtrip-20", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "roundtrip-20", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:roundtrip-21','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','roundtrip-21','{"id": "requests:roundtrip-21", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "roundtrip-21", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:roundtrip-22','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','roundtrip-22','{"id": "requests:roundtrip-22", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "roundtrip-22", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:roundtrip-23','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','roundtrip-23','{"id": "requests:roundtrip-23", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "roundtrip-23", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:roundtrip-24','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','roundtrip-24','{"id": "requests:roundtrip-24", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "roundtrip-24", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:roundtrip-25','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','roundtrip-25','{"id": "requests:roundtrip-25", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "roundtrip-25", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:roundtrip-26','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','roundtrip-26','{"id": "requests:roundtrip-26", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "roundtrip-26", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:roundtrip-27','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','roundtrip-27','{"id": "requests:roundtrip-27", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "roundtrip-27", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:roundtrip-28','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','roundtrip-28','{"id": "requests:roundtrip-28", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "roundtrip-28", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:roundtrip-29','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','roundtrip-29','{"id": "requests:roundtrip-29", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "roundtrip-29", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:delete-0','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','delete-0','{"id": "requests:delete-0", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "delete-0", "attemptId": null, "requestId": null, "status": "deleted", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:delete-1','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','delete-1','{"id": "requests:delete-1", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "delete-1", "attemptId": null, "requestId": null, "status": "deleted", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:delete-2','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','delete-2','{"id": "requests:delete-2", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "delete-2", "attemptId": null, "requestId": null, "status": "deleted", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:delete-3','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','delete-3','{"id": "requests:delete-3", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "delete-3", "attemptId": null, "requestId": null, "status": "deleted", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:delete-4','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','delete-4','{"id": "requests:delete-4", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "delete-4", "attemptId": null, "requestId": null, "status": "deleted", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:delete-5','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','delete-5','{"id": "requests:delete-5", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "delete-5", "attemptId": null, "requestId": null, "status": "answering", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:delete-6','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','delete-6','{"id": "requests:delete-6", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "delete-6", "attemptId": null, "requestId": null, "status": "uncertain", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:delete-7','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','delete-7','{"id": "requests:delete-7", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "delete-7", "attemptId": null, "requestId": null, "status": "deleted", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:delete-8','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','delete-8','{"id": "requests:delete-8", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "delete-8", "attemptId": null, "requestId": null, "status": "deleted", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:delete-9','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','delete-9','{"id": "requests:delete-9", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "delete-9", "attemptId": null, "requestId": null, "status": "deleted", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:delete-10','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','delete-10','{"id": "requests:delete-10", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "delete-10", "attemptId": null, "requestId": null, "status": "deleted", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:delete-11','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','delete-11','{"id": "requests:delete-11", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "delete-11", "attemptId": null, "requestId": null, "status": "deleted", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:delete-12','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','delete-12','{"id": "requests:delete-12", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "delete-12", "attemptId": null, "requestId": null, "status": "answering", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:delete-13','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','delete-13','{"id": "requests:delete-13", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "delete-13", "attemptId": null, "requestId": null, "status": "uncertain", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:delete-14','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','delete-14','{"id": "requests:delete-14", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "delete-14", "attemptId": null, "requestId": null, "status": "deleted", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:delete-15','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','delete-15','{"id": "requests:delete-15", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "delete-15", "attemptId": null, "requestId": null, "status": "deleted", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:delete-16','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','delete-16','{"id": "requests:delete-16", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "delete-16", "attemptId": null, "requestId": null, "status": "deleted", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:delete-17','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','delete-17','{"id": "requests:delete-17", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "delete-17", "attemptId": null, "requestId": null, "status": "deleted", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:delete-18','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','delete-18','{"id": "requests:delete-18", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "delete-18", "attemptId": null, "requestId": null, "status": "deleted", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:delete-19','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','delete-19','{"id": "requests:delete-19", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "delete-19", "attemptId": null, "requestId": null, "status": "answering", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:delete-20','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','delete-20','{"id": "requests:delete-20", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "delete-20", "attemptId": null, "requestId": null, "status": "uncertain", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:failure-answer-notSent','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','failure-answer-notSent','{"id": "requests:failure-answer-notSent", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "failure-answer-notSent", "attemptId": null, "requestId": null, "status": "pending", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:failure-answer-unknown','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','failure-answer-unknown','{"id": "requests:failure-answer-unknown", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "failure-answer-unknown", "attemptId": null, "requestId": null, "status": "uncertain", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:failure-delete-notSent','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','failure-delete-notSent','{"id": "requests:failure-delete-notSent", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "failure-delete-notSent", "attemptId": null, "requestId": null, "status": "pending", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:failure-delete-unknown','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','failure-delete-unknown','{"id": "requests:failure-delete-unknown", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "failure-delete-unknown", "attemptId": null, "requestId": null, "status": "uncertain", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:invalid-0','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','invalid-0','{"id": "requests:invalid-0", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "invalid-0", "attemptId": null, "requestId": null, "status": "pending", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:invalid-1','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','invalid-1','{"id": "requests:invalid-1", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "invalid-1", "attemptId": null, "requestId": null, "status": "pending", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:invalid-2','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','invalid-2','{"id": "requests:invalid-2", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "invalid-2", "attemptId": null, "requestId": null, "status": "pending", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:invalid-3','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','invalid-3','{"id": "requests:invalid-3", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "invalid-3", "attemptId": null, "requestId": null, "status": "pending", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:invalid-4','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','invalid-4','{"id": "requests:invalid-4", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "invalid-4", "attemptId": null, "requestId": null, "status": "pending", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:invalid-5','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','invalid-5','{"id": "requests:invalid-5", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "invalid-5", "attemptId": null, "requestId": null, "status": "pending", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:invalid-6','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','invalid-6','{"id": "requests:invalid-6", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "invalid-6", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:invalid-7','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','invalid-7','{"id": "requests:invalid-7", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "invalid-7", "attemptId": null, "requestId": null, "status": "pending", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:invalid-8','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','invalid-8','{"id": "requests:invalid-8", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "invalid-8", "attemptId": null, "requestId": null, "status": "pending", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:invalid-9','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','invalid-9','{"id": "requests:invalid-9", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "invalid-9", "attemptId": null, "requestId": null, "status": "pending", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:invalid-10','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','invalid-10','{"id": "requests:invalid-10", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "invalid-10", "attemptId": null, "requestId": null, "status": "pending", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:invalid-11','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','invalid-11','{"id": "requests:invalid-11", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "invalid-11", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:race-completed','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','race-completed','{"id": "requests:race-completed", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "race-completed", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:race-failed','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','race-failed','{"id": "requests:race-failed", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "race-failed", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:race-resolved','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','race-resolved','{"id": "requests:race-resolved", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "race-resolved", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
CREATE TABLE runtime_execution_inputs (run TEXT NOT NULL, event TEXT NOT NULL, PRIMARY KEY(run,event));
CREATE TABLE runtime_execution_nodes (id TEXT PRIMARY KEY, run TEXT NOT NULL, record TEXT NOT NULL, status TEXT GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN record->>'$.status' END) VIRTUAL CHECK("status" IN ('unknown','idle','queued','starting','running','waiting','waiting_on_monitor','approval','completed','interrupted','failed','blocked','paused','parked','abandoned','capacity-retry')));
INSERT INTO runtime_execution_nodes VALUES('worker:aaaaaaaa-1111-4111-8111-111111111111','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','{"id": "worker:aaaaaaaa-1111-4111-8111-111111111111", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "managed_worker", "agentId": "aaaaaaaa-1111-4111-8111-111111111111", "status": "interrupted"}');
INSERT INTO runtime_execution_nodes VALUES('worker:aaaaaaaa-2222-4222-8222-222222222222','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','{"id": "worker:aaaaaaaa-2222-4222-8222-222222222222", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "managed_worker", "agentId": "aaaaaaaa-2222-4222-8222-222222222222", "status": "interrupted"}');
INSERT INTO runtime_execution_nodes VALUES('worker:bbbbbbbb-2222-4222-8222-222222222222','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','{"id": "worker:bbbbbbbb-2222-4222-8222-222222222222", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "managed_worker", "agentId": "bbbbbbbb-2222-4222-8222-222222222222", "status": "interrupted"}');
INSERT INTO runtime_execution_nodes VALUES('worker:cccccccc-1111-4111-8111-111111111111','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','{"id": "worker:cccccccc-1111-4111-8111-111111111111", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "managed_worker", "agentId": "cccccccc-1111-4111-8111-111111111111", "status": "interrupted"}');
CREATE TABLE runtime_execution_runs (id TEXT PRIMARY KEY, agent TEXT NOT NULL, account TEXT NOT NULL, epoch INTEGER NOT NULL, thread TEXT, turn TEXT, created REAL NOT NULL, record TEXT NOT NULL, status TEXT GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN record->>'$.status' END) VIRTUAL CHECK("status" IN ('unsent','pending','accepted','observed','running','unknown','rejected','completed','failed','interrupted','cancelled')));
INSERT INTO runtime_execution_runs VALUES('run:18c363e5-2b4a-57a2-a086-76339fa02f4e','abf56fbb-f8dc-4241-ad30-3960b0afa460','default',3,'thread-0','turn-0',1792684446.800099,'{"id": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "agent": "abf56fbb-f8dc-4241-ad30-3960b0afa460", "accountKey": "default", "epoch": 3, "threadId": "thread-0", "turnId": "turn-0", "created": 1792684446.8000987, "status": "running", "firstAttemptId": null, "rootAttemptId": null, "latestAttemptId": null, "reconciled": true, "restartRecovery": {"epoch": 3, "accountKey": "default", "threadId": "thread-0", "turnId": "turn-0", "autoWake": true, "at": 1792684447.7894883, "stage": "pending", "startAttempt": null}, "disconnectRecovery": {"epoch": 3, "accountKey": "default", "threadId": "thread-0", "turnId": "turn-0", "autoWake": true, "at": 1792684447.7894883, "source": "restart"}}');
INSERT INTO runtime_execution_runs VALUES('run:ca8cf778-ff24-526e-82db-707ddddaf9f8','3e73e736-e287-42b7-963c-6bef9312a291','default',3,'thread-1','turn-1',1792684446.803557,'{"id": "run:ca8cf778-ff24-526e-82db-707ddddaf9f8", "agent": "3e73e736-e287-42b7-963c-6bef9312a291", "accountKey": "default", "epoch": 3, "threadId": "thread-1", "turnId": "turn-1", "created": 1792684446.8035574, "status": "running", "firstAttemptId": null, "rootAttemptId": null, "latestAttemptId": null, "reconciled": true, "restartRecovery": {"epoch": 3, "accountKey": "default", "threadId": "thread-1", "turnId": "turn-1", "autoWake": true, "at": 1792684447.7954564, "stage": "pending", "startAttempt": null}, "disconnectRecovery": {"epoch": 3, "accountKey": "default", "threadId": "thread-1", "turnId": "turn-1", "autoWake": true, "at": 1792684447.7954564, "source": "restart"}}');
INSERT INTO runtime_execution_runs VALUES('run:b334b0de-1363-5393-906f-3d49e1e9ab8b','7c2692e5-4303-49f4-8ba0-cdc5a6f1db4a','default',3,'thread-2','turn-2',1792684446.807727,'{"id": "run:b334b0de-1363-5393-906f-3d49e1e9ab8b", "agent": "7c2692e5-4303-49f4-8ba0-cdc5a6f1db4a", "accountKey": "default", "epoch": 3, "threadId": "thread-2", "turnId": "turn-2", "created": 1792684446.8077269, "status": "running", "firstAttemptId": null, "rootAttemptId": null, "latestAttemptId": null, "reconciled": true, "restartRecovery": {"epoch": 3, "accountKey": "default", "threadId": "thread-2", "turnId": "turn-2", "autoWake": true, "at": 1792684447.7961004, "stage": "pending", "startAttempt": null}, "disconnectRecovery": {"epoch": 3, "accountKey": "default", "threadId": "thread-2", "turnId": "turn-2", "autoWake": true, "at": 1792684447.7961004, "source": "restart"}}');
INSERT INTO runtime_execution_runs VALUES('run:3d30ec42-08b4-5705-a9f1-265b1e23298c','aaaaaaaa-1111-4111-8111-111111111111','default',3,'thread-1','turn-1',1792684447.3010902,'{"id": "run:3d30ec42-08b4-5705-a9f1-265b1e23298c", "agent": "aaaaaaaa-1111-4111-8111-111111111111", "accountKey": "default", "epoch": 3, "threadId": "thread-1", "turnId": "turn-1", "created": 1792684447.3010902, "status": "running", "firstAttemptId": null, "rootAttemptId": null, "latestAttemptId": null, "reconciled": true, "restartRecovery": {"epoch": 3, "accountKey": "default", "threadId": "thread-1", "turnId": "turn-1", "autoWake": true, "at": 1792684447.7965105, "stage": "pending", "startAttempt": null}, "disconnectRecovery": {"epoch": 3, "accountKey": "default", "threadId": "thread-1", "turnId": "turn-1", "autoWake": true, "at": 1792684447.7965105, "source": "restart"}}');
INSERT INTO runtime_execution_runs VALUES('run:cd8957f8-27ab-5eef-a9c3-5a02a523ef8b','aaaaaaaa-2222-4222-8222-222222222222','default',3,'thread-1','turn-1',1792684447.304318,'{"id": "run:cd8957f8-27ab-5eef-a9c3-5a02a523ef8b", "agent": "aaaaaaaa-2222-4222-8222-222222222222", "accountKey": "default", "epoch": 3, "threadId": "thread-1", "turnId": "turn-1", "created": 1792684447.3043177, "status": "running", "firstAttemptId": null, "rootAttemptId": null, "latestAttemptId": null, "reconciled": true, "restartRecovery": {"epoch": 3, "accountKey": "default", "threadId": "thread-1", "turnId": "turn-1", "autoWake": true, "at": 1792684447.7969077, "stage": "pending", "startAttempt": null}, "disconnectRecovery": {"epoch": 3, "accountKey": "default", "threadId": "thread-1", "turnId": "turn-1", "autoWake": true, "at": 1792684447.7969077, "source": "restart"}}');
INSERT INTO runtime_execution_runs VALUES('run:3c0493c4-e9bc-5726-b0e6-6232cb0a370b','bbbbbbbb-1111-4111-8111-111111111111','default',3,'thread-2','turn-2',1792684447.307078,'{"id": "run:3c0493c4-e9bc-5726-b0e6-6232cb0a370b", "agent": "bbbbbbbb-1111-4111-8111-111111111111", "accountKey": "default", "epoch": 3, "threadId": "thread-2", "turnId": "turn-2", "created": 1792684447.3070784, "status": "running", "firstAttemptId": null, "rootAttemptId": null, "latestAttemptId": null, "reconciled": true, "restartRecovery": {"epoch": 3, "accountKey": "default", "threadId": "thread-2", "turnId": "turn-2", "autoWake": true, "at": 1792684447.7972267, "stage": "pending", "startAttempt": null}, "disconnectRecovery": {"epoch": 3, "accountKey": "default", "threadId": "thread-2", "turnId": "turn-2", "autoWake": true, "at": 1792684447.7972267, "source": "restart"}}');
INSERT INTO runtime_execution_runs VALUES('run:baaa4bf8-d7e3-5029-a6e4-ee9bead0baf9','bbbbbbbb-2222-4222-8222-222222222222','default',3,'thread-1','turn-1',1792684447.3093696,'{"id": "run:baaa4bf8-d7e3-5029-a6e4-ee9bead0baf9", "agent": "bbbbbbbb-2222-4222-8222-222222222222", "accountKey": "default", "epoch": 3, "threadId": "thread-1", "turnId": "turn-1", "created": 1792684447.3093696, "status": "running", "firstAttemptId": null, "rootAttemptId": null, "latestAttemptId": null, "reconciled": true, "restartRecovery": {"epoch": 3, "accountKey": "default", "threadId": "thread-1", "turnId": "turn-1", "autoWake": true, "at": 1792684447.797499, "stage": "pending", "startAttempt": null}, "disconnectRecovery": {"epoch": 3, "accountKey": "default", "threadId": "thread-1", "turnId": "turn-1", "autoWake": true, "at": 1792684447.797499, "source": "restart"}}');
INSERT INTO runtime_execution_runs VALUES('run:6cbe02fd-b7fd-5704-9198-1be59eb38284','cccccccc-1111-4111-8111-111111111111','default',3,'thread-1','turn-1',1792684447.311975,'{"id": "run:6cbe02fd-b7fd-5704-9198-1be59eb38284", "agent": "cccccccc-1111-4111-8111-111111111111", "accountKey": "default", "epoch": 3, "threadId": "thread-1", "turnId": "turn-1", "created": 1792684447.311975, "status": "running", "firstAttemptId": null, "rootAttemptId": null, "latestAttemptId": null, "reconciled": true}');
CREATE TABLE runtime_federation_identity (
          id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_federation_inbox (
          peer_id TEXT NOT NULL, message_id TEXT NOT NULL, record TEXT NOT NULL, receipt_ack TEXT GENERATED ALWAYS AS (json_extract(record,'$.ack')) VIRTUAL NOT NULL CHECK("receipt_ack" IN ('pending','sent','confirmed')), receipt_kind TEXT GENERATED ALWAYS AS (json_extract(record,'$.kind')) VIRTUAL  CHECK("receipt_kind" IN ('chat-message','room-invite','room-accept')),
          PRIMARY KEY(peer_id,message_id));
CREATE TABLE runtime_federation_invites (
          id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_federation_outbox (
          id TEXT PRIMARY KEY, peer_id TEXT NOT NULL, record TEXT NOT NULL, receipt_state TEXT GENERATED ALWAYS AS (json_extract(record,'$.status')) VIRTUAL NOT NULL CHECK("receipt_state" IN ('queued','delivered')), receipt_kind TEXT GENERATED ALWAYS AS (json_extract(record,'$.kind')) VIRTUAL NOT NULL CHECK("receipt_kind" IN ('chat-message','room-invite','room-accept')));
CREATE TABLE runtime_federation_peers (
          id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_federation_rooms (
          id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_federation_settings (
          id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_item_fulltext (id TEXT PRIMARY KEY, body TEXT NOT NULL);
CREATE TABLE runtime_items (
                  id TEXT PRIMARY KEY, agent TEXT NOT NULL, record TEXT NOT NULL, created REAL NOT NULL);
INSERT INTO runtime_items VALUES('abf56fbb-f8dc-4241-ad30-3960b0afa460:preview','abf56fbb-f8dc-4241-ad30-3960b0afa460','{"id": "abf56fbb-f8dc-4241-ad30-3960b0afa460:preview", "role": "tool", "title": "commandExecution", "text": "{\"command\": \"printf review\", \"cwd\": \"/fixture\", \"changes\": [{\"path\": \"file.txt\", \"kind\": \"update\"}]}", "truncated": false, "at": 1792684447.5432637}',1792684447.543277);
CREATE TABLE runtime_lead_requests (id TEXT PRIMARY KEY, agent TEXT NOT NULL, signature TEXT NOT NULL);
CREATE TABLE runtime_plans (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_profiles (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_projects (id TEXT PRIMARY KEY, record TEXT NOT NULL, removal_reason TEXT GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN record->>'$.removal.reason' END) VIRTUAL CHECK("removal_reason" IN ('delete')));
INSERT INTO runtime_projects VALUES('__CALLBACK_STATE__','{"id":"__CALLBACK_STATE__","path":"__CALLBACK_STATE__","name":"template","created":1792684446.784392,"accountKey":"default","accountRevision":1,"homeServerId":"c22626d9ddce41e5a3b269d2299f3e5e","locations":[{"serverId":"c22626d9ddce41e5a3b269d2299f3e5e","path":"__CALLBACK_STATE__","projectId":"__CALLBACK_STATE__"}],"locationsRevision":0,"peerTeams":[]}');
CREATE TABLE runtime_requests (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_rooms (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_rules (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_scheduler_agent_change_state (id INTEGER PRIMARY KEY CHECK(id=1),floor INTEGER NOT NULL,generation INTEGER NOT NULL,entries INTEGER NOT NULL,total INTEGER NOT NULL);
INSERT INTO runtime_scheduler_agent_change_state VALUES(1,0,148,8,1156);
CREATE TABLE runtime_scheduler_agent_changes (id TEXT PRIMARY KEY,generation INTEGER NOT NULL CHECK(typeof(generation)='integer' AND generation>=0));
INSERT INTO runtime_scheduler_agent_changes VALUES('abf56fbb-f8dc-4241-ad30-3960b0afa460',141);
INSERT INTO runtime_scheduler_agent_changes VALUES('3e73e736-e287-42b7-963c-6bef9312a291',142);
INSERT INTO runtime_scheduler_agent_changes VALUES('7c2692e5-4303-49f4-8ba0-cdc5a6f1db4a',143);
INSERT INTO runtime_scheduler_agent_changes VALUES('aaaaaaaa-1111-4111-8111-111111111111',144);
INSERT INTO runtime_scheduler_agent_changes VALUES('aaaaaaaa-2222-4222-8222-222222222222',145);
INSERT INTO runtime_scheduler_agent_changes VALUES('bbbbbbbb-1111-4111-8111-111111111111',146);
INSERT INTO runtime_scheduler_agent_changes VALUES('bbbbbbbb-2222-4222-8222-222222222222',147);
INSERT INTO runtime_scheduler_agent_changes VALUES('cccccccc-1111-4111-8111-111111111111',148);
CREATE TABLE runtime_search_next_meta (
                id TEXT PRIMARY KEY, agent TEXT NOT NULL, kind TEXT NOT NULL,
                search_rowid INTEGER NOT NULL UNIQUE);
CREATE TABLE runtime_search_rollout (
                id INTEGER PRIMARY KEY CHECK(id=1), phase TEXT NOT NULL,
                cursor INTEGER NOT NULL DEFAULT 0, updated REAL NOT NULL);
INSERT INTO runtime_search_rollout VALUES(1,'building',0,1792684446.7317002);
CREATE TABLE runtime_server_fetch (
                    id TEXT PRIMARY KEY, server TEXT NOT NULL, signature TEXT NOT NULL,
                    state TEXT NOT NULL, result TEXT, admission TEXT, receipt_state TEXT GENERATED ALWAYS AS (state) VIRTUAL NOT NULL CHECK("receipt_state" IN ('preparing','fetching','complete')));
CREATE TABLE runtime_server_inbox (
                    id TEXT PRIMARY KEY, signature TEXT NOT NULL, state TEXT NOT NULL,
                    result TEXT, created_at REAL NOT NULL DEFAULT 0, completed_at REAL NOT NULL DEFAULT 0, principal TEXT, actor TEXT, admission TEXT, receipt_state TEXT GENERATED ALWAYS AS (state) VIRTUAL NOT NULL CHECK("receipt_state" IN ('queued','running','waiting','complete')));
CREATE TABLE runtime_server_links (
                    id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_server_outbox (
                    id TEXT PRIMARY KEY, server TEXT NOT NULL, body TEXT NOT NULL,
                    state TEXT NOT NULL, result TEXT, attempts INTEGER NOT NULL DEFAULT 0,
                    next_at REAL NOT NULL DEFAULT 0, error TEXT, created_at REAL NOT NULL DEFAULT 0, completed_at REAL NOT NULL DEFAULT 0, fingerprint TEXT, admission TEXT, receipt_state TEXT GENERATED ALWAYS AS (state) VIRTUAL NOT NULL CHECK("receipt_state" IN ('queued','running','waiting','complete')), receipt_kind TEXT GENERATED ALWAYS AS (json_extract(body,'$.action')) VIRTUAL NOT NULL CHECK("receipt_kind" IN ('event','input','message','move_send','move_status','location_info','location_matches','stop','exec_event','projects','projects.changed','folders','git','export','chunk','exec_read','exec_input','exec_cancel','release','update.offer','update.export','update.chunk','update.release','backup.put','task','complaint','context','directory','chat_read','move_manage','move_interrupt','move_spawn','exec','exec_receipt','browser_callback','add_location','remove_location','move_validate','move_begin','device_trust')));
CREATE TABLE runtime_server_output_tools (
                    id TEXT PRIMARY KEY, expires REAL NOT NULL);
CREATE TABLE runtime_server_retired (
                    id TEXT NOT NULL, direction TEXT NOT NULL, signature TEXT NOT NULL,
                    server TEXT, actor TEXT, PRIMARY KEY(id,direction));
CREATE TABLE runtime_sidebar_order (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_tasks (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_tool_requests (id TEXT PRIMARY KEY, record TEXT NOT NULL, origin_key TEXT GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN record->>'$.originKey' END) VIRTUAL, agent TEXT GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN CASE record->>'$.kind' WHEN 'managed_tool' THEN record->>'$.agent' WHEN 'provider_callback' THEN record->>'$.payload.identity.context.agent' END END) VIRTUAL, kind TEXT GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN record->>'$.kind' END) VIRTUAL CHECK("kind" IN ('managed_tool','provider_callback','unrecognized')), status TEXT GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN CASE record->>'$.kind' WHEN 'managed_tool' THEN record->>'$.stage' WHEN 'provider_callback' THEN record->>'$.status' END END) VIRTUAL CHECK("status" IN ('queued','running','cancelled','interrupted','failed','unknown','completed')), request_id TEXT GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN CASE record->>'$.kind' WHEN 'managed_tool' THEN record->>'$.admitted.id' WHEN 'provider_callback' THEN record->>'$.id' END END) VIRTUAL, updated INTEGER GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN record->>'$.updated' END) VIRTUAL);
INSERT INTO runtime_tool_requests VALUES('thread-0:alias-0','{"accountKey":"default","admitted":{"created":1793451753336,"id":"thread-0:alias-0","signature":"1dff160641e4d1fe77178179b43844c3c6d588f8f3741f77bb9b8655c3a5e579"},"agent":"abf56fbb-f8dc-4241-ad30-3960b0afa460","aliases":["alias-0","thread-0:alias-0"],"callId":"alias-0","cancelRequested":false,"connectionId":"connection","created":10.0,"epoch":3,"error":"Server restarted before the execution receipt","executionQueueDelayMs":1792684437211.5857,"id":"thread-0:alias-0","kind":"managed_tool","originKey":null,"outcome":"unknown","readOnly":true,"rpcId":0,"signature":"1dff160641e4d1fe77178179b43844c3c6d588f8f3741f77bb9b8655c3a5e579","stage":"interrupted","started":1792684447.2115858,"threadId":"thread-0","timing":{"handlerStartedAt":223681181514591,"reservationBeganAt":223681162235562,"reservationEndedAt":223681164328340,"reservationLockedAt":223681163731562},"tool":"orchestration_task","turnId":"turn-0","updated":1792684447.805279}');
INSERT INTO runtime_tool_requests VALUES('thread-0:spawn:request-1','{"accountKey":"default","admitted":{"created":1793451753336,"id":"thread-0:spawn:request-1","signature":"c91f8a50f288534226b47d6abda036631ba03b0ad5aae43ad0b8e1fe6431fc12"},"agent":"abf56fbb-f8dc-4241-ad30-3960b0afa460","aliases":["alias-1","thread-0:alias-1","request-1"],"callId":"alias-1","cancelRequested":false,"connectionId":"connection","created":10.0,"epoch":3,"error":"Server restarted before execution","id":"thread-0:spawn:request-1","kind":"managed_tool","originKey":null,"outcome":"not_applied","readOnly":false,"request_id":"request-1","rpcId":1,"signature":"c91f8a50f288534226b47d6abda036631ba03b0ad5aae43ad0b8e1fe6431fc12","stage":"cancelled","threadId":"thread-0","timing":{"reservationBeganAt":223681165205452,"reservationEndedAt":223681166951360,"reservationLockedAt":223681166531504},"tool":"orchestration_spawn","turnId":"turn-0","updated":1792684447.8056414}');
INSERT INTO runtime_tool_requests VALUES('thread-0:send:request-2','{"accountKey":"default","admitted":{"created":1793451753336,"id":"thread-0:send:request-2","signature":"abb0f8214ffec1e848f67f36b7b030d1f53121efbd2979fd75680fd3c7785749"},"agent":"abf56fbb-f8dc-4241-ad30-3960b0afa460","aliases":["alias-2","thread-0:alias-2","request-2"],"callId":"alias-2","cancelRequested":false,"connectionId":"connection","created":10.0,"epoch":3,"error":"Server restarted before execution","id":"thread-0:send:request-2","kind":"managed_tool","originKey":null,"outcome":"not_applied","readOnly":false,"request_id":"request-2","rpcId":2,"signature":"abb0f8214ffec1e848f67f36b7b030d1f53121efbd2979fd75680fd3c7785749","stage":"cancelled","threadId":"thread-0","timing":{"reservationBeganAt":223681167627355,"reservationEndedAt":223681169180973,"reservationLockedAt":223681168803998},"tool":"orchestration_send","turnId":"turn-0","updated":1792684447.8057444}');
INSERT INTO runtime_tool_requests VALUES('thread-0:review:request-3','{"accountKey":"default","admitted":{"created":1793451753336,"id":"thread-0:review:request-3","signature":"69b6d0789d8ba6854d46da3a2ec8e90d8871e7ce6895cd678acd23aaf86106ca"},"agent":"abf56fbb-f8dc-4241-ad30-3960b0afa460","aliases":["alias-3","thread-0:alias-3","request-3"],"callId":"alias-3","cancelRequested":false,"connectionId":"connection","created":10.0,"epoch":3,"error":"Server restarted before execution","id":"thread-0:review:request-3","kind":"managed_tool","originKey":null,"outcome":"not_applied","readOnly":false,"request_id":"request-3","rpcId":3,"signature":"69b6d0789d8ba6854d46da3a2ec8e90d8871e7ce6895cd678acd23aaf86106ca","stage":"cancelled","threadId":"thread-0","timing":{"reservationBeganAt":223681169847842,"reservationEndedAt":223681171825163,"reservationLockedAt":223681171343832},"tool":"orchestration_review","turnId":"turn-0","updated":1792684447.8058205}');
INSERT INTO runtime_tool_requests VALUES('thread-0:server:request-4','{"accountKey":"default","admitted":{"created":1793451753336,"id":"thread-0:server:request-4","signature":"4b00aba39b4be2847cacf972191005ff1b73f15438c48c9b54e8507c26aeb478"},"agent":"abf56fbb-f8dc-4241-ad30-3960b0afa460","aliases":["alias-4","thread-0:alias-4","request-4"],"callId":"alias-4","cancelRequested":false,"connectionId":"connection","created":10.0,"epoch":3,"error":"Server restarted before execution","id":"thread-0:server:request-4","kind":"managed_tool","originKey":null,"outcome":"not_applied","readOnly":false,"request_id":"request-4","rpcId":4,"signature":"4b00aba39b4be2847cacf972191005ff1b73f15438c48c9b54e8507c26aeb478","stage":"cancelled","threadId":"thread-0","timing":{"reservationBeganAt":223681172689060,"reservationEndedAt":223681174577605,"reservationLockedAt":223681174125018},"tool":"orchestration_servers","turnId":"turn-0","updated":1792684447.8058898}');
INSERT INTO runtime_tool_requests VALUES('thread-0:move:request-5','{"accountKey":"default","admitted":{"created":1793451753336,"id":"thread-0:move:request-5","signature":"00cebdf9732e1609eecdd9a0f86b687817cd2a371ef83e312ac50706f822f661"},"agent":"abf56fbb-f8dc-4241-ad30-3960b0afa460","aliases":["alias-5","thread-0:alias-5","request-5"],"callId":"alias-5","cancelRequested":false,"connectionId":"connection","created":10.0,"epoch":3,"error":"Server restarted before execution","id":"thread-0:move:request-5","kind":"managed_tool","originKey":null,"outcome":"not_applied","readOnly":false,"request_id":"request-5","rpcId":5,"signature":"00cebdf9732e1609eecdd9a0f86b687817cd2a371ef83e312ac50706f822f661","stage":"cancelled","threadId":"thread-0","timing":{"reservationBeganAt":223681175384957,"reservationEndedAt":223681177367979,"reservationLockedAt":223681176984230},"tool":"orchestration_move","turnId":"turn-0","updated":1792684447.8059556}');
INSERT INTO runtime_tool_requests VALUES('thread-0:move:request-6','{"accountKey":"default","admitted":{"created":1793451753336,"id":"thread-0:move:request-6","signature":"4d2a73cfcffe4df8b7075fa3edcc43dc0faab254b347892d55b3c227be583e82"},"agent":"abf56fbb-f8dc-4241-ad30-3960b0afa460","aliases":["alias-6","thread-0:alias-6","request-6"],"callId":"alias-6","cancelRequested":false,"connectionId":"connection","created":10.0,"epoch":3,"error":"Server restarted before execution","id":"thread-0:move:request-6","kind":"managed_tool","originKey":null,"outcome":"not_applied","readOnly":false,"request_id":"request-6","rpcId":6,"signature":"4d2a73cfcffe4df8b7075fa3edcc43dc0faab254b347892d55b3c227be583e82","stage":"cancelled","threadId":"thread-0","timing":{"reservationBeganAt":223681177956620,"reservationEndedAt":223681179710313,"reservationLockedAt":223681179373843},"tool":"orchestration_teleport","turnId":"turn-0","updated":1792684447.806042}');
CREATE TABLE runtime_transcript_revisions(
                        agent TEXT PRIMARY KEY, revision INTEGER NOT NULL DEFAULT 0);
INSERT INTO runtime_transcript_revisions VALUES('abf56fbb-f8dc-4241-ad30-3960b0afa460',127);
INSERT INTO runtime_transcript_revisions VALUES('3e73e736-e287-42b7-963c-6bef9312a291',1);
INSERT INTO runtime_transcript_revisions VALUES('7c2692e5-4303-49f4-8ba0-cdc5a6f1db4a',1);
INSERT INTO runtime_transcript_revisions VALUES('aaaaaaaa-1111-4111-8111-111111111111',1);
INSERT INTO runtime_transcript_revisions VALUES('aaaaaaaa-2222-4222-8222-222222222222',1);
INSERT INTO runtime_transcript_revisions VALUES('bbbbbbbb-1111-4111-8111-111111111111',1);
INSERT INTO runtime_transcript_revisions VALUES('bbbbbbbb-2222-4222-8222-222222222222',1);
INSERT INTO runtime_transcript_revisions VALUES('cccccccc-1111-4111-8111-111111111111',1);
CREATE TABLE runtime_usage_resumes (id TEXT PRIMARY KEY, agent TEXT NOT NULL, record TEXT NOT NULL);
CREATE TABLE runtime_work (id TEXT PRIMARY KEY, record TEXT NOT NULL, status TEXT GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN record->>'$.status' END) VIRTUAL CHECK("status" IN ('ready','blocked','running','review','accepted','cancelled')), archive_status TEXT GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN record->>'$.archiveIntent.status' END) VIRTUAL CHECK("archive_status" IN ('completed','terminal','pending','retryable','submitted','unknown','not_sent','complete','applied','failed','cancelled')), archive_next_at REAL GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN COALESCE(record->>'$.archiveIntent.nextAttemptAt',0) END) VIRTUAL, vm_submit_status TEXT GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN record->>'$.vmSubmit.status' END) VIRTUAL CHECK("vm_submit_status" IN ('completed','terminal','pending','retryable','submitted','unknown','not_sent','complete','applied','failed','cancelled')), vm_accept_status TEXT GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN record->>'$.vmAccept.status' END) VIRTUAL CHECK("vm_accept_status" IN ('completed','terminal','pending','retryable','submitted','unknown','not_sent','complete','applied','failed','cancelled')));
CREATE TABLE runtime_workspace_operations (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE sync_documents (
        seq INTEGER PRIMARY KEY AUTOINCREMENT, scope TEXT NOT NULL,
        id TEXT NOT NULL, payload TEXT NOT NULL, deleted INTEGER NOT NULL DEFAULT 0,
        UNIQUE(scope,id));
CREATE TABLE sync_entities (
        collection TEXT NOT NULL, id TEXT NOT NULL, seq INTEGER NOT NULL,
        hash TEXT NOT NULL, payload TEXT, deleted INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY(collection,id));
INSERT INTO sync_entities VALUES('project','__CALLBACK_STATE__',1,'410fc08cbbbac2bcdcbf8eef2367b14c74b03d5f6a709773d1f309bf37f20930','{"collection":"project","id":"__CALLBACK_STATE__","value":{"accountKey":"default","accountRevision":1,"created":1792684446.784392,"homeServerId":"c22626d9ddce41e5a3b269d2299f3e5e","id":"__CALLBACK_STATE__","locations":[{"path":"__CALLBACK_STATE__","projectId":"__CALLBACK_STATE__","serverId":"c22626d9ddce41e5a3b269d2299f3e5e"}],"locationsRevision":0,"name":"template","path":"__CALLBACK_STATE__"}}',0);
INSERT INTO sync_entities VALUES('agent','abf56fbb-f8dc-4241-ad30-3960b0afa460',346,'55a50a583672e6e1a9ceb99e46be3924a7437e8eca1bd2a28bb5f0a666251d96','{"collection": "agent", "id": "abf56fbb-f8dc-4241-ad30-3960b0afa460", "value": {"accountKey": "default", "agentMode": "multi", "agentModeRevision": 0, "agentModeSupported": true, "autoWake": true, "canSend": true, "commandProgress": [], "compactions": 0, "concurrency": 32, "contextUsage": null, "created": 1792684446.7842586, "cwd": "__CALLBACK_STATE__", "cyberAccessProgram": "standard", "daybreakEnabled": false, "effort": "medium", "empty": false, "environment": "host", "epoch": 3, "error": null, "executionMode": "native", "fastMode": false, "hostProjectPath": "__CALLBACK_STATE__", "id": "abf56fbb-f8dc-4241-ad30-3960b0afa460", "inFlight": false, "isLead": true, "kind": "agent", "launcherAlive": true, "layrReady": false, "model": "gpt-6-astra", "name": "Callback lead", "nextTurnSettingsSupported": true, "parentId": null, "provider": "codex", "readStateSupported": true, "reviewDefaults": {"effort": null, "model": null}, "role": "orchestrator", "rootId": "abf56fbb-f8dc-4241-ad30-3960b0afa460", "serverId": "c22626d9ddce41e5a3b269d2299f3e5e", "source": "managed", "stall": null, "startWait": null, "status": "running", "subagentConcurrencyVersion": 2, "tail": "", "threadId": "thread-0", "tokensUsed": 0, "turnId": "turn-0", "workerDefaults": {"daybreakEnabled": false, "delegationDepth": 1, "effort": "high", "fastMode": false, "model": "gpt-6-luna"}, "workspaceBackend": null, "workspaceMode": "worktree", "worktree": false, "yoloMode": true}}',0);
INSERT INTO sync_entities VALUES('agent','3e73e736-e287-42b7-963c-6bef9312a291',136,'9445eeaa88debfef2b1d0de4c2751a0663ea7c5bfa838c92032067da602f8316','{"collection": "agent", "id": "3e73e736-e287-42b7-963c-6bef9312a291", "value": {"accountKey": "default", "agentMode": "multi", "agentModeRevision": 0, "agentModeSupported": true, "autoWake": true, "canSend": true, "commandProgress": [], "compactions": 0, "concurrency": 32, "contextUsage": null, "created": 1792684446.7901473, "cwd": "__CALLBACK_STATE__", "cyberAccessProgram": "standard", "daybreakEnabled": false, "effort": "medium", "environment": "host", "epoch": 3, "error": null, "executionMode": "native", "fastMode": false, "hostProjectPath": "__CALLBACK_STATE__", "id": "3e73e736-e287-42b7-963c-6bef9312a291", "inFlight": false, "isLead": false, "kind": "agent", "launcherAlive": true, "layrReady": false, "model": "gpt-6-astra", "name": "Callback worker", "nextTurnSettingsSupported": true, "overview": {"result": "", "resultFile": null, "resultTruncated": false, "resultTurnId": null, "task": "fixture", "taskTruncated": false}, "parentId": "abf56fbb-f8dc-4241-ad30-3960b0afa460", "provider": "codex", "readStateSupported": true, "reviewDefaults": {"effort": null, "model": null}, "role": "orchestrator", "rootId": "abf56fbb-f8dc-4241-ad30-3960b0afa460", "serverId": "c22626d9ddce41e5a3b269d2299f3e5e", "source": "managed", "stall": null, "startWait": null, "status": "running", "subagentConcurrencyVersion": 2, "tail": "", "threadId": "thread-1", "tokensUsed": 0, "turnId": "turn-1", "workerDefaults": {"daybreakEnabled": false, "delegationDepth": 1, "effort": "high", "fastMode": false, "model": "gpt-6-luna"}, "workspaceBackend": null, "workspaceMode": "worktree", "worktree": false, "yoloMode": true}}',0);
INSERT INTO sync_entities VALUES('agent','7c2692e5-4303-49f4-8ba0-cdc5a6f1db4a',137,'a7d0113df7ad39da48ae6c28291a5b0f23e0606a6a537b4554a79c243a66f7fe','{"collection": "agent", "id": "7c2692e5-4303-49f4-8ba0-cdc5a6f1db4a", "value": {"accountKey": "default", "agentMode": "multi", "agentModeRevision": 0, "agentModeSupported": true, "autoWake": true, "canSend": true, "commandProgress": [], "compactions": 0, "concurrency": 32, "contextUsage": null, "created": 1792684446.7960482, "cwd": "__CALLBACK_STATE__", "cyberAccessProgram": "standard", "daybreakEnabled": false, "effort": "medium", "empty": false, "environment": "host", "epoch": 3, "error": null, "executionMode": "native", "fastMode": false, "hostProjectPath": "__CALLBACK_STATE__", "id": "7c2692e5-4303-49f4-8ba0-cdc5a6f1db4a", "inFlight": false, "isLead": true, "kind": "agent", "launcherAlive": true, "layrReady": false, "model": "gpt-6-astra", "name": "Other team", "nextTurnSettingsSupported": true, "parentId": null, "provider": "codex", "readStateSupported": true, "reviewDefaults": {"effort": null, "model": null}, "role": "orchestrator", "rootId": "7c2692e5-4303-49f4-8ba0-cdc5a6f1db4a", "serverId": "c22626d9ddce41e5a3b269d2299f3e5e", "source": "managed", "stall": null, "startWait": null, "status": "running", "subagentConcurrencyVersion": 2, "tail": "", "threadId": "thread-2", "tokensUsed": 0, "turnId": "turn-2", "workerDefaults": {"daybreakEnabled": false, "delegationDepth": 1, "effort": "high", "fastMode": false, "model": "gpt-6-luna"}, "workspaceBackend": null, "workspaceMode": "worktree", "worktree": false, "yoloMode": true}}',0);
INSERT INTO sync_entities VALUES('request','answer-0',9,'ca780824cd43ffc0e5eef158a5ab34ae0e9ec57bbc342f18efea62e55be2efe7','{"collection":"request","id":"answer-0","value":{}}',1);
INSERT INTO sync_entities VALUES('request','answer-1',11,'600ec1576085c7345df51a1d24d1c04d2836a7a625adb13de0ee2bd4d514f64e','{"collection":"request","id":"answer-1","value":{}}',1);
INSERT INTO sync_entities VALUES('request','answer-2',13,'b6ddd9f61d40f13509a0e91850aae6a39332031fb6c3aad43f47d69c66ba736d','{"collection":"request","id":"answer-2","value":{}}',1);
INSERT INTO sync_entities VALUES('request','answer-3',15,'0097b9b99d887aa31a64948aaf35a9cc5284b29933c8531a0b3e4ba2adecea5a','{"collection":"request","id":"answer-3","value":{}}',1);
INSERT INTO sync_entities VALUES('request','answer-4',17,'78d304379b02c07bff7788800c50147b093790b9b3cae7975befc4dd1d267c8b','{"collection":"request","id":"answer-4","value":{}}',1);
INSERT INTO sync_entities VALUES('request','answer-5',19,'bb5f7a79ae1b4730ca02265bfdbbf86507eec5f166c7115963e95b8a62085b57','{"collection":"request","id":"answer-5","value":{}}',1);
INSERT INTO sync_entities VALUES('request','answer-6',21,'4af940c68e6ce2dc9acf18d7a8593d304eff5c7978fbff00c71daf97ae2426e2','{"collection":"request","id":"answer-6","value":{}}',1);
INSERT INTO sync_entities VALUES('request','answer-7',23,'71248c4ae2759d0257bb131c5b434d64c745abdf4e801c2fabe6c2cee6043490','{"collection":"request","id":"answer-7","value":{}}',1);
INSERT INTO sync_entities VALUES('request','answer-8',25,'7991f8ce68ae78287e0794d10e38666769f5d4859d2309ef7e40767d0518773b','{"collection":"request","id":"answer-8","value":{}}',1);
INSERT INTO sync_entities VALUES('request','answer-9',27,'67d06bd9b0b8507a29b26084c2916fbd751945a440cdeca5b61ed6c1c22f1791','{"collection":"request","id":"answer-9","value":{}}',1);
INSERT INTO sync_entities VALUES('request','answer-10',29,'f3a4368c277a2d8be1ea88c4f19382233d5ba7503f223596cea4fb9b2f8d2829','{"collection":"request","id":"answer-10","value":{}}',1);
INSERT INTO sync_entities VALUES('request','answer-11',31,'3659d9dca58ea38273a7933a72c4eb42350a24e38a468cbebd703a3b32b70149','{"collection":"request","id":"answer-11","value":{}}',1);
INSERT INTO sync_entities VALUES('request','answer-12',33,'ff1b0099378833648c9a1cb497891c8b1ea91f81b5f85df4d027eff3e8c98212','{"collection":"request","id":"answer-12","value":{}}',1);
INSERT INTO sync_entities VALUES('request','answer-13',35,'53adca3f2506a6ecc4e2c3deb57cb7c781218c018cf1388a99a9840c4e2d44de','{"collection":"request","id":"answer-13","value":{}}',1);
INSERT INTO sync_entities VALUES('request','answer-14',37,'9cede43c415fc336299b87173df9d7972d46e1d8d67773ddde4bfc95d8bbc0b4','{"collection":"request","id":"answer-14","value":{}}',1);
INSERT INTO sync_entities VALUES('request','answer-15',39,'289a2c77d5ec9176f1965efad5737448dc0234807388ff1ebfe3e035dd1ed66f','{"collection":"request","id":"answer-15","value":{}}',1);
INSERT INTO sync_entities VALUES('request','answer-16',41,'f362717c0703093fc9778272136959fee9958e8064705d16f7c50f3d5968b4e3','{"collection":"request","id":"answer-16","value":{}}',1);
INSERT INTO sync_entities VALUES('request','answer-17',43,'9dadbf231395b5d03b3240c24797ee2186eca4e6422ad52debc30c7f95fb5fa8','{"collection":"request","id":"answer-17","value":{}}',1);
INSERT INTO sync_entities VALUES('request','answer-18',45,'a6b4216be987bff145b27ed8b5f17b3b44c939c4930d6d5eae446c655e3320a2','{"collection":"request","id":"answer-18","value":{}}',1);
INSERT INTO sync_entities VALUES('request','answer-19',47,'34111e5d7135cabf313dfe1c69e7d0ca3a67887a752d1c1548ebceb77e624378','{"collection":"request","id":"answer-19","value":{}}',1);
INSERT INTO sync_entities VALUES('request','answer-20',49,'d4c793c9c991018070b19a52c6a92a5fad975edb71e151f7f8ad463e2f43bb35','{"collection":"request","id":"answer-20","value":{}}',1);
INSERT INTO sync_entities VALUES('request','answer-21',51,'f69b92a6e8dc6d23887503d931d0107df8bf2e6d6f54dead32281ee7b7ac4528','{"collection":"request","id":"answer-21","value":{}}',1);
INSERT INTO sync_entities VALUES('request','answer-22',53,'2c98bb62b49953b5300f884ccd554cc2d755becde757004f10e3d0c2dfec5e2e','{"collection":"request","id":"answer-22","value":{}}',1);
INSERT INTO sync_entities VALUES('request','answer-23',55,'17fe27955b4d421eff9ff0ccb66541b981b4fe9022811b544240f44e50265642','{"collection":"request","id":"answer-23","value":{}}',1);
INSERT INTO sync_entities VALUES('request','answer-24',57,'37d87bb32563438dabe2dc0b040680e4c9986d7e6fc9d0398b8a8f94af1c5acb','{"collection":"request","id":"answer-24","value":{}}',1);
INSERT INTO sync_entities VALUES('request','answer-25',59,'10cb4a8f05ebfc2294341f83c9321339d2a210e901686276f5cd8ada4a363252','{"collection":"request","id":"answer-25","value":{}}',1);
INSERT INTO sync_entities VALUES('request','answer-26',61,'2a244dca467341146a1c33b33ceaf68ea3d54f9cb2cb14a3545c7acdeb931174','{"collection":"request","id":"answer-26","value":{}}',1);
INSERT INTO sync_entities VALUES('request','answer-27',63,'3675bf7bd8a97ddfdb7ad128348f1628576d6f4fdc48524dfb4df3d46a3089c0','{"collection":"request","id":"answer-27","value":{}}',1);
INSERT INTO sync_entities VALUES('request','answer-28',65,'330077b156dd9c69b9cf9a60b7eaf4f67a735367c057e0b8ee10cb846a510df2','{"collection":"request","id":"answer-28","value":{}}',1);
INSERT INTO sync_entities VALUES('request','answer-29',67,'466f9c6cd01898afd34501a47f8e7950886eee8cf0727b9a9f80cc4d48d9ddc8','{"collection":"request","id":"answer-29","value":{}}',1);
INSERT INTO sync_entities VALUES('agent','aaaaaaaa-1111-4111-8111-111111111111',127,'43088c10788f7f7304e91449020c91e0b36a92a50b0880088ff153f4578916d1','{"collection": "agent", "id": "aaaaaaaa-1111-4111-8111-111111111111", "value": {"accountKey": "default", "agentMode": "multi", "agentModeRevision": 0, "agentModeSupported": true, "autoWake": false, "canSend": true, "commandProgress": [], "compactions": 0, "contextUsage": null, "created": 1792684446.7901473, "cwd": "__CALLBACK_STATE__", "cyberAccessProgram": "standard", "daybreakEnabled": false, "deletedAt": null, "effort": "medium", "environment": "host", "epoch": 3, "error": "Server restarted during a turn. Review history, then send a new instruction.", "executionMode": "native", "fastMode": false, "hostProjectPath": "__CALLBACK_STATE__", "id": "aaaaaaaa-1111-4111-8111-111111111111", "inFlight": false, "isLead": false, "kind": "agent", "launcherAlive": true, "layrReady": false, "model": "gpt-6-astra", "name": "Callback worker", "nextTurnSettingsSupported": true, "overview": {"result": "", "resultFile": null, "resultTruncated": false, "resultTurnId": null, "task": "fixture", "taskTruncated": false}, "parentId": "abf56fbb-f8dc-4241-ad30-3960b0afa460", "provider": "codex", "readStateSupported": true, "reviewDefaults": {"effort": null, "model": null}, "role": "orchestrator", "rootId": "abf56fbb-f8dc-4241-ad30-3960b0afa460", "serverId": "c22626d9ddce41e5a3b269d2299f3e5e", "source": "managed", "stall": null, "startWait": null, "status": "interrupted", "subagentConcurrencyVersion": 2, "tail": "", "threadId": "thread-1", "tokensUsed": 0, "turnId": "turn-1", "workerDefaults": {"daybreakEnabled": false, "delegationDepth": 1, "effort": "high", "fastMode": false, "model": "gpt-6-luna"}, "workspaceBackend": null, "workspaceMode": "worktree", "worktree": false, "yoloMode": true}}',0);
INSERT INTO sync_entities VALUES('agent','aaaaaaaa-2222-4222-8222-222222222222',128,'dc4dbe89f7d044ff395b32142f143a6e204ea18c5cccaaf9671d1959c5611300','{"collection": "agent", "id": "aaaaaaaa-2222-4222-8222-222222222222", "value": {"accountKey": "default", "agentMode": "multi", "agentModeRevision": 0, "agentModeSupported": true, "autoWake": false, "canSend": true, "commandProgress": [], "compactions": 0, "contextUsage": null, "created": 1792684446.7901473, "cwd": "__CALLBACK_STATE__", "cyberAccessProgram": "standard", "daybreakEnabled": false, "deletedAt": null, "effort": "medium", "environment": "host", "epoch": 3, "error": "Server restarted during a turn. Review history, then send a new instruction.", "executionMode": "native", "fastMode": false, "hostProjectPath": "__CALLBACK_STATE__", "id": "aaaaaaaa-2222-4222-8222-222222222222", "inFlight": false, "isLead": false, "kind": "agent", "launcherAlive": true, "layrReady": false, "model": "gpt-6-astra", "name": "Callback worker", "nextTurnSettingsSupported": true, "overview": {"result": "", "resultFile": null, "resultTruncated": false, "resultTurnId": null, "task": "fixture", "taskTruncated": false}, "parentId": "abf56fbb-f8dc-4241-ad30-3960b0afa460", "provider": "codex", "readStateSupported": true, "reviewDefaults": {"effort": null, "model": null}, "role": "orchestrator", "rootId": "abf56fbb-f8dc-4241-ad30-3960b0afa460", "serverId": "c22626d9ddce41e5a3b269d2299f3e5e", "source": "managed", "stall": null, "startWait": null, "status": "interrupted", "subagentConcurrencyVersion": 2, "tail": "", "threadId": "thread-1", "tokensUsed": 0, "turnId": "turn-1", "workerDefaults": {"daybreakEnabled": false, "delegationDepth": 1, "effort": "high", "fastMode": false, "model": "gpt-6-luna"}, "workspaceBackend": null, "workspaceMode": "worktree", "worktree": false, "yoloMode": true}}',0);
INSERT INTO sync_entities VALUES('agent','bbbbbbbb-1111-4111-8111-111111111111',129,'a60c20f5e0a9e3978b9e42fb97cbf8ed199a3c2f063a13090fe351b1fc6c6045','{"collection": "agent", "id": "bbbbbbbb-1111-4111-8111-111111111111", "value": {"accountKey": "default", "agentMode": "multi", "agentModeRevision": 0, "agentModeSupported": true, "autoWake": false, "canSend": true, "commandProgress": [], "compactions": 0, "concurrency": 32, "contextUsage": null, "created": 1792684446.7960482, "cwd": "__CALLBACK_STATE__", "cyberAccessProgram": "standard", "daybreakEnabled": false, "deletedAt": null, "effort": "medium", "empty": false, "environment": "host", "epoch": 3, "error": "Server restarted during a turn. Review history, then send a new instruction.", "executionMode": "native", "fastMode": false, "hostProjectPath": "__CALLBACK_STATE__", "id": "bbbbbbbb-1111-4111-8111-111111111111", "inFlight": false, "isLead": true, "kind": "agent", "launcherAlive": true, "layrReady": false, "model": "gpt-6-astra", "name": "Other team", "nextTurnSettingsSupported": true, "parentId": null, "provider": "codex", "readStateSupported": true, "reviewDefaults": {"effort": null, "model": null}, "role": "orchestrator", "rootId": "bbbbbbbb-1111-4111-8111-111111111111", "serverId": "c22626d9ddce41e5a3b269d2299f3e5e", "source": "managed", "stall": null, "startWait": null, "status": "interrupted", "subagentConcurrencyVersion": 2, "tail": "", "threadId": "thread-2", "tokensUsed": 0, "turnId": "turn-2", "workerDefaults": {"daybreakEnabled": false, "delegationDepth": 1, "effort": "high", "fastMode": false, "model": "gpt-6-luna"}, "workspaceBackend": null, "workspaceMode": "worktree", "worktree": false, "yoloMode": true}}',0);
INSERT INTO sync_entities VALUES('agent','bbbbbbbb-2222-4222-8222-222222222222',130,'ae3676d3e799d3a700eee04b6dc77ddd95c55213dc9f97f742ff95b951a31e0c','{"collection": "agent", "id": "bbbbbbbb-2222-4222-8222-222222222222", "value": {"accountKey": "default", "agentMode": "multi", "agentModeRevision": 0, "agentModeSupported": true, "autoWake": false, "canSend": true, "commandProgress": [], "compactions": 0, "contextUsage": null, "created": 1792684446.7901473, "cwd": "__CALLBACK_STATE__", "cyberAccessProgram": "standard", "daybreakEnabled": false, "deletedAt": null, "effort": "medium", "environment": "host", "epoch": 3, "error": "Server restarted during a turn. Review history, then send a new instruction.", "executionMode": "native", "fastMode": false, "hostProjectPath": "__CALLBACK_STATE__", "id": "bbbbbbbb-2222-4222-8222-222222222222", "inFlight": false, "isLead": false, "kind": "agent", "launcherAlive": true, "layrReady": false, "model": "gpt-6-astra", "name": "Callback worker", "nextTurnSettingsSupported": true, "overview": {"result": "", "resultFile": null, "resultTruncated": false, "resultTurnId": null, "task": "fixture", "taskTruncated": false}, "parentId": "abf56fbb-f8dc-4241-ad30-3960b0afa460", "provider": "codex", "readStateSupported": true, "reviewDefaults": {"effort": null, "model": null}, "role": "orchestrator", "rootId": "abf56fbb-f8dc-4241-ad30-3960b0afa460", "serverId": "c22626d9ddce41e5a3b269d2299f3e5e", "source": "managed", "stall": null, "startWait": null, "status": "interrupted", "subagentConcurrencyVersion": 2, "tail": "", "threadId": "thread-1", "tokensUsed": 0, "turnId": "turn-1", "workerDefaults": {"daybreakEnabled": false, "delegationDepth": 1, "effort": "high", "fastMode": false, "model": "gpt-6-luna"}, "workspaceBackend": null, "workspaceMode": "worktree", "worktree": false, "yoloMode": true}}',0);
INSERT INTO sync_entities VALUES('agent','cccccccc-1111-4111-8111-111111111111',73,'cdad54a0bd7b1bd7dd271f09bfe9cf42fc1986c5a17f8342cb8e97b3a21d35aa','{"collection":"agent","id":"cccccccc-1111-4111-8111-111111111111","value":{}}',1);
INSERT INTO sync_entities VALUES('work','5d133257-ae7a-4370-aa83-1a34dd5b7e09',91,'5bc197a8cbb7d7de3f46fb225252095fa8ac9b595653dfeb5df4cd02e81a6896','{"collection":"work","id":"5d133257-ae7a-4370-aa83-1a34dd5b7e09","value":{"id":"5d133257-ae7a-4370-aa83-1a34dd5b7e09","rootId":"abf56fbb-f8dc-4241-ad30-3960b0afa460","status":"ready","title":"Valid title"}}',0);
INSERT INTO sync_entities VALUES('work','5c30f2eb-316e-4fb4-aa09-276856bf7385',77,'18a02031c746e55428e08a68ed064402c91f26ca3ec80e31c76bac0b67fe8958','{"collection":"work","id":"5c30f2eb-316e-4fb4-aa09-276856bf7385","value":{"id":"5c30f2eb-316e-4fb4-aa09-276856bf7385","rootId":"abf56fbb-f8dc-4241-ad30-3960b0afa460","status":"ready","title":"Valid title"}}',0);
INSERT INTO sync_entities VALUES('work','378e58ab-a1ce-428c-925f-b40b8e08143a',84,'389c1b6c492481cf3ff63bbe1b97db045d466117a1e65b5c3ccd80f5293af13d','{"collection":"work","id":"378e58ab-a1ce-428c-925f-b40b8e08143a","value":{"id":"378e58ab-a1ce-428c-925f-b40b8e08143a","rootId":"abf56fbb-f8dc-4241-ad30-3960b0afa460","status":"ready","title":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"}}',0);
INSERT INTO sync_entities VALUES('work','cdd039e5-e272-4855-8eff-fa22c71b841a',85,'aefc34a69e07a9845852092d9d7a91de9edffd89ce980566944db9f104dd3cf3','{"collection":"work","id":"cdd039e5-e272-4855-8eff-fa22c71b841a","value":{"id":"cdd039e5-e272-4855-8eff-fa22c71b841a","rootId":"abf56fbb-f8dc-4241-ad30-3960b0afa460","status":"blocked","title":"Valid title"}}',0);
INSERT INTO sync_entities VALUES('work','6cc71248-30bf-4811-b473-1aafcab6f5a8',86,'aac8f540448c2033867281cd98503e6b6d39848f2349deaa2a02f2e4a59e314c','{"collection":"work","id":"6cc71248-30bf-4811-b473-1aafcab6f5a8","value":{"id":"6cc71248-30bf-4811-b473-1aafcab6f5a8","rootId":"abf56fbb-f8dc-4241-ad30-3960b0afa460","status":"ready","title":"Valid title"}}',0);
INSERT INTO sync_entities VALUES('request','8b359a1f-85cd-4d54-adfd-733c9c049f3c',92,'e25115772397b38ee391b517e9292054f7cfd674dcd651140180d31527f61442','{"collection":"request","id":"8b359a1f-85cd-4d54-adfd-733c9c049f3c","value":{"accountKey":"default","agent":"abf56fbb-f8dc-4241-ad30-3960b0afa460","connectionId":"connection","createdAt":20.0,"epoch":3,"id":"8b359a1f-85cd-4d54-adfd-733c9c049f3c","method":"item/commandExecution/requestApproval","params":{"itemId":"preview","threadId":"thread-0","turnId":"turn-0"},"preview":{"changes":[{"kind":"update","path":"file.txt"}],"command":"printf review","cwd":"/fixture"},"status":"pending"}}',0);
INSERT INTO sync_entities VALUES('request','resolve-card',332,'b56fe424da372c2f0cee9848ee1276a9abb0095b37933d74f95c978c94ba06d0','{"collection":"request","id":"resolve-card","value":{}}',1);
INSERT INTO sync_entities VALUES('room','private-review',139,'389df7f888ea04f82fc1246c33d5e1cfffaebb460578b60737edeafde043c409','{"collection":"room","id":"private-review","value":{}}',1);
INSERT INTO sync_entities VALUES('request','python-written-card',138,'7c3aa5e1db8164aa80c7c540e2fd4c2cf05383fd12dee2ff6451c17fd75195ef','{"collection":"request","id":"python-written-card","value":{"accountKey":"default","agent":"abf56fbb-f8dc-4241-ad30-3960b0afa460","connectionId":"connection","createdAt":10.0,"epoch":3,"id":"python-written-card","method":"item/commandExecution/requestApproval","params":{"threadId":"thread-0","turnId":"turn-0"},"status":"pending"}}',0);
INSERT INTO sync_entities VALUES('monitor','6a1cbd51-e035-54ce-af5c-7fd9850e3065',131,'cb93790aad7c0f1615f856e7bd53a918c2832d8b46f3f7769137c5146bfa8cfe','{"collection":"monitor","id":"6a1cbd51-e035-54ce-af5c-7fd9850e3065","value":{"agent":"abf56fbb-f8dc-4241-ad30-3960b0afa460","bytes":0,"command":"printf fixture","created":19.0,"cwd":"__CALLBACK_STATE__","error":"Server restarted. Command outcome unknown; not rerun.","exitCode":null,"id":"6a1cbd51-e035-54ce-af5c-7fd9850e3065","interactive":false,"log":"__CALLBACK_STATE__/monitor-logs/6a1cbd51-e035-54ce-af5c-7fd9850e3065.log","ruleId":null,"status":"lost","tail":"","timeout_ms":3600000}}',0);
INSERT INTO sync_entities VALUES('event','monitor:6a1cbd51-e035-54ce-af5c-7fd9850e3065',132,'c83d84e17f36ac227468d22802128d966dcc7b8f60817af8bc74f556b678fd5c','{"collection":"event","id":"monitor:6a1cbd51-e035-54ce-af5c-7fd9850e3065","value":{"agent":"abf56fbb-f8dc-4241-ad30-3960b0afa460","created":1792684447.798625,"error":null,"id":"monitor:6a1cbd51-e035-54ce-af5c-7fd9850e3065","kind":"monitor_exit","status":"pending"}}',0);
INSERT INTO sync_entities VALUES('workspace','current',347,'4990594b804cce122f0625d1d43c3965e97b8b461de3dd0417b0e6baec9b9dac','{"collection":"workspace","id":"current","value":{"connected":false,"nativeNotices":[],"peerTeamsVersion":1,"projectOrganizationVersion":1,"sidebarOrder":{"groups":null,"revision":0},"stateDir":"__CALLBACK_STATE__","tasksHistoryLimit":100}}',0);
INSERT INTO sync_entities VALUES('request','roundtrip-0',143,'e5f99ef1a17519404a853ab3446797a9d3573b65255e1282c7793d8ecd76db52','{"collection":"request","id":"roundtrip-0","value":{}}',1);
INSERT INTO sync_entities VALUES('request','roundtrip-1',147,'ba4e0cf2a0dd76483aa802509c7cc19cd88f6b943f26d115b5d4e80741bbf677','{"collection":"request","id":"roundtrip-1","value":{}}',1);
INSERT INTO sync_entities VALUES('request','roundtrip-2',151,'e6f35819a7f32d00fc34b92a0ebbf3aff7945a25cc50451833b1210b1f67a02b','{"collection":"request","id":"roundtrip-2","value":{}}',1);
INSERT INTO sync_entities VALUES('request','roundtrip-3',155,'f24fc7b5a63a80f6d76a53b23c381345cf060e7a1807c731044c2f2012d6daae','{"collection":"request","id":"roundtrip-3","value":{}}',1);
INSERT INTO sync_entities VALUES('request','roundtrip-4',159,'e8cb8e7bd99f1518d2aeca1f6323ca9e354d77533dbf4572abab3e3ff4f4eadc','{"collection":"request","id":"roundtrip-4","value":{}}',1);
INSERT INTO sync_entities VALUES('request','roundtrip-5',163,'db4372257c17f77d6509a1b76ee59ba73a5b5b9da0c1b6886579570c582d0f10','{"collection":"request","id":"roundtrip-5","value":{}}',1);
INSERT INTO sync_entities VALUES('request','roundtrip-6',167,'f5c338dac04e38798669550ff7df4c317ab8918d72e0da96aaf3828dd27db1cd','{"collection":"request","id":"roundtrip-6","value":{}}',1);
INSERT INTO sync_entities VALUES('request','roundtrip-7',171,'dd8f160de7c898970358d0fba998015e7ccee63b1ef45e328945ee8a51f2d019','{"collection":"request","id":"roundtrip-7","value":{}}',1);
INSERT INTO sync_entities VALUES('request','roundtrip-8',175,'94339f9d5d6819cd698a1f1009a48bb190c01e3ec9ad2f411258596848b1e370','{"collection":"request","id":"roundtrip-8","value":{}}',1);
INSERT INTO sync_entities VALUES('request','roundtrip-9',179,'6a05945ca71ffaa5d4d82b74f8c35709c9ba71f494ecee308946cbd69580494d','{"collection":"request","id":"roundtrip-9","value":{}}',1);
INSERT INTO sync_entities VALUES('request','roundtrip-10',183,'c93e985c47b4851d9767d57bc6d8fc9408072180f772fcbe419c848a733a425e','{"collection":"request","id":"roundtrip-10","value":{}}',1);
INSERT INTO sync_entities VALUES('request','roundtrip-11',187,'da9e2420e3d81dedf05d8f55d19ecde1b2d16d80a3c5d04b8e3465525ed5f5ac','{"collection":"request","id":"roundtrip-11","value":{}}',1);
INSERT INTO sync_entities VALUES('request','roundtrip-12',191,'dccc301cfee87cdd7eb6a07b293555fa63bf96679199d8fe03d8bf36db3e0ae8','{"collection":"request","id":"roundtrip-12","value":{}}',1);
INSERT INTO sync_entities VALUES('request','roundtrip-13',195,'63b6a88de11e4c292fc5faa1b4a7fcd8eea1bea784569b02d3b5bf17500d06b2','{"collection":"request","id":"roundtrip-13","value":{}}',1);
INSERT INTO sync_entities VALUES('request','roundtrip-14',199,'1b6b860e2657531cf4cffe6f1381183eb4fc2df5d64ad0e2aefb3c470c8e3721','{"collection":"request","id":"roundtrip-14","value":{}}',1);
INSERT INTO sync_entities VALUES('request','roundtrip-15',203,'15706097b9ec83248a6928f6057874d0bb5c7616a0010792e55cccbe8ee0a1d9','{"collection":"request","id":"roundtrip-15","value":{}}',1);
INSERT INTO sync_entities VALUES('request','roundtrip-16',207,'069c86194173996773c7a047a7c5375ba30f8bc7d364c0561abac1991eb023d3','{"collection":"request","id":"roundtrip-16","value":{}}',1);
INSERT INTO sync_entities VALUES('request','roundtrip-17',211,'84d4940524927b1e7e4e746427a2fe98d5606e281a4468bd4bb929e8b108d6f9','{"collection":"request","id":"roundtrip-17","value":{}}',1);
INSERT INTO sync_entities VALUES('request','roundtrip-18',215,'4e4b9f076dce4bd2283dc12a09a8ca6115f32433325a7c69bc3053c8c138a28c','{"collection":"request","id":"roundtrip-18","value":{}}',1);
INSERT INTO sync_entities VALUES('request','roundtrip-19',219,'ee0381965eef1b6ee3c0ed8e24e33cfef1437352a821382f139b2f30e459346d','{"collection":"request","id":"roundtrip-19","value":{}}',1);
INSERT INTO sync_entities VALUES('request','roundtrip-20',223,'3b30f90432bc5daec8f0adf229dd3f1e3220ddeeea0f9d54da838e7996c28f43','{"collection":"request","id":"roundtrip-20","value":{}}',1);
INSERT INTO sync_entities VALUES('request','roundtrip-21',227,'2dd0e5a2b4b35cab8d104f2966f3b5c7a3558d1833dfe494fb213c32c3780129','{"collection":"request","id":"roundtrip-21","value":{}}',1);
INSERT INTO sync_entities VALUES('request','roundtrip-22',231,'44a40432b3bd084739d2aaa2bd141129fd4f333b214d5bb90cf67179be1d05be','{"collection":"request","id":"roundtrip-22","value":{}}',1);
INSERT INTO sync_entities VALUES('request','roundtrip-23',235,'69c3fd0357595ecda5675926e33d0d7d3007723eae5912b0b74ca305f61a1681','{"collection":"request","id":"roundtrip-23","value":{}}',1);
INSERT INTO sync_entities VALUES('request','roundtrip-24',239,'c17ee8cee3676dbeb4c9974c2687af6cc9bfd35ad2a486027877b46503fee4f8','{"collection":"request","id":"roundtrip-24","value":{}}',1);
INSERT INTO sync_entities VALUES('request','roundtrip-25',243,'1b1575151bca69a27ce49e25d4a601ec77bba8f9bc4b7abe771a813a46c9a298','{"collection":"request","id":"roundtrip-25","value":{}}',1);
INSERT INTO sync_entities VALUES('request','roundtrip-26',247,'144e1ac11ebdcc05fe671d81bb805dab96a24b7a4c9c5f1415df853fb9b5bf7b','{"collection":"request","id":"roundtrip-26","value":{}}',1);
INSERT INTO sync_entities VALUES('request','roundtrip-27',251,'a8f1d046dd7995d43dfe5b2eae2e589acb11c36784e00e5f7054a42acf5dff4f','{"collection":"request","id":"roundtrip-27","value":{}}',1);
INSERT INTO sync_entities VALUES('request','roundtrip-28',255,'3f8fb0757890f94eb5ecff4fed80522dd051ca44968e14655888913183c598cf','{"collection":"request","id":"roundtrip-28","value":{}}',1);
INSERT INTO sync_entities VALUES('request','roundtrip-29',259,'f4a222db69732fd86a56ef0dc9079c0c46aa2bf954c0fcfe080686522d96fe4a','{"collection":"request","id":"roundtrip-29","value":{}}',1);
INSERT INTO sync_entities VALUES('request','delete-0',263,'52b1d5de365561d74eae300c8aad5b466c93030326cb7a9a80d9bf771b9fa878','{"collection":"request","id":"delete-0","value":{}}',1);
INSERT INTO sync_entities VALUES('request','delete-1',264,'6e84191aebe6f2cce5305e159f1065ac7e73f5b857a0a8d6dffa9cda818e0129','{"collection":"request","id":"delete-1","value":{}}',1);
INSERT INTO sync_entities VALUES('request','delete-2',265,'21eb4b6cb0991ad4e9d0c580b04fe8d31f535297d287eecb15a5de21944fb1c5','{"collection":"request","id":"delete-2","value":{}}',1);
INSERT INTO sync_entities VALUES('request','delete-3',266,'0c8f017f809b5cdc90e549ef4f6522d1a4c66e611d970b25410c13691dd01db6','{"collection":"request","id":"delete-3","value":{}}',1);
INSERT INTO sync_entities VALUES('request','delete-4',267,'494af05404917313f0f11db8016531c71139637eec5fe16c918b899cf5882e0c','{"collection":"request","id":"delete-4","value":{}}',1);
INSERT INTO sync_entities VALUES('request','delete-5',268,'ba506f68c2cb4551631fe1869b91fc8da46c4cbaaa24da690999e17e36f4133d','{"collection":"request","id":"delete-5","value":{}}',1);
INSERT INTO sync_entities VALUES('request','delete-6',269,'472396bf0e7a502f7f633b6ace6c5f2a7189e781dfc82bee78dfee0ddb8f0627','{"collection":"request","id":"delete-6","value":{}}',1);
INSERT INTO sync_entities VALUES('request','delete-7',271,'32d923fc16f0bee09633cae5821a870f8bac2fa12e474a76b4c9a89a73c0b524','{"collection":"request","id":"delete-7","value":{}}',1);
INSERT INTO sync_entities VALUES('request','delete-8',274,'5a08bd2cb14f610fbdc6aa9622281b2929b6e2b91280ecb2ee37f2843ce2fbae','{"collection":"request","id":"delete-8","value":{}}',1);
INSERT INTO sync_entities VALUES('request','delete-9',275,'751815c30ec2cd6c5b5f72ee43a9c79b10967563ce2c4f48af4b59e6cffdb16e','{"collection":"request","id":"delete-9","value":{}}',1);
INSERT INTO sync_entities VALUES('request','delete-10',276,'3572f1f60b4c9567964bef8eadafcb8623ea1158058877fa88982bed55873af8','{"collection":"request","id":"delete-10","value":{}}',1);
INSERT INTO sync_entities VALUES('request','delete-11',277,'8f2fd6cc8575b8550dff5802e778798627c984359d148e1d87694ddaef46d8dd','{"collection":"request","id":"delete-11","value":{}}',1);
INSERT INTO sync_entities VALUES('request','delete-12',278,'4b587e1528f028a527c78912a77ea45688d2fa01caa4f2bbf76d4d4664cd780a','{"collection":"request","id":"delete-12","value":{}}',1);
INSERT INTO sync_entities VALUES('request','delete-13',279,'a45d52bb6c12aff6ae32a0707c94835cbbdc691d862c2b365b7a41747268c6a4','{"collection":"request","id":"delete-13","value":{}}',1);
INSERT INTO sync_entities VALUES('request','delete-14',281,'837cf366f6efc48fbaedb3b3e4275677af105e42eee7b554adcfae996b03c892','{"collection":"request","id":"delete-14","value":{}}',1);
INSERT INTO sync_entities VALUES('request','delete-15',284,'4dbce2ce0b3e5fae0772da06a3b090254d78c96a7c69a45802149ba4ce8a785c','{"collection":"request","id":"delete-15","value":{}}',1);
INSERT INTO sync_entities VALUES('request','delete-16',285,'5209cecc500cdadd801c3c38f9988a81a4a7041da53cfa0e2d61006d078f30db','{"collection":"request","id":"delete-16","value":{}}',1);
INSERT INTO sync_entities VALUES('request','delete-17',286,'97ee5d175d3fdbcc4399726b4949a57219d440659189c7d45fdba7d8c43abc19','{"collection":"request","id":"delete-17","value":{}}',1);
INSERT INTO sync_entities VALUES('request','delete-18',287,'2276d762cf2ede987a6e1985ce623fdaca5257c0157200126eab40127d864ecf','{"collection":"request","id":"delete-18","value":{}}',1);
INSERT INTO sync_entities VALUES('request','delete-19',288,'ff0291203091490b4fa5e4e709aee3efc3581455fc7ede59e5fb46f1e11741f5','{"collection":"request","id":"delete-19","value":{}}',1);
INSERT INTO sync_entities VALUES('request','delete-20',289,'61dcfe43d3196f73e6020ba9e80923d950d9dabcbc18936735855414d8cbad63','{"collection":"request","id":"delete-20","value":{}}',1);
INSERT INTO sync_entities VALUES('request','failure-answer-notSent',292,'25bd9046685c6831f2164245ee418dd698c7ce7cd9e38066e7ec3b64137ff75c','{"collection":"request","id":"failure-answer-notSent","value":{"accountKey":"default","agent":"abf56fbb-f8dc-4241-ad30-3960b0afa460","connectionId":"connection","createdAt":10.0,"epoch":3,"id":"failure-answer-notSent","method":"item/tool/requestUserInput","params":{"questions":[],"threadId":"thread-0","turnId":"turn-0"},"status":"pending"}}',0);
INSERT INTO sync_entities VALUES('request','failure-answer-unknown',294,'1ed647a88d4bdbca68cc9d3d716083d9fc75675ba447e5fa5581848451801b68','{"collection":"request","id":"failure-answer-unknown","value":{}}',1);
INSERT INTO sync_entities VALUES('request','failure-delete-notSent',297,'b2e9c27c2f827c8acd7661d5abc3c7e4920e044bb6573d665294d621cc354e03','{"collection":"request","id":"failure-delete-notSent","value":{"accountKey":"default","agent":"abf56fbb-f8dc-4241-ad30-3960b0afa460","connectionId":"connection","createdAt":10.0,"epoch":3,"id":"failure-delete-notSent","method":"item/tool/requestUserInput","params":{"questions":[],"threadId":"thread-0","turnId":"turn-0"},"status":"pending"}}',0);
INSERT INTO sync_entities VALUES('request','failure-delete-unknown',299,'22332fdf971cce5670cbf0af80ac0edd66c290601aff3e7bb2b4ea05e678e0b3','{"collection":"request","id":"failure-delete-unknown","value":{}}',1);
INSERT INTO sync_entities VALUES('request','invalid-0',300,'05d6cb87c4ebb8e149b5224eadafda809f348fb341e89200a07173d1704c61dd','{"collection":"request","id":"invalid-0","value":{"accountKey":"default","agent":"abf56fbb-f8dc-4241-ad30-3960b0afa460","connectionId":"connection","createdAt":10.0,"epoch":3,"id":"invalid-0","method":"item/commandExecution/requestApproval","params":{"threadId":"thread-0","turnId":"turn-0"},"status":"pending"}}',0);
INSERT INTO sync_entities VALUES('request','invalid-1',301,'cf930ac01b7a68c1ba992b02fc37fc13188d5963a7097ab8ad1083ecc380a10e','{"collection":"request","id":"invalid-1","value":{"accountKey":"default","agent":"abf56fbb-f8dc-4241-ad30-3960b0afa460","connectionId":"connection","createdAt":10.0,"epoch":3,"id":"invalid-1","method":"item/commandExecution/requestApproval","params":{"threadId":"thread-0","turnId":"turn-0"},"status":"pending"}}',0);
INSERT INTO sync_entities VALUES('request','invalid-2',302,'0d6e8d71e4f0af466b8f05e9a17b2ff560409f4d511e5e17c6fb9ad98e971c28','{"collection":"request","id":"invalid-2","value":{"accountKey":"default","agent":"abf56fbb-f8dc-4241-ad30-3960b0afa460","connectionId":"connection","createdAt":10.0,"epoch":3,"id":"invalid-2","method":"item/commandExecution/requestApproval","params":{"threadId":"thread-0","turnId":"turn-0"},"status":"pending"}}',0);
INSERT INTO sync_entities VALUES('request','invalid-3',303,'1f6604d38254bee37b2f86548254d7703af265bc6436f39d04786da9850673a9','{"collection":"request","id":"invalid-3","value":{"accountKey":"default","agent":"abf56fbb-f8dc-4241-ad30-3960b0afa460","connectionId":"connection","createdAt":10.0,"epoch":3,"id":"invalid-3","method":"item/fileChange/requestApproval","params":{"threadId":"thread-0","turnId":"turn-0"},"status":"pending"}}',0);
INSERT INTO sync_entities VALUES('request','invalid-4',304,'6c03cb29f15e79e43bde42695ad147a70c81487fa3474801c810d0520c36ffe8','{"collection":"request","id":"invalid-4","value":{"accountKey":"default","agent":"abf56fbb-f8dc-4241-ad30-3960b0afa460","connectionId":"connection","createdAt":10.0,"epoch":3,"id":"invalid-4","method":"item/fileChange/requestApproval","params":{"threadId":"thread-0","turnId":"turn-0"},"status":"pending"}}',0);
INSERT INTO sync_entities VALUES('request','invalid-5',305,'a74d5cba144e138d20dbe7f092aec823b2908202e2224516afb23cca333082a6','{"collection":"request","id":"invalid-5","value":{"accountKey":"default","agent":"abf56fbb-f8dc-4241-ad30-3960b0afa460","connectionId":"connection","createdAt":10.0,"epoch":3,"id":"invalid-5","method":"item/fileChange/requestApproval","params":{"threadId":"thread-0","turnId":"turn-0"},"status":"pending"}}',0);
INSERT INTO sync_entities VALUES('request','invalid-6',307,'0b0a7ad6adabb0179fb323909e2f440f9a86646995c13fc48dc3503a02c77903','{"collection":"request","id":"invalid-6","value":{}}',1);
INSERT INTO sync_entities VALUES('request','invalid-7',310,'06ae7dcddd9ebc0a4b601dace4550f7d5f6e2e2510f695ab9ea3dfcca81ad9eb','{"collection":"request","id":"invalid-7","value":{"accountKey":"default","agent":"abf56fbb-f8dc-4241-ad30-3960b0afa460","connectionId":"connection","createdAt":10.0,"epoch":3,"id":"invalid-7","method":"mcpServer/elicitation/request","params":{"threadId":"thread-0","turnId":"turn-0"},"status":"pending"}}',0);
INSERT INTO sync_entities VALUES('request','invalid-8',311,'793a73b6ac4c1199699cc4776b116fb5b2bc51d4c79eeabfb49255794d72d48e','{"collection":"request","id":"invalid-8","value":{"accountKey":"default","agent":"abf56fbb-f8dc-4241-ad30-3960b0afa460","connectionId":"connection","createdAt":10.0,"epoch":3,"id":"invalid-8","method":"mcpServer/elicitation/request","params":{"threadId":"thread-0","turnId":"turn-0"},"status":"pending"}}',0);
INSERT INTO sync_entities VALUES('request','invalid-9',312,'219a279c94e1b17e8ccd06eb6efa1bd17c161e8646c0e2826b8928306f010a9b','{"collection":"request","id":"invalid-9","value":{"accountKey":"default","agent":"abf56fbb-f8dc-4241-ad30-3960b0afa460","connectionId":"connection","createdAt":10.0,"epoch":3,"id":"invalid-9","method":"item/tool/requestUserInput","params":{"threadId":"thread-0","turnId":"turn-0"},"status":"pending"}}',0);
INSERT INTO sync_entities VALUES('request','invalid-10',313,'6f6d0be9e0466596d28ecf4935332ba9c060a65b7c6d024c5c3f060f23794609','{"collection":"request","id":"invalid-10","value":{"accountKey":"default","agent":"abf56fbb-f8dc-4241-ad30-3960b0afa460","connectionId":"connection","createdAt":10.0,"epoch":3,"id":"invalid-10","method":"item/tool/requestUserInput","params":{"threadId":"thread-0","turnId":"turn-0"},"status":"pending"}}',0);
INSERT INTO sync_entities VALUES('request','invalid-11',315,'d2746eb775e3cfd0c343de1ee142d9afcb2285d1b1b0bad61888849abb4a2a96','{"collection":"request","id":"invalid-11","value":{}}',1);
INSERT INTO sync_entities VALUES('request','race-completed',335,'f4b7fcf436dea35407957546e329c1859bf736c98d54ac9a1e4fd6d8202ca621','{"collection":"request","id":"race-completed","value":{}}',1);
INSERT INTO sync_entities VALUES('request','race-failed',340,'5f99c703789614fbb4796c7b7f32b6b7002cb2271a2114b0d83256db33ab4f13','{"collection":"request","id":"race-failed","value":{}}',1);
INSERT INTO sync_entities VALUES('request','race-resolved',345,'aeeef4714265a62cd15508d6e99cb36f817fb6361a552d5bac83b94d46f64667','{"collection":"request","id":"race-resolved","value":{}}',1);
CREATE TABLE sync_entity_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
INSERT INTO sync_entity_meta VALUES('entity_tombstone_count','91');
INSERT INTO sync_entity_meta VALUES('event_window_seq','132');
INSERT INTO sync_entity_meta VALUES('task_window_migrated','1');
INSERT INTO sync_entity_meta VALUES('seeded','1');
INSERT INTO sync_entity_meta VALUES('agent_organization_fields','2');
CREATE TABLE sync_generation (id INTEGER PRIMARY KEY, value INTEGER NOT NULL);
INSERT INTO sync_generation VALUES(1,0);
CREATE TABLE sync_identity (id TEXT PRIMARY KEY);
INSERT INTO sync_identity VALUES('0257042456d74b1d98d79602ce870d31');
CREATE TABLE IF NOT EXISTS "runtime_agents"(id TEXT PRIMARY KEY,"root_id" TEXT GENERATED ALWAYS AS (record->>'$.rootId') STORED ,"parent_id" TEXT GENERATED ALWAYS AS (record->>'$.parentId') STORED ,"status" TEXT GENERATED ALWAYS AS (record->>'$.status') STORED ,"account_key" TEXT GENERATED ALWAYS AS (COALESCE(record->>'$.accountKey','default')) STORED NOT NULL,"provider" TEXT GENERATED ALWAYS AS (record->>'$.provider') STORED NOT NULL,"role" TEXT GENERATED ALWAYS AS (record->>'$.role') STORED ,"deleted_at"  GENERATED ALWAYS AS (record->>'$.deletedAt') STORED ,"thread_id" TEXT GENERATED ALWAYS AS (record->>'$.threadId') STORED ,"turn_id" TEXT GENERATED ALWAYS AS (record->>'$.turnId') STORED ,"cwd" TEXT GENERATED ALWAYS AS (record->>'$.cwd') STORED ,"workspace_operation"  GENERATED ALWAYS AS (record->>'$.workspaceOperation') STORED ,"in_flight" INTEGER GENERATED ALWAYS AS (record->>'$.inFlight') STORED ,"auto_wake" INTEGER GENERATED ALWAYS AS (record->>'$.autoWake') STORED ,"remote_worker"  GENERATED ALWAYS AS (record->>'$.remoteWorker') STORED ,"remote_anchor"  GENERATED ALWAYS AS (record->>'$.remoteAnchor') STORED ,"native_failure_hold"  GENERATED ALWAYS AS (record->>'$.nativeFailureHold') STORED ,"steer_rejected_turn_id" TEXT GENERATED ALWAYS AS (record->>'$.steerRejectedTurnId') STORED ,"execution_mode" TEXT GENERATED ALWAYS AS (record->>'$.executionMode') STORED ,"native_tool_refresh_id" TEXT GENERATED ALWAYS AS (record->>'$.nativeToolRefreshId') STORED ,"context_repair_phase" TEXT GENERATED ALWAYS AS (record->>'$.contextRepair.phase') STORED ,"source_cleanup_phase" TEXT GENERATED ALWAYS AS (record->>'$.contextRepair.sourceCleanup.phase') STORED ,"source_cleanup_account_key" TEXT GENERATED ALWAYS AS (record->>'$.contextRepair.sourceCleanup.accountKey') STORED ,"source_cleanup_thread_id" TEXT GENERATED ALWAYS AS (record->>'$.contextRepair.sourceCleanup.threadId') STORED ,"has_disconnect_recovery" INTEGER GENERATED ALWAYS AS (COALESCE(json_type(record,'$.disconnectRecovery')='object',0)) STORED NOT NULL,"disconnect_recovery_stage" TEXT GENERATED ALWAYS AS (record->>'$.disconnectRecovery.stage') STORED ,"has_restart_recovery" INTEGER GENERATED ALWAYS AS (COALESCE(json_type(record,'$.restartRecovery')='object',0)) STORED NOT NULL,"restart_recovery_stage" TEXT GENERATED ALWAYS AS (record->>'$.restartRecovery.stage') STORED ,"has_workspace_cleanup_intent" INTEGER GENERATED ALWAYS AS (COALESCE(json_type(record,'$.workspaceCleanupIntent')='object',0)) STORED NOT NULL,"agent_archived" INTEGER GENERATED ALWAYS AS (NOT (json_type(record,'$.agentArchive') IS NULL OR json_type(record,'$.agentArchive') IN ('null','false') OR (json_type(record,'$.agentArchive') IN ('integer','real') AND record->>'$.agentArchive'=0) OR (json_type(record,'$.agentArchive')='text' AND record->>'$.agentArchive'='') OR (json_type(record,'$.agentArchive')='array' AND json_array_length(record,'$.agentArchive')=0) OR (json_type(record,'$.agentArchive')='object' AND record->>'$.agentArchive'='{}'))) STORED NOT NULL,"review_archive_failed" INTEGER GENERATED ALWAYS AS (NOT (json_type(record,'$.reviewArchiveError') IS NULL OR json_type(record,'$.reviewArchiveError') IN ('null','false') OR (json_type(record,'$.reviewArchiveError') IN ('integer','real') AND record->>'$.reviewArchiveError'=0) OR (json_type(record,'$.reviewArchiveError')='text' AND record->>'$.reviewArchiveError'='') OR (json_type(record,'$.reviewArchiveError')='array' AND json_array_length(record,'$.reviewArchiveError')=0) OR (json_type(record,'$.reviewArchiveError')='object' AND record->>'$.reviewArchiveError'='{}'))) STORED NOT NULL,"review_archive_marked" INTEGER GENERATED ALWAYS AS (record->>'$.reviewArchiveScheduled' IS NOT NULL) STORED NOT NULL,"review_archive_scheduled" INTEGER GENERATED ALWAYS AS (COALESCE(json_type(record,'$.reviewArchiveScheduled')='text' AND record->>'$.reviewArchiveScheduled'<>'',0)) STORED NOT NULL,"review_archive_next_at"  GENERATED ALWAYS AS (record->>'$.reviewArchiveNextAt') STORED ,"idle_episode_type" TEXT GENERATED ALWAYS AS (json_type(record,'$.idleCompactionEpisode')) STORED ,"idle_episode_fire_at"  GENERATED ALWAYS AS (record->>'$.idleCompactionEpisode.fireAt') STORED ,"idle_episode_id"  GENERATED ALWAYS AS (record->>'$.idleCompactionEpisode.id') STORED ,"legacy_unsettled" INTEGER GENERATED ALWAYS AS (COALESCE(record->>'$.workspaceMode'='image' OR record->>'$.imageWorkspace'=1,0) AND COALESCE(record->>'$.legacyWorkspaceSettled',0)=0) STORED NOT NULL,record TEXT NOT NULL,hold_status TEXT NOT NULL DEFAULT 'clear' CHECK("hold_status" IN ('clear','invalid','unrecognized')),hold_incident TEXT,title_status TEXT GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN record->>'$.titleJob.status' END) VIRTUAL CHECK("title_status" IN ('scheduled','written','invalid','discarded','pending','submitted','unknown','not_sent','applied','failed')),vm_launch_status TEXT GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN record->>'$.vmLaunch.status' END) VIRTUAL CHECK("vm_launch_status" IN ('completed','terminal','pending','retryable','submitted','unknown','not_sent','complete','applied','failed','cancelled')),idle_episode_outcome TEXT GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN record->>'$.idleCompactionEpisode.outcome' END) VIRTUAL CHECK("idle_episode_outcome" IN ('compacted','disabled','compaction_off','idle_compaction_off','prefix_changed','started','dispatched','failed','ttl_changed','lapsed','small','newer_request','near_limit','active','queued_input','stopped','no_host','would_compact')),provider_stream_generation INTEGER GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN record->>'$.providerStream.generation' END) VIRTUAL,provider_stream_ack INTEGER GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN record->>'$.providerStream.ack' END) VIRTUAL);
INSERT INTO runtime_agents VALUES('abf56fbb-f8dc-4241-ad30-3960b0afa460','{"accountKey":"default","activeExecution":{"attemptId":null,"runId":"run:18c363e5-2b4a-57a2-a086-76339fa02f4e"},"agentMode":"multi","agentModeRevision":0,"agentModeSupported":true,"autoWake":true,"compactions":0,"compactionsObservedOnly":false,"concurrency":32,"created":1792684446.7842586,"cwd":"__CALLBACK_STATE__","cyberAccessProgram":"standard","daybreakEnabled":false,"effort":"medium","environment":"host","epoch":3,"error":null,"executionMode":"native","executionSettingsAccountKey":"default","fastMode":false,"hostProjectPath":"__CALLBACK_STATE__","id":"abf56fbb-f8dc-4241-ad30-3960b0afa460","inFlight":false,"isLead":true,"layrReady":false,"maxAgents":64,"maxAgentsExplicit":false,"model":"gpt-6-astra","name":"Callback lead","needsTitle":false,"parentId":null,"profileId":null,"profileInstructions":"","prompt":"fixture","provider":"codex","reviewDefaults":{"effort":null,"model":null},"role":"orchestrator","rootId":"abf56fbb-f8dc-4241-ad30-3960b0afa460","status":"running","subagentConcurrencyVersion":2,"threadId":"thread-0","tokenBudget":null,"turnEpoch":3,"turnId":"turn-0","usageResumeEnabled":true,"workerBaseBehindMain":null,"workerBaseCommit":null,"workerBaseMainRef":null,"workerBaseRef":null,"workerDefaults":{"daybreakEnabled":false,"delegationDepth":1,"effort":"high","fastMode":false,"model":"gpt-6-luna"},"workspaceBackend":null,"workspaceInGit":false,"workspaceMode":"worktree","workspaceModeExplicit":false,"worktree":false,"worktreeReady":false,"worktreeWarning":null,"yoloMode":true}','clear',NULL);
INSERT INTO runtime_agents VALUES('3e73e736-e287-42b7-963c-6bef9312a291','{"accountKey":"default","activeExecution":{"attemptId":null,"runId":"run:ca8cf778-ff24-526e-82db-707ddddaf9f8"},"agentMode":"multi","agentModeRevision":0,"agentModeSupported":true,"autoWake":true,"compactions":0,"compactionsObservedOnly":false,"concurrency":32,"created":1792684446.7901473,"cwd":"__CALLBACK_STATE__","cyberAccessProgram":"standard","daybreakEnabled":false,"effort":"medium","environment":"host","epoch":3,"error":null,"executionMode":"native","executionSettingsAccountKey":"default","fastMode":false,"hostProjectPath":"__CALLBACK_STATE__","id":"3e73e736-e287-42b7-963c-6bef9312a291","inFlight":false,"isLead":false,"layrReady":false,"maxAgents":64,"maxAgentsExplicit":false,"model":"gpt-6-astra","name":"Callback worker","needsTitle":false,"parentId":"abf56fbb-f8dc-4241-ad30-3960b0afa460","profileId":null,"profileInstructions":"","prompt":"fixture","provider":"codex","reviewDefaults":{"effort":null,"model":null},"role":"orchestrator","rootId":"abf56fbb-f8dc-4241-ad30-3960b0afa460","status":"running","subagentConcurrencyVersion":2,"threadId":"thread-1","tokenBudget":null,"turnEpoch":3,"turnId":"turn-1","usageResumeEnabled":true,"workerBaseBehindMain":null,"workerBaseCommit":null,"workerBaseMainRef":null,"workerBaseRef":null,"workerDefaults":{"daybreakEnabled":false,"delegationDepth":1,"effort":"high","fastMode":false,"model":"gpt-6-luna"},"workspaceBackend":null,"workspaceInGit":false,"workspaceMode":"worktree","workspaceModeExplicit":false,"worktree":false,"worktreeReady":false,"worktreeWarning":null,"yoloMode":true}','clear',NULL);
INSERT INTO runtime_agents VALUES('7c2692e5-4303-49f4-8ba0-cdc5a6f1db4a','{"accountKey":"default","activeExecution":{"attemptId":null,"runId":"run:b334b0de-1363-5393-906f-3d49e1e9ab8b"},"agentMode":"multi","agentModeRevision":0,"agentModeSupported":true,"autoWake":true,"compactions":0,"compactionsObservedOnly":false,"concurrency":32,"created":1792684446.7960482,"cwd":"__CALLBACK_STATE__","cyberAccessProgram":"standard","daybreakEnabled":false,"effort":"medium","environment":"host","epoch":3,"error":null,"executionMode":"native","executionSettingsAccountKey":"default","fastMode":false,"hostProjectPath":"__CALLBACK_STATE__","id":"7c2692e5-4303-49f4-8ba0-cdc5a6f1db4a","inFlight":false,"isLead":true,"layrReady":false,"maxAgents":64,"maxAgentsExplicit":false,"model":"gpt-6-astra","name":"Other team","needsTitle":false,"parentId":null,"profileId":null,"profileInstructions":"","prompt":"fixture","provider":"codex","reviewDefaults":{"effort":null,"model":null},"role":"orchestrator","rootId":"7c2692e5-4303-49f4-8ba0-cdc5a6f1db4a","status":"running","subagentConcurrencyVersion":2,"threadId":"thread-2","tokenBudget":null,"turnEpoch":3,"turnId":"turn-2","usageResumeEnabled":true,"workerBaseBehindMain":null,"workerBaseCommit":null,"workerBaseMainRef":null,"workerBaseRef":null,"workerDefaults":{"daybreakEnabled":false,"delegationDepth":1,"effort":"high","fastMode":false,"model":"gpt-6-luna"},"workspaceBackend":null,"workspaceInGit":false,"workspaceMode":"worktree","workspaceModeExplicit":false,"worktree":false,"worktreeReady":false,"worktreeWarning":null,"yoloMode":true}','clear',NULL);
INSERT INTO runtime_agents VALUES('aaaaaaaa-1111-4111-8111-111111111111','{"accountKey":"default","activeExecution":{"attemptId":null,"runId":"run:3d30ec42-08b4-5705-a9f1-265b1e23298c"},"agentMode":"multi","agentModeRevision":0,"agentModeSupported":true,"autoWake":false,"compactions":0,"compactionsObservedOnly":false,"created":1792684446.7901473,"cwd":"__CALLBACK_STATE__","cyberAccessProgram":"standard","daybreakEnabled":false,"deletedAt":null,"disconnectRecovery":{"accountKey":"default","at":1792684447.7965105,"autoWake":true,"epoch":3,"source":"restart","threadId":"thread-1","turnId":"turn-1"},"effort":"medium","environment":"host","epoch":3,"error":"Server restarted during a turn. Review history, then send a new instruction.","executionMode":"native","executionSettingsAccountKey":"default","fastMode":false,"hostProjectPath":"__CALLBACK_STATE__","id":"aaaaaaaa-1111-4111-8111-111111111111","inFlight":false,"isLead":false,"layrReady":false,"maxAgents":64,"maxAgentsExplicit":false,"model":"gpt-6-astra","name":"Callback worker","needsTitle":false,"parentId":"abf56fbb-f8dc-4241-ad30-3960b0afa460","profileId":null,"profileInstructions":"","prompt":"fixture","provider":"codex","restartRecovery":{"accountKey":"default","at":1792684447.7965105,"autoWake":true,"epoch":3,"stage":"pending","startAttempt":null,"threadId":"thread-1","turnId":"turn-1"},"reviewDefaults":{"effort":null,"model":null},"role":"orchestrator","rootId":"abf56fbb-f8dc-4241-ad30-3960b0afa460","status":"interrupted","subagentConcurrencyVersion":2,"supervisorRestore":{"accountKey":"default","at":1792684447.8197367,"reason":"supervisor_mode_disabled","status":"not_restored"},"threadId":"thread-1","tokenBudget":null,"turnEpoch":3,"turnId":"turn-1","usageResumeEnabled":true,"workerBaseBehindMain":null,"workerBaseCommit":null,"workerBaseMainRef":null,"workerBaseRef":null,"workerDefaults":{"daybreakEnabled":false,"delegationDepth":1,"effort":"high","fastMode":false,"model":"gpt-6-luna"},"workspaceBackend":null,"workspaceInGit":false,"workspaceMode":"worktree","workspaceModeExplicit":false,"worktree":false,"worktreeReady":false,"worktreeWarning":null,"yoloMode":true}','clear',NULL);
INSERT INTO runtime_agents VALUES('aaaaaaaa-2222-4222-8222-222222222222','{"accountKey":"default","activeExecution":{"attemptId":null,"runId":"run:cd8957f8-27ab-5eef-a9c3-5a02a523ef8b"},"agentMode":"multi","agentModeRevision":0,"agentModeSupported":true,"autoWake":false,"compactions":0,"compactionsObservedOnly":false,"created":1792684446.7901473,"cwd":"__CALLBACK_STATE__","cyberAccessProgram":"standard","daybreakEnabled":false,"deletedAt":null,"disconnectRecovery":{"accountKey":"default","at":1792684447.7969077,"autoWake":true,"epoch":3,"source":"restart","threadId":"thread-1","turnId":"turn-1"},"effort":"medium","environment":"host","epoch":3,"error":"Server restarted during a turn. Review history, then send a new instruction.","executionMode":"native","executionSettingsAccountKey":"default","fastMode":false,"hostProjectPath":"__CALLBACK_STATE__","id":"aaaaaaaa-2222-4222-8222-222222222222","inFlight":false,"isLead":false,"layrReady":false,"maxAgents":64,"maxAgentsExplicit":false,"model":"gpt-6-astra","name":"Callback worker","needsTitle":false,"parentId":"abf56fbb-f8dc-4241-ad30-3960b0afa460","profileId":null,"profileInstructions":"","prompt":"fixture","provider":"codex","restartRecovery":{"accountKey":"default","at":1792684447.7969077,"autoWake":true,"epoch":3,"stage":"pending","startAttempt":null,"threadId":"thread-1","turnId":"turn-1"},"reviewDefaults":{"effort":null,"model":null},"role":"orchestrator","rootId":"abf56fbb-f8dc-4241-ad30-3960b0afa460","status":"interrupted","subagentConcurrencyVersion":2,"supervisorRestore":{"accountKey":"default","at":1792684447.8197367,"reason":"supervisor_mode_disabled","status":"not_restored"},"threadId":"thread-1","tokenBudget":null,"turnEpoch":3,"turnId":"turn-1","usageResumeEnabled":true,"workerBaseBehindMain":null,"workerBaseCommit":null,"workerBaseMainRef":null,"workerBaseRef":null,"workerDefaults":{"daybreakEnabled":false,"delegationDepth":1,"effort":"high","fastMode":false,"model":"gpt-6-luna"},"workspaceBackend":null,"workspaceInGit":false,"workspaceMode":"worktree","workspaceModeExplicit":false,"worktree":false,"worktreeReady":false,"worktreeWarning":null,"yoloMode":true}','clear',NULL);
INSERT INTO runtime_agents VALUES('bbbbbbbb-1111-4111-8111-111111111111','{"accountKey":"default","activeExecution":{"attemptId":null,"runId":"run:3c0493c4-e9bc-5726-b0e6-6232cb0a370b"},"agentMode":"multi","agentModeRevision":0,"agentModeSupported":true,"autoWake":false,"compactions":0,"compactionsObservedOnly":false,"concurrency":32,"created":1792684446.7960482,"cwd":"__CALLBACK_STATE__","cyberAccessProgram":"standard","daybreakEnabled":false,"deletedAt":null,"disconnectRecovery":{"accountKey":"default","at":1792684447.7972267,"autoWake":true,"epoch":3,"source":"restart","threadId":"thread-2","turnId":"turn-2"},"effort":"medium","environment":"host","epoch":3,"error":"Server restarted during a turn. Review history, then send a new instruction.","executionMode":"native","executionSettingsAccountKey":"default","fastMode":false,"hostProjectPath":"__CALLBACK_STATE__","id":"bbbbbbbb-1111-4111-8111-111111111111","inFlight":false,"isLead":true,"layrReady":false,"maxAgents":64,"maxAgentsExplicit":false,"model":"gpt-6-astra","name":"Other team","needsTitle":false,"parentId":null,"profileId":null,"profileInstructions":"","prompt":"fixture","provider":"codex","restartRecovery":{"accountKey":"default","at":1792684447.7972267,"autoWake":true,"epoch":3,"stage":"pending","startAttempt":null,"threadId":"thread-2","turnId":"turn-2"},"reviewDefaults":{"effort":null,"model":null},"role":"orchestrator","rootId":"bbbbbbbb-1111-4111-8111-111111111111","status":"interrupted","subagentConcurrencyVersion":2,"supervisorRestore":{"accountKey":"default","at":1792684447.8197367,"reason":"supervisor_mode_disabled","status":"not_restored"},"threadId":"thread-2","tokenBudget":null,"turnEpoch":3,"turnId":"turn-2","usageResumeEnabled":true,"workerBaseBehindMain":null,"workerBaseCommit":null,"workerBaseMainRef":null,"workerBaseRef":null,"workerDefaults":{"daybreakEnabled":false,"delegationDepth":1,"effort":"high","fastMode":false,"model":"gpt-6-luna"},"workspaceBackend":null,"workspaceInGit":false,"workspaceMode":"worktree","workspaceModeExplicit":false,"worktree":false,"worktreeReady":false,"worktreeWarning":null,"yoloMode":true}','clear',NULL);
INSERT INTO runtime_agents VALUES('bbbbbbbb-2222-4222-8222-222222222222','{"accountKey":"default","activeExecution":{"attemptId":null,"runId":"run:baaa4bf8-d7e3-5029-a6e4-ee9bead0baf9"},"agentMode":"multi","agentModeRevision":0,"agentModeSupported":true,"autoWake":false,"compactions":0,"compactionsObservedOnly":false,"created":1792684446.7901473,"cwd":"__CALLBACK_STATE__","cyberAccessProgram":"standard","daybreakEnabled":false,"deletedAt":null,"disconnectRecovery":{"accountKey":"default","at":1792684447.797499,"autoWake":true,"epoch":3,"source":"restart","threadId":"thread-1","turnId":"turn-1"},"effort":"medium","environment":"host","epoch":3,"error":"Server restarted during a turn. Review history, then send a new instruction.","executionMode":"native","executionSettingsAccountKey":"default","fastMode":false,"hostProjectPath":"__CALLBACK_STATE__","id":"bbbbbbbb-2222-4222-8222-222222222222","inFlight":false,"isLead":false,"layrReady":false,"maxAgents":64,"maxAgentsExplicit":false,"model":"gpt-6-astra","name":"Callback worker","needsTitle":false,"parentId":"abf56fbb-f8dc-4241-ad30-3960b0afa460","profileId":null,"profileInstructions":"","prompt":"fixture","provider":"codex","restartRecovery":{"accountKey":"default","at":1792684447.797499,"autoWake":true,"epoch":3,"stage":"pending","startAttempt":null,"threadId":"thread-1","turnId":"turn-1"},"reviewDefaults":{"effort":null,"model":null},"role":"orchestrator","rootId":"abf56fbb-f8dc-4241-ad30-3960b0afa460","status":"interrupted","subagentConcurrencyVersion":2,"supervisorRestore":{"accountKey":"default","at":1792684447.8197367,"reason":"supervisor_mode_disabled","status":"not_restored"},"threadId":"thread-1","tokenBudget":null,"turnEpoch":3,"turnId":"turn-1","usageResumeEnabled":true,"workerBaseBehindMain":null,"workerBaseCommit":null,"workerBaseMainRef":null,"workerBaseRef":null,"workerDefaults":{"daybreakEnabled":false,"delegationDepth":1,"effort":"high","fastMode":false,"model":"gpt-6-luna"},"workspaceBackend":null,"workspaceInGit":false,"workspaceMode":"worktree","workspaceModeExplicit":false,"worktree":false,"worktreeReady":false,"worktreeWarning":null,"yoloMode":true}','clear',NULL);
INSERT INTO runtime_agents VALUES('cccccccc-1111-4111-8111-111111111111','{"accountKey":"default","activeExecution":{"attemptId":null,"runId":"run:6cbe02fd-b7fd-5704-9198-1be59eb38284"},"agentMode":"multi","agentModeRevision":0,"agentModeSupported":true,"autoWake":false,"compactions":0,"compactionsObservedOnly":false,"created":1792684446.7901473,"cwd":"__CALLBACK_STATE__","cyberAccessProgram":"standard","daybreakEnabled":false,"deletedAt":15.0,"effort":"medium","environment":"host","epoch":3,"error":"Server restarted during a turn. Review history, then send a new instruction.","executionMode":"native","executionSettingsAccountKey":"default","fastMode":false,"hostProjectPath":"__CALLBACK_STATE__","id":"cccccccc-1111-4111-8111-111111111111","inFlight":false,"isLead":false,"layrReady":false,"maxAgents":64,"maxAgentsExplicit":false,"model":"gpt-6-astra","name":"Callback worker","needsTitle":false,"parentId":"abf56fbb-f8dc-4241-ad30-3960b0afa460","profileId":null,"profileInstructions":"","prompt":"fixture","provider":"codex","reviewDefaults":{"effort":null,"model":null},"role":"orchestrator","rootId":"abf56fbb-f8dc-4241-ad30-3960b0afa460","status":"interrupted","subagentConcurrencyVersion":2,"threadId":"thread-1","tokenBudget":null,"turnEpoch":3,"turnId":"turn-1","usageResumeEnabled":true,"workerBaseBehindMain":null,"workerBaseCommit":null,"workerBaseMainRef":null,"workerBaseRef":null,"workerDefaults":{"daybreakEnabled":false,"delegationDepth":1,"effort":"high","fastMode":false,"model":"gpt-6-luna"},"workspaceBackend":null,"workspaceInGit":false,"workspaceMode":"worktree","workspaceModeExplicit":false,"worktree":false,"worktreeReady":false,"worktreeWarning":null,"yoloMode":true}','clear',NULL);
CREATE TABLE durable_rows (
        tbl TEXT NOT NULL, id TEXT NOT NULL, seq INTEGER NOT NULL,
        deleted INTEGER NOT NULL DEFAULT 0, at INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY(tbl, id));
INSERT INTO durable_rows VALUES('runtime_agents','abf56fbb-f8dc-4241-ad30-3960b0afa460',356,0,0);
INSERT INTO durable_rows VALUES('runtime_agents','3e73e736-e287-42b7-963c-6bef9312a291',357,0,0);
INSERT INTO durable_rows VALUES('runtime_agents','7c2692e5-4303-49f4-8ba0-cdc5a6f1db4a',358,0,0);
INSERT INTO durable_rows VALUES('runtime_agents','aaaaaaaa-1111-4111-8111-111111111111',359,0,0);
INSERT INTO durable_rows VALUES('runtime_agents','aaaaaaaa-2222-4222-8222-222222222222',360,0,0);
INSERT INTO durable_rows VALUES('runtime_agents','bbbbbbbb-1111-4111-8111-111111111111',361,0,0);
INSERT INTO durable_rows VALUES('runtime_agents','bbbbbbbb-2222-4222-8222-222222222222',362,0,0);
INSERT INTO durable_rows VALUES('runtime_agents','cccccccc-1111-4111-8111-111111111111',363,0,0);
CREATE TABLE durable_floor (
        id INTEGER PRIMARY KEY, floor INTEGER NOT NULL);
INSERT INTO durable_floor VALUES(1,0);
CREATE TABLE runtime_agent_volatile(id TEXT PRIMARY KEY,"events" TEXT,"lastEvent" TEXT,"activity" TEXT,"activeTools" TEXT,"tail" TEXT,"lastAnswer" TEXT,"contextUsage" TEXT,"tokensUsed" TEXT,"tokenUsageAccounting" TEXT);
INSERT INTO runtime_agent_volatile VALUES('abf56fbb-f8dc-4241-ad30-3960b0afa460','0',NULL,NULL,NULL,'""',NULL,NULL,'0',NULL);
INSERT INTO runtime_agent_volatile VALUES('3e73e736-e287-42b7-963c-6bef9312a291','0',NULL,NULL,NULL,'""',NULL,NULL,'0',NULL);
INSERT INTO runtime_agent_volatile VALUES('7c2692e5-4303-49f4-8ba0-cdc5a6f1db4a','0',NULL,NULL,NULL,'""',NULL,NULL,'0',NULL);
INSERT INTO runtime_agent_volatile VALUES('aaaaaaaa-1111-4111-8111-111111111111','0',NULL,NULL,NULL,'""',NULL,NULL,'0',NULL);
INSERT INTO runtime_agent_volatile VALUES('aaaaaaaa-2222-4222-8222-222222222222','0',NULL,NULL,NULL,'""',NULL,NULL,'0',NULL);
INSERT INTO runtime_agent_volatile VALUES('bbbbbbbb-1111-4111-8111-111111111111','0',NULL,NULL,NULL,'""',NULL,NULL,'0',NULL);
INSERT INTO runtime_agent_volatile VALUES('bbbbbbbb-2222-4222-8222-222222222222','0',NULL,NULL,NULL,'""',NULL,NULL,'0',NULL);
INSERT INTO runtime_agent_volatile VALUES('cccccccc-1111-4111-8111-111111111111','0',NULL,NULL,NULL,'""',NULL,NULL,'0',NULL);
CREATE TABLE IF NOT EXISTS "runtime_monitors" (
        id TEXT PRIMARY KEY, record TEXT NOT NULL,
        stream_pending INTEGER GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN COALESCE(json_array_length(record,'$.streamQueue')>0,0) ELSE 0 END) STORED NOT NULL CHECK(stream_pending IN (0,1)),
        monitor_status TEXT GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN CASE WHEN json_type(record,'$.status')='text' AND json_extract(record,'$.status') IN ('approval','starting','running','completed','failed','cancelled','lost') THEN json_extract(record,'$.status') ELSE 'lost' END ELSE 'lost' END) STORED NOT NULL CHECK(monitor_status IN ('approval','starting','running','completed','failed','cancelled','lost')),
        monitor_error_candidate INTEGER GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN json_type(record,'$.status') IS NOT 'text' OR json_extract(record,'$.status') NOT IN ('approval','starting','running','completed','failed','cancelled','lost') OR json_extract(record,'$.status')='lost' OR (json_extract(record,'$.status')='failed' AND json_type(record,'$.exitCode') IS NOT 'integer') ELSE 1 END) STORED NOT NULL CHECK(monitor_error_candidate IN (0,1)),
        monitor_created REAL GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN CASE WHEN json_type(record,'$.created') IN ('integer','real') AND json_extract(record,'$.created')>=0 AND json_extract(record,'$.created')<=1.7976931348623157e308 THEN json_extract(record,'$.created') ELSE 0 END ELSE 0 END) STORED NOT NULL CHECK(monitor_created >= 0 AND monitor_created <= 1.7976931348623157e308)
    );
INSERT INTO runtime_monitors VALUES('6a1cbd51-e035-54ce-af5c-7fd9850e3065','{"activityAt":19.0,"activityGeneration":0,"agent":"abf56fbb-f8dc-4241-ad30-3960b0afa460","bytes":0,"command":"printf fixture","created":19.0,"cwd":"__CALLBACK_STATE__","epoch":3,"error":"Server restarted. Command outcome unknown; not rerun.","id":"6a1cbd51-e035-54ce-af5c-7fd9850e3065","reattachRecovery":{"accountKey":"default","epoch":3,"status":"running","startedAt":19.0},"requestId":"managed-monitor","turnId":"turn-0","outputEventCount":0,"outputEventIds":[],"outputEventSequence":0,"outputPatterns":[],"interactive":false,"livenessCommand":"","log":"__CALLBACK_STATE__/monitor-logs/6a1cbd51-e035-54ce-af5c-7fd9850e3065.log","operation":{"accountKey":"default","agent":"abf56fbb-f8dc-4241-ad30-3960b0afa460","connectionId":"connection","epoch":3,"rootRequestId":"01a1920e-0046-76b9-a0e9-67a100abb4e7"},"stallTimeoutSeconds":1800,"stallWakeGeneration":-1,"status":"lost","successExitCodes":[0],"tail":"","timeout_ms":3600000,"wakeOn":"exit"}');
CREATE TABLE runtime_analytics_projection (seq INTEGER PRIMARY KEY AUTOINCREMENT, record TEXT NOT NULL);
CREATE TABLE runtime_item_blobs (id TEXT PRIMARY KEY, hash TEXT NOT NULL, size INTEGER NOT NULL);
PRAGMA writable_schema=ON;
INSERT INTO sqlite_schema(type,name,tbl_name,rootpage,sql)VALUES('table','runtime_search_next','runtime_search_next',0,'CREATE VIRTUAL TABLE runtime_search_next USING fts5(body,content='''',contentless_delete=1,tokenize=''unicode61'')');
CREATE TABLE IF NOT EXISTS 'runtime_search_next_data'(id INTEGER PRIMARY KEY, block BLOB);
INSERT INTO runtime_search_next_data VALUES(1,x'');
INSERT INTO runtime_search_next_data VALUES(10,x'00000000ff000001000000');
CREATE TABLE IF NOT EXISTS 'runtime_search_next_idx'(segid, term, pgno, PRIMARY KEY(segid, term)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS 'runtime_search_next_docsize'(id INTEGER PRIMARY KEY, sz BLOB, origin INTEGER);
CREATE TABLE IF NOT EXISTS 'runtime_search_next_config'(k PRIMARY KEY, v) WITHOUT ROWID;
INSERT INTO runtime_search_next_config VALUES('version',4);
CREATE TABLE runtime_orchestration_settings (id TEXT PRIMARY KEY, record TEXT NOT NULL);
INSERT INTO runtime_orchestration_settings VALUES('global','{"expired_before_ms":1793451753336}');
CREATE TABLE groups (
                  id TEXT PRIMARY KEY, name TEXT NOT NULL, members TEXT NOT NULL);
CREATE TABLE messages (
                  id TEXT PRIMARY KEY, room TEXT NOT NULL, author TEXT NOT NULL,
                  text TEXT NOT NULL, at REAL NOT NULL, deliveries TEXT NOT NULL);
CREATE TABLE graph_edges (
                  id TEXT PRIMARY KEY, source TEXT NOT NULL, target TEXT NOT NULL,
                  kind TEXT NOT NULL, UNIQUE(source, target, kind));
CREATE TABLE voice_sessions (
                  id TEXT PRIMARY KEY, agent TEXT NOT NULL, created REAL NOT NULL,
                  ended REAL, sdp TEXT, answer TEXT, native_thread TEXT,
                  account_key TEXT, connection_id TEXT, state TEXT, error TEXT, provider_scope TEXT);
CREATE TABLE voice_records (
                  seq INTEGER PRIMARY KEY AUTOINCREMENT, id TEXT UNIQUE NOT NULL,
                  agent TEXT NOT NULL, session TEXT NOT NULL, kind TEXT NOT NULL,
                  text TEXT NOT NULL, item_id TEXT NOT NULL, previous_item_id TEXT NOT NULL,
                  payload TEXT NOT NULL, created REAL NOT NULL);
CREATE TABLE voice_approvals (id TEXT PRIMARY KEY, agent TEXT NOT NULL, speech TEXT NOT NULL, transcript TEXT NOT NULL, status TEXT NOT NULL, result TEXT);
CREATE TABLE voice_deliveries (
                  id TEXT PRIMARY KEY, agent TEXT NOT NULL, records TEXT NOT NULL, text TEXT NOT NULL, edited_text TEXT);
CREATE TABLE voice_speech_requests (
                id TEXT PRIMARY KEY, session TEXT NOT NULL, record_id TEXT NOT NULL,
                state TEXT NOT NULL, error TEXT);
CREATE TABLE user_terminals (id TEXT PRIMARY KEY, record TEXT NOT NULL, output TEXT NOT NULL, offset INTEGER NOT NULL, output_generation INTEGER NOT NULL DEFAULT 0);
CREATE TABLE user_terminal_output (terminal TEXT NOT NULL, start INTEGER NOT NULL, end INTEGER NOT NULL, text TEXT NOT NULL, PRIMARY KEY (terminal,start));
CREATE TABLE user_terminal_event_cursor (terminal TEXT PRIMARY KEY, sequence INTEGER NOT NULL);
CREATE TABLE user_terminal_decoder_state (terminal TEXT PRIMARY KEY, pending BLOB NOT NULL);
CREATE TABLE runtime_access_settings(id INTEGER PRIMARY KEY CHECK(id=1), auto_pair INTEGER NOT NULL);
INSERT INTO runtime_access_settings VALUES(1,1);
CREATE TABLE runtime_access_discovered(id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_access_auto_attempts(id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_access_unrevokes(id TEXT PRIMARY KEY, client TEXT NOT NULL);
CREATE TABLE runtime_model_status (agent TEXT, revision TEXT, at REAL, record TEXT, PRIMARY KEY(agent,revision));
CREATE TABLE runtime_native_notices (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_process_actions (id TEXT PRIMARY KEY, signature TEXT NOT NULL, record TEXT NOT NULL);
CREATE TABLE runtime_safety_retries (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_limit_reset_attempts (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_limit_reset_requests (id TEXT PRIMARY KEY, attempt_id TEXT NOT NULL);
CREATE TABLE runtime_server_exec (
            id TEXT PRIMARY KEY, owner TEXT NOT NULL, actor TEXT NOT NULL,
            record TEXT NOT NULL);
CREATE TABLE runtime_server_exec_audit (
            id TEXT PRIMARY KEY, actor TEXT NOT NULL, source_server TEXT NOT NULL,
            server TEXT NOT NULL, cwd TEXT NOT NULL, argv_hash TEXT NOT NULL,
            start REAL NOT NULL, end REAL, exit_code INTEGER, signal INTEGER);
CREATE TABLE runtime_server_exec_input (
            id TEXT PRIMARY KEY, handle TEXT NOT NULL);
CREATE TABLE runtime_payload_migrations (name TEXT PRIMARY KEY,cursor INTEGER NOT NULL DEFAULT 0,complete INTEGER NOT NULL DEFAULT 0,status TEXT NOT NULL DEFAULT 'pending',updated REAL,error TEXT, cutover_ms INTEGER, receipt_enum_guard INTEGER GENERATED ALWAYS AS (NULL) VIRTUAL CHECK("status" IN ('pending','running','waitingForSpace','error','complete')));
INSERT INTO runtime_payload_migrations VALUES('2026-10-30-task-records',0,1,'complete',NULL,NULL,NULL);
INSERT INTO runtime_payload_migrations VALUES('receipts:cutover',0,1,'complete',1793451753.336,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:37:2026-10-28-receipts-execution-prepare:',0,1,'complete',1793451753.336,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:29:2026-10-28-receipts-execution:runtime_execution_runs',8,1,'complete',1793451753.336,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:29:2026-10-28-receipts-execution:runtime_execution_attempts',0,1,'complete',1793451753.336,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:29:2026-10-28-receipts-execution:runtime_execution_effects',119,1,'complete',1793451753.336,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:29:2026-10-28-receipts-execution:runtime_execution_nodes',4,1,'complete',1793451753.336,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:29:2026-10-28-receipts-execution:runtime_agents',8,1,'complete',1793451753.336,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:36:2026-10-28-receipts-execution-finish:',0,1,'complete',1793451753.336,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:35:2026-10-28-receipts-actions-prepare:',0,1,'complete',1793451753.427,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:27:2026-10-28-receipts-actions:runtime_operation_receipts',0,1,'complete',1793451753.427,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:27:2026-10-28-receipts-actions:runtime_native_action_receipts',0,1,'complete',1793451753.427,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:27:2026-10-28-receipts-actions:user_terminal_receipts',0,1,'complete',1793451753.427,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:27:2026-10-28-receipts-actions:runtime_agents',8,1,'complete',1793451753.427,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:27:2026-10-28-receipts-actions:runtime_safety_retries',0,1,'complete',1793451753.427,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:27:2026-10-28-receipts-actions:runtime_limit_reset_attempts',0,1,'complete',1793451753.427,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:27:2026-10-28-receipts-actions:runtime_process_actions',0,1,'complete',1793451753.427,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:27:2026-10-28-receipts-actions:runtime_monitors',1,1,'complete',1793451753.427,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:34:2026-10-28-receipts-actions-finish:',0,1,'complete',1793451753.427,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:42:2026-10-28-receipts-context-repair-prepare:',0,1,'complete',1793451753.563,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:34:2026-10-28-receipts-context-repair:runtime_context_terminal_receipts',0,1,'complete',1793451753.563,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:34:2026-10-28-receipts-context-repair:runtime_agents',8,1,'complete',1793451753.563,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:41:2026-10-28-receipts-context-repair-finish:',0,1,'complete',1793451753.563,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:33:2026-10-28-receipts-voice-prepare:',0,1,'complete',1793451753.616,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:25:2026-10-28-receipts-voice:voice_sessions',0,1,'complete',1793451753.616,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:25:2026-10-28-receipts-voice:runtime_operation_receipts',0,1,'complete',1793451753.616,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:32:2026-10-28-receipts-voice-finish:',0,1,'complete',1793451753.616,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:32:2026-10-28-receipts-work-prepare:',0,1,'complete',1793451753.658,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:24:2026-10-28-receipts-work:work-current-counters',0,1,'complete',1793451753.658,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:24:2026-10-28-receipts-work:runtime_operation_receipts',0,1,'complete',1793451753.658,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:31:2026-10-28-receipts-work-finish:',0,1,'complete',1793451753.658,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:33:2026-10-28-receipts-title-prepare:',0,1,'complete',1793451753.724,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:25:2026-10-28-receipts-title:title-effects',0,1,'complete',1793451753.724,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:25:2026-10-28-receipts-title:title-public',0,1,'complete',1793451753.724,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:32:2026-10-28-receipts-title-finish:',0,1,'complete',1793451753.724,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:39:2026-10-28-receipts-owner-state-prepare:',0,1,'complete',1793451753.766,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:31:2026-10-28-receipts-owner-state:runtime_operation_receipts',0,1,'complete',1793451753.766,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:38:2026-10-28-receipts-owner-state-finish:',0,1,'complete',1793451753.766,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:33:2026-10-28-receipts-tools-prepare:',0,1,'complete',1793451753.81,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:25:2026-10-28-receipts-tools:deleted-completed-c:managed-project-location',0,1,'complete',1793451753.336,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:25:2026-10-28-receipts-tools:normalized-signature:managed-project-location',0,1,'complete',1793451753.336,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:25:2026-10-28-receipts-tools:runtime_tool_request_aliases',20,1,'complete',1793451753.81,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:25:2026-10-28-receipts-tools:runtime_tool_results',0,1,'complete',1793451753.81,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:25:2026-10-28-receipts-tools:runtime_tool_requests',7,1,'complete',1793451753.81,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:32:2026-10-28-receipts-tools-finish:',0,1,'complete',1793451753.81,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:36:2026-10-28-receipts-requests-prepare:',0,1,'complete',1793451753.903,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:28:2026-10-28-receipts-requests:deleted-completed-c:peer-team',0,1,'complete',1793451753.336,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:28:2026-10-28-receipts-requests:deleted-completed-c:sidebar-order',0,1,'complete',1793451753.336,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:28:2026-10-28-receipts-requests:runtime_operation_receipts',0,1,'complete',1793451753.903,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:28:2026-10-28-receipts-requests:messages',0,1,'complete',1793451753.903,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:28:2026-10-28-receipts-requests:runtime_chat_messages',0,1,'complete',1793451753.903,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:28:2026-10-28-receipts-requests:runtime_workspace_operations',0,1,'complete',1793451753.903,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:28:2026-10-28-receipts-requests:runtime_event_meta',1,1,'complete',1793451753.903,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:35:2026-10-28-receipts-requests-finish:',0,1,'complete',1793451753.903,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:34:2026-10-28-receipts-remote-prepare:',0,1,'complete',1793451753.994,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:26:2026-10-28-receipts-remote:normalized-wire:server-outbox-operation',0,1,'complete',1793451753.336,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:26:2026-10-28-receipts-remote:runtime_access_receipts',0,1,'complete',1793451753.994,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:26:2026-10-28-receipts-remote:runtime_access_outbound',0,1,'complete',1793451753.994,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:26:2026-10-28-receipts-remote:runtime_server_inbox',0,1,'complete',1793451753.994,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:26:2026-10-28-receipts-remote:runtime_server_outbox',0,1,'complete',1793451753.994,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:26:2026-10-28-receipts-remote:runtime_server_fetch',0,1,'complete',1793451753.994,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:26:2026-10-28-receipts-remote:runtime_federation_inbox',0,1,'complete',1793451753.994,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:26:2026-10-28-receipts-remote:runtime_federation_outbox',0,1,'complete',1793451753.994,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:33:2026-10-28-receipts-remote-finish:',0,1,'complete',1793451753.994,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:45:2026-10-28-receipts-provider-delivery-prepare:',0,1,'complete',1793451754.17,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:37:2026-10-28-receipts-provider-delivery:runtime_operation_receipts',0,1,'complete',1793451754.17,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:44:2026-10-28-receipts-provider-delivery-finish:',0,1,'complete',1793451754.17,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:46:2026-10-28-receipts-provider-callbacks-prepare:',0,1,'complete',1793451754.208,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:38:2026-10-28-receipts-provider-callbacks:callbacks',0,1,'complete',1793451754.208,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:38:2026-10-28-receipts-provider-callbacks:frames',0,1,'complete',1793451754.208,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:38:2026-10-28-receipts-provider-callbacks:frame-outcomes',0,1,'complete',1793451754.208,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:38:2026-10-28-receipts-provider-callbacks:unanswered',0,1,'complete',1793451754.208,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:45:2026-10-28-receipts-provider-callbacks-finish:',0,1,'complete',1793451754.208,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:39:2026-10-28-receipts-vm-provider-prepare:',0,1,'complete',1793451754.295,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:31:2026-10-28-receipts-vm-provider:runtime_operation_receipts',0,1,'complete',1793451754.295,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:38:2026-10-28-receipts-vm-provider-finish:',0,1,'complete',1793451754.295,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:42:2026-10-28-receipts-account-worker-prepare:',0,1,'complete',1793451754.335,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:34:2026-10-28-receipts-account-worker:journal',0,1,'complete',1793451754.335,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:34:2026-10-28-receipts-account-worker:state',0,1,'complete',1793451754.335,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:34:2026-10-28-receipts-account-worker:calls',0,1,'complete',1793451754.335,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:34:2026-10-28-receipts-account-worker:effects',0,1,'complete',1793451754.335,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:34:2026-10-28-receipts-account-worker:streams',0,1,'complete',1793451754.335,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:34:2026-10-28-receipts-account-worker:stream-pending',0,1,'complete',1793451754.335,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:34:2026-10-28-receipts-account-worker:stream-bases',0,1,'complete',1793451754.335,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:34:2026-10-28-receipts-account-worker:submission-inputs',0,1,'complete',1793451754.335,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:34:2026-10-28-receipts-account-worker:unmatched',0,1,'complete',1793451754.335,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:41:2026-10-28-receipts-account-worker-finish:',0,1,'complete',1793451754.335,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:37:2026-10-28-receipts-incidents-prepare:',0,1,'complete',1793451754.466,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:29:2026-10-28-receipts-incidents:runtime_operation_receipts',0,1,'complete',1793451754.466,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:36:2026-10-28-receipts-incidents-finish:',0,1,'complete',1793451754.466,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:34:2026-10-28-receipts-checks-prepare:',0,1,'complete',1793451754.517,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:26:2026-10-28-receipts-checks:runtime_events',1,1,'complete',1793451754.517,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:26:2026-10-28-receipts-checks:runtime_execution_effects',119,1,'complete',1793451754.517,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:26:2026-10-28-receipts-checks:runtime_payload_migrations',94,1,'complete',1793451754.517,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:33:2026-10-28-receipts-checks-finish:',0,1,'complete',1793451754.517,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:43:2026-10-28-receipts-drop-old-tables-prepare:',0,1,'complete',1793451754.706,NULL,1793451753336);
INSERT INTO runtime_payload_migrations VALUES('receipts:42:2026-10-28-receipts-drop-old-tables-finish:',0,1,'complete',1793451754.706,NULL,1793451753336);
CREATE TABLE runtime_stored_text_batches (
        name TEXT NOT NULL, batch INTEGER NOT NULL, first_rowid INTEGER NOT NULL,
        last_rowid INTEGER NOT NULL, rows INTEGER NOT NULL, plain_bytes INTEGER NOT NULL,
        stored_bytes INTEGER NOT NULL, sha256 TEXT NOT NULL, created REAL NOT NULL,
        PRIMARY KEY(name, batch));
CREATE TABLE runtime_outbox (
            id TEXT PRIMARY KEY, kind TEXT NOT NULL CHECK("kind" IN ('provider_call','provider_callback','journal_entry','publication','stream_flush','followup_effect','steer','voice_stop','voice_delete','work_archive','work_vm','title_generation','native_action','terminal_action','monitor_input','mcp_reconnect','project_registration','remote_delivery','managed_tool')), owner_kind TEXT NOT NULL CHECK("owner_kind" IN ('agent','task','account','vm','project','monitor','terminal','message','request','server')),
            owner_id TEXT NOT NULL, request_id TEXT, signature TEXT NOT NULL,
            status TEXT NOT NULL CHECK("status" IN ('pending','submitted','unknown','committed','unrecognized')), account TEXT, stream_id TEXT, generation INTEGER,
            ordinal INTEGER, next_at INTEGER, payload TEXT NOT NULL, created INTEGER NOT NULL, target TEXT,
            CHECK((status!='unknown' AND status!='unrecognized') OR next_at IS NULL));
CREATE TABLE runtime_error_incidents (
            id TEXT PRIMARY KEY, kind TEXT NOT NULL CHECK("kind" IN ('monitor_outcome_lost','monitor_not_recorded','input_unconfirmed','input_uncertain','input_stuck','turn_disconnected','workspace_failed','http_failure','recovery_failed','stored_record_invalid','unclassified_failure','turn_stalled','input_not_started','session_not_started','schedule_stalled')), owner_kind TEXT NOT NULL CHECK("owner_kind" IN ('agent','task','account','vm','project','monitor','terminal','message','request','server')),
            owner_id TEXT NOT NULL, created INTEGER NOT NULL, last_seen INTEGER NOT NULL,
            dismissed_at INTEGER, payload TEXT NOT NULL);
CREATE TABLE runtime_account_state (
            account TEXT PRIMARY KEY, next_generation INTEGER NOT NULL, connection_generation INTEGER,
            connection_id TEXT, journal_ack INTEGER NOT NULL, recovery_required INTEGER NOT NULL,
            streams TEXT NOT NULL, limits_observation TEXT, start_failure TEXT);
CREATE TABLE request_results (
           id TEXT PRIMARY KEY,
           signature TEXT NOT NULL,
           status TEXT NOT NULL DEFAULT 'applied' CHECK("status" IN ('applied','rejected')),
           ref_kind TEXT CHECK("ref_kind" IN ('agent','annotation','complaint','message','project','room','work')),
           ref_id TEXT,
           ref_rev INTEGER,
           result TEXT,
           created INTEGER NOT NULL,
           CHECK ((result IS NOT NULL AND ref_kind IS NULL AND ref_id IS NULL AND ref_rev IS NULL)
               OR (result IS NULL AND ref_kind IS NOT NULL AND ref_id IS NOT NULL AND ref_rev IS NOT NULL)
               OR (result IS NULL AND status = 'rejected' AND ref_kind IS NULL AND ref_id IS NULL AND ref_rev IS NULL))
         );
INSERT INTO request_results VALUES('thread-0:alias-0','1dff160641e4d1fe77178179b43844c3c6d588f8f3741f77bb9b8655c3a5e579','applied',NULL,NULL,NULL,'{"actor":"abf56fbb-f8dc-4241-ad30-3960b0afa460","reply":{"contentItems":[{"text":"{\"requestId\": \"thread-0:alias-0\", \"stage\": \"interrupted\", \"outcome\": \"unknown\", \"message\": \"Server restarted before the execution receipt. Nothing was repeated. Check what the call did before you run it again.\"}","type":"inputText"}],"success":false},"references":[]}',1793451753336);
INSERT INTO request_results VALUES('thread-0:spawn:request-1','c91f8a50f288534226b47d6abda036631ba03b0ad5aae43ad0b8e1fe6431fc12','applied',NULL,NULL,NULL,'{"actor":"abf56fbb-f8dc-4241-ad30-3960b0afa460","reply":{"contentItems":[{"text":"{\"requestId\": \"thread-0:spawn:request-1\", \"stage\": \"cancelled\", \"outcome\": \"not_applied\", \"cancelRequested\": false, \"message\": \"Server restarted before execution\"}","type":"inputText"}],"success":false},"references":[]}',1793451753336);
INSERT INTO request_results VALUES('thread-0:send:request-2','abb0f8214ffec1e848f67f36b7b030d1f53121efbd2979fd75680fd3c7785749','applied',NULL,NULL,NULL,'{"actor":"abf56fbb-f8dc-4241-ad30-3960b0afa460","reply":{"contentItems":[{"text":"{\"requestId\": \"thread-0:send:request-2\", \"stage\": \"cancelled\", \"outcome\": \"not_applied\", \"cancelRequested\": false, \"message\": \"Server restarted before execution\"}","type":"inputText"}],"success":false},"references":[]}',1793451753336);
INSERT INTO request_results VALUES('thread-0:review:request-3','69b6d0789d8ba6854d46da3a2ec8e90d8871e7ce6895cd678acd23aaf86106ca','applied',NULL,NULL,NULL,'{"actor":"abf56fbb-f8dc-4241-ad30-3960b0afa460","reply":{"contentItems":[{"text":"{\"requestId\": \"thread-0:review:request-3\", \"stage\": \"cancelled\", \"outcome\": \"not_applied\", \"cancelRequested\": false, \"message\": \"Server restarted before execution\"}","type":"inputText"}],"success":false},"references":[]}',1793451753336);
INSERT INTO request_results VALUES('thread-0:server:request-4','4b00aba39b4be2847cacf972191005ff1b73f15438c48c9b54e8507c26aeb478','applied',NULL,NULL,NULL,'{"actor":"abf56fbb-f8dc-4241-ad30-3960b0afa460","reply":{"contentItems":[{"text":"{\"requestId\": \"thread-0:server:request-4\", \"stage\": \"cancelled\", \"outcome\": \"not_applied\", \"cancelRequested\": false, \"message\": \"Server restarted before execution\"}","type":"inputText"}],"success":false},"references":[]}',1793451753336);
INSERT INTO request_results VALUES('thread-0:move:request-5','00cebdf9732e1609eecdd9a0f86b687817cd2a371ef83e312ac50706f822f661','applied',NULL,NULL,NULL,'{"actor":"abf56fbb-f8dc-4241-ad30-3960b0afa460","reply":{"contentItems":[{"text":"{\"requestId\": \"thread-0:move:request-5\", \"stage\": \"cancelled\", \"outcome\": \"not_applied\", \"cancelRequested\": false, \"message\": \"Server restarted before execution\"}","type":"inputText"}],"success":false},"references":[]}',1793451753336);
INSERT INTO request_results VALUES('thread-0:move:request-6','4d2a73cfcffe4df8b7075fa3edcc43dc0faab254b347892d55b3c227be583e82','applied',NULL,NULL,NULL,'{"actor":"abf56fbb-f8dc-4241-ad30-3960b0afa460","reply":{"contentItems":[{"text":"{\"requestId\": \"thread-0:move:request-6\", \"stage\": \"cancelled\", \"outcome\": \"not_applied\", \"cancelRequested\": false, \"message\": \"Server restarted before execution\"}","type":"inputText"}],"success":false},"references":[]}',1793451753336);
CREATE TABLE client_command_watermarks (
    client  TEXT PRIMARY KEY,
    applied INTEGER NOT NULL CHECK (applied >= 0)
) WITHOUT ROWID;
CREATE TABLE runtime_event_revisions(id TEXT PRIMARY KEY, rev INTEGER NOT NULL) WITHOUT ROWID;
CREATE TABLE durable_view_log(
             collection TEXT NOT NULL, id TEXT NOT NULL, seq INTEGER NOT NULL,
             deleted INTEGER NOT NULL, PRIMARY KEY(collection,id));
INSERT INTO durable_view_log VALUES('agent','abf56fbb-f8dc-4241-ad30-3960b0afa460',346,0);
INSERT INTO durable_view_log VALUES('agent','3e73e736-e287-42b7-963c-6bef9312a291',136,0);
INSERT INTO durable_view_log VALUES('agent','7c2692e5-4303-49f4-8ba0-cdc5a6f1db4a',137,0);
INSERT INTO durable_view_log VALUES('agent','aaaaaaaa-1111-4111-8111-111111111111',127,0);
INSERT INTO durable_view_log VALUES('agent','aaaaaaaa-2222-4222-8222-222222222222',128,0);
INSERT INTO durable_view_log VALUES('agent','bbbbbbbb-1111-4111-8111-111111111111',129,0);
INSERT INTO durable_view_log VALUES('agent','bbbbbbbb-2222-4222-8222-222222222222',130,0);
INSERT INTO durable_view_log VALUES('agent','cccccccc-1111-4111-8111-111111111111',73,1);
INSERT INTO durable_view_log VALUES('request','8b359a1f-85cd-4d54-adfd-733c9c049f3c',92,0);
INSERT INTO durable_view_log VALUES('request','python-written-card',138,0);
INSERT INTO durable_view_log VALUES('request','failure-answer-notSent',292,0);
INSERT INTO durable_view_log VALUES('request','failure-delete-notSent',297,0);
INSERT INTO durable_view_log VALUES('request','invalid-0',300,0);
INSERT INTO durable_view_log VALUES('request','invalid-1',301,0);
INSERT INTO durable_view_log VALUES('request','invalid-2',302,0);
INSERT INTO durable_view_log VALUES('request','invalid-3',303,0);
INSERT INTO durable_view_log VALUES('request','invalid-4',304,0);
INSERT INTO durable_view_log VALUES('request','invalid-5',305,0);
INSERT INTO durable_view_log VALUES('request','invalid-7',310,0);
INSERT INTO durable_view_log VALUES('request','invalid-8',311,0);
INSERT INTO durable_view_log VALUES('request','invalid-9',312,0);
INSERT INTO durable_view_log VALUES('request','invalid-10',313,0);
INSERT INTO durable_view_log VALUES('request','answer-0',9,1);
INSERT INTO durable_view_log VALUES('request','answer-1',11,1);
INSERT INTO durable_view_log VALUES('request','answer-2',13,1);
INSERT INTO durable_view_log VALUES('request','answer-3',15,1);
INSERT INTO durable_view_log VALUES('request','answer-4',17,1);
INSERT INTO durable_view_log VALUES('request','answer-5',19,1);
INSERT INTO durable_view_log VALUES('request','answer-6',21,1);
INSERT INTO durable_view_log VALUES('request','answer-7',23,1);
INSERT INTO durable_view_log VALUES('request','answer-8',25,1);
INSERT INTO durable_view_log VALUES('request','answer-9',27,1);
INSERT INTO durable_view_log VALUES('request','answer-10',29,1);
INSERT INTO durable_view_log VALUES('request','answer-11',31,1);
INSERT INTO durable_view_log VALUES('request','answer-12',33,1);
INSERT INTO durable_view_log VALUES('request','answer-13',35,1);
INSERT INTO durable_view_log VALUES('request','answer-14',37,1);
INSERT INTO durable_view_log VALUES('request','answer-15',39,1);
INSERT INTO durable_view_log VALUES('request','answer-16',41,1);
INSERT INTO durable_view_log VALUES('request','answer-17',43,1);
INSERT INTO durable_view_log VALUES('request','answer-18',45,1);
INSERT INTO durable_view_log VALUES('request','answer-19',47,1);
INSERT INTO durable_view_log VALUES('request','answer-20',49,1);
INSERT INTO durable_view_log VALUES('request','answer-21',51,1);
INSERT INTO durable_view_log VALUES('request','answer-22',53,1);
INSERT INTO durable_view_log VALUES('request','answer-23',55,1);
INSERT INTO durable_view_log VALUES('request','answer-24',57,1);
INSERT INTO durable_view_log VALUES('request','answer-25',59,1);
INSERT INTO durable_view_log VALUES('request','answer-26',61,1);
INSERT INTO durable_view_log VALUES('request','answer-27',63,1);
INSERT INTO durable_view_log VALUES('request','answer-28',65,1);
INSERT INTO durable_view_log VALUES('request','answer-29',67,1);
INSERT INTO durable_view_log VALUES('request','resolve-card',332,1);
INSERT INTO durable_view_log VALUES('request','roundtrip-0',143,1);
INSERT INTO durable_view_log VALUES('request','roundtrip-1',147,1);
INSERT INTO durable_view_log VALUES('request','roundtrip-2',151,1);
INSERT INTO durable_view_log VALUES('request','roundtrip-3',155,1);
INSERT INTO durable_view_log VALUES('request','roundtrip-4',159,1);
INSERT INTO durable_view_log VALUES('request','roundtrip-5',163,1);
INSERT INTO durable_view_log VALUES('request','roundtrip-6',167,1);
INSERT INTO durable_view_log VALUES('request','roundtrip-7',171,1);
INSERT INTO durable_view_log VALUES('request','roundtrip-8',175,1);
INSERT INTO durable_view_log VALUES('request','roundtrip-9',179,1);
INSERT INTO durable_view_log VALUES('request','roundtrip-10',183,1);
INSERT INTO durable_view_log VALUES('request','roundtrip-11',187,1);
INSERT INTO durable_view_log VALUES('request','roundtrip-12',191,1);
INSERT INTO durable_view_log VALUES('request','roundtrip-13',195,1);
INSERT INTO durable_view_log VALUES('request','roundtrip-14',199,1);
INSERT INTO durable_view_log VALUES('request','roundtrip-15',203,1);
INSERT INTO durable_view_log VALUES('request','roundtrip-16',207,1);
INSERT INTO durable_view_log VALUES('request','roundtrip-17',211,1);
INSERT INTO durable_view_log VALUES('request','roundtrip-18',215,1);
INSERT INTO durable_view_log VALUES('request','roundtrip-19',219,1);
INSERT INTO durable_view_log VALUES('request','roundtrip-20',223,1);
INSERT INTO durable_view_log VALUES('request','roundtrip-21',227,1);
INSERT INTO durable_view_log VALUES('request','roundtrip-22',231,1);
INSERT INTO durable_view_log VALUES('request','roundtrip-23',235,1);
INSERT INTO durable_view_log VALUES('request','roundtrip-24',239,1);
INSERT INTO durable_view_log VALUES('request','roundtrip-25',243,1);
INSERT INTO durable_view_log VALUES('request','roundtrip-26',247,1);
INSERT INTO durable_view_log VALUES('request','roundtrip-27',251,1);
INSERT INTO durable_view_log VALUES('request','roundtrip-28',255,1);
INSERT INTO durable_view_log VALUES('request','roundtrip-29',259,1);
INSERT INTO durable_view_log VALUES('request','delete-0',263,1);
INSERT INTO durable_view_log VALUES('request','delete-1',264,1);
INSERT INTO durable_view_log VALUES('request','delete-2',265,1);
INSERT INTO durable_view_log VALUES('request','delete-3',266,1);
INSERT INTO durable_view_log VALUES('request','delete-4',267,1);
INSERT INTO durable_view_log VALUES('request','delete-5',268,1);
INSERT INTO durable_view_log VALUES('request','delete-6',269,1);
INSERT INTO durable_view_log VALUES('request','delete-7',271,1);
INSERT INTO durable_view_log VALUES('request','delete-8',274,1);
INSERT INTO durable_view_log VALUES('request','delete-9',275,1);
INSERT INTO durable_view_log VALUES('request','delete-10',276,1);
INSERT INTO durable_view_log VALUES('request','delete-11',277,1);
INSERT INTO durable_view_log VALUES('request','delete-12',278,1);
INSERT INTO durable_view_log VALUES('request','delete-13',279,1);
INSERT INTO durable_view_log VALUES('request','delete-14',281,1);
INSERT INTO durable_view_log VALUES('request','delete-15',284,1);
INSERT INTO durable_view_log VALUES('request','delete-16',285,1);
INSERT INTO durable_view_log VALUES('request','delete-17',286,1);
INSERT INTO durable_view_log VALUES('request','delete-18',287,1);
INSERT INTO durable_view_log VALUES('request','delete-19',288,1);
INSERT INTO durable_view_log VALUES('request','delete-20',289,1);
INSERT INTO durable_view_log VALUES('request','failure-answer-unknown',294,1);
INSERT INTO durable_view_log VALUES('request','failure-delete-unknown',299,1);
INSERT INTO durable_view_log VALUES('request','invalid-6',307,1);
INSERT INTO durable_view_log VALUES('request','invalid-11',315,1);
INSERT INTO durable_view_log VALUES('request','race-completed',335,1);
INSERT INTO durable_view_log VALUES('request','race-failed',340,1);
INSERT INTO durable_view_log VALUES('request','race-resolved',345,1);
INSERT INTO durable_view_log VALUES('work','5d133257-ae7a-4370-aa83-1a34dd5b7e09',91,0);
INSERT INTO durable_view_log VALUES('work','5c30f2eb-316e-4fb4-aa09-276856bf7385',77,0);
INSERT INTO durable_view_log VALUES('work','378e58ab-a1ce-428c-925f-b40b8e08143a',84,0);
INSERT INTO durable_view_log VALUES('work','cdd039e5-e272-4855-8eff-fa22c71b841a',85,0);
INSERT INTO durable_view_log VALUES('work','6cc71248-30bf-4811-b473-1aafcab6f5a8',86,0);
CREATE TABLE chats(
            id TEXT PRIMARY KEY, state TEXT NOT NULL, status TEXT NOT NULL,
            auto_wake INTEGER, usage_resume_enabled INTEGER, execution_mode TEXT, turn_epoch INTEGER) STRICT;
INSERT INTO chats VALUES('abf56fbb-f8dc-4241-ad30-3960b0afa460','active','running',1,1,'native',3);
INSERT INTO chats VALUES('3e73e736-e287-42b7-963c-6bef9312a291','active','running',1,1,'native',3);
INSERT INTO chats VALUES('7c2692e5-4303-49f4-8ba0-cdc5a6f1db4a','active','running',1,1,'native',3);
INSERT INTO chats VALUES('aaaaaaaa-1111-4111-8111-111111111111','active','interrupted',0,1,'native',3);
INSERT INTO chats VALUES('aaaaaaaa-2222-4222-8222-222222222222','active','interrupted',0,1,'native',3);
INSERT INTO chats VALUES('bbbbbbbb-1111-4111-8111-111111111111','active','interrupted',0,1,'native',3);
INSERT INTO chats VALUES('bbbbbbbb-2222-4222-8222-222222222222','active','interrupted',0,1,'native',3);
INSERT INTO chats VALUES('cccccccc-1111-4111-8111-111111111111','active','interrupted',0,1,'native',3);
CREATE TABLE sessions(
            id TEXT PRIMARY KEY, chat TEXT NOT NULL, thread_id TEXT NOT NULL,
            account_key TEXT NOT NULL, state TEXT NOT NULL) STRICT;
INSERT INTO sessions VALUES('abf56fbb-f8dc-4241-ad30-3960b0afa460:thread-0','abf56fbb-f8dc-4241-ad30-3960b0afa460','thread-0','default','ready');
INSERT INTO sessions VALUES('3e73e736-e287-42b7-963c-6bef9312a291:thread-1','3e73e736-e287-42b7-963c-6bef9312a291','thread-1','default','ready');
INSERT INTO sessions VALUES('7c2692e5-4303-49f4-8ba0-cdc5a6f1db4a:thread-2','7c2692e5-4303-49f4-8ba0-cdc5a6f1db4a','thread-2','default','ready');
INSERT INTO sessions VALUES('aaaaaaaa-1111-4111-8111-111111111111:thread-1','aaaaaaaa-1111-4111-8111-111111111111','thread-1','default','ready');
INSERT INTO sessions VALUES('aaaaaaaa-2222-4222-8222-222222222222:thread-1','aaaaaaaa-2222-4222-8222-222222222222','thread-1','default','ready');
INSERT INTO sessions VALUES('bbbbbbbb-1111-4111-8111-111111111111:thread-2','bbbbbbbb-1111-4111-8111-111111111111','thread-2','default','ready');
INSERT INTO sessions VALUES('bbbbbbbb-2222-4222-8222-222222222222:thread-1','bbbbbbbb-2222-4222-8222-222222222222','thread-1','default','ready');
INSERT INTO sessions VALUES('cccccccc-1111-4111-8111-111111111111:thread-1','cccccccc-1111-4111-8111-111111111111','thread-1','default','ready');
CREATE TABLE turns(
            id TEXT PRIMARY KEY, chat TEXT NOT NULL, state TEXT NOT NULL, session TEXT,
            provider_turn TEXT, attempt TEXT, interrupt_ticket TEXT,
            attempt_submitted INTEGER GENERATED ALWAYS AS (attempt->>'$.submitted') STORED,
            attempt_connection_generation INTEGER GENERATED ALWAYS AS (attempt->>'$.connectionGeneration') STORED,
            attempt_action TEXT GENERATED ALWAYS AS (attempt->>'$.action') STORED,
            attempt_active_at_reservation INTEGER GENERATED ALWAYS AS (attempt->>'$.activeAtReservation') STORED) STRICT;
CREATE TABLE IF NOT EXISTS sqlite_sequence(name,seq);
DELETE FROM sqlite_sequence;
INSERT INTO sqlite_sequence VALUES('runtime_chat_messages',1);
INSERT INTO sqlite_sequence VALUES('runtime_analytics_projection',0);
CREATE TRIGGER sync_entity_tombstone_count_insert
      AFTER INSERT ON sync_entities
      WHEN NEW.deleted=1 AND NEW.collection NOT LIKE 'transcript:%' BEGIN
        UPDATE sync_entity_meta SET value=CAST(value AS INTEGER)+1
        WHERE key='entity_tombstone_count';
      END;
CREATE TRIGGER sync_entity_tombstone_count_update
      AFTER UPDATE OF deleted,collection ON sync_entities
      WHEN (OLD.deleted=1 AND OLD.collection NOT LIKE 'transcript:%') !=
           (NEW.deleted=1 AND NEW.collection NOT LIKE 'transcript:%') BEGIN
        UPDATE sync_entity_meta SET value=CAST(value AS INTEGER)+
          CASE WHEN NEW.deleted=1 AND NEW.collection NOT LIKE 'transcript:%' THEN 1 ELSE -1 END
        WHERE key='entity_tombstone_count';
      END;
CREATE TRIGGER sync_entity_tombstone_count_delete
      AFTER DELETE ON sync_entities
      WHEN OLD.deleted=1 AND OLD.collection NOT LIKE 'transcript:%' BEGIN
        UPDATE sync_entity_meta SET value=CAST(value AS INTEGER)-1
        WHERE key='entity_tombstone_count';
      END;
CREATE TRIGGER sync_transcript_revision_runtime_items_insert AFTER INSERT ON runtime_items BEGIN INSERT INTO runtime_transcript_revisions(agent,revision)
                                    VALUES (NEW.agent,1) ON CONFLICT(agent) DO UPDATE SET revision=revision+1; END;
CREATE TRIGGER sync_transcript_revision_runtime_items_update AFTER UPDATE ON runtime_items BEGIN INSERT INTO runtime_transcript_revisions(agent,revision)
                                    VALUES (NEW.agent,1) ON CONFLICT(agent) DO UPDATE SET revision=revision+1;
                                    INSERT INTO runtime_transcript_revisions(agent,revision)
                                    SELECT OLD.agent,1 WHERE OLD.agent!=NEW.agent
                                    ON CONFLICT(agent) DO UPDATE SET revision=revision+1; END;
CREATE TRIGGER sync_transcript_revision_runtime_items_delete AFTER DELETE ON runtime_items BEGIN INSERT INTO runtime_transcript_revisions(agent,revision)
                                    VALUES (OLD.agent,1) ON CONFLICT(agent) DO UPDATE SET revision=revision+1; END;
CREATE TRIGGER runtime_agent_record_generation_insert AFTER INSERT ON runtime_agents BEGIN UPDATE runtime_agent_record_generation SET value=value+1 WHERE id=1; UPDATE runtime_scheduler_agent_change_state SET entries=entries+CASE WHEN EXISTS(SELECT 1 FROM runtime_scheduler_agent_changes WHERE id=NEW.id) THEN 0 ELSE 1 END,total=total-COALESCE((SELECT generation FROM runtime_scheduler_agent_changes WHERE id=NEW.id),0)+(SELECT value FROM runtime_agent_record_generation WHERE id=1) WHERE id=1; INSERT INTO runtime_scheduler_agent_changes(id,generation) SELECT NEW.id,value FROM runtime_agent_record_generation WHERE id=1 ON CONFLICT(id) DO UPDATE SET generation=excluded.generation; UPDATE runtime_scheduler_agent_change_state SET generation=(SELECT value FROM runtime_agent_record_generation WHERE id=1) WHERE id=1; END;
CREATE TRIGGER runtime_agent_record_generation_update AFTER UPDATE ON runtime_agents WHEN OLD.record IS NOT NEW.record OR OLD.id IS NOT NEW.id OR OLD.rowid IS NOT NEW.rowid BEGIN UPDATE runtime_agent_record_generation SET value=value+1 WHERE id=1; UPDATE runtime_scheduler_agent_change_state SET entries=entries+CASE WHEN EXISTS(SELECT 1 FROM runtime_scheduler_agent_changes WHERE id=OLD.id) THEN 0 ELSE 1 END,total=total-COALESCE((SELECT generation FROM runtime_scheduler_agent_changes WHERE id=OLD.id),0)+(SELECT value FROM runtime_agent_record_generation WHERE id=1) WHERE id=1 AND OLD.id IS NOT NEW.id; INSERT INTO runtime_scheduler_agent_changes(id,generation) SELECT OLD.id,value FROM runtime_agent_record_generation WHERE id=1 AND OLD.id IS NOT NEW.id ON CONFLICT(id) DO UPDATE SET generation=excluded.generation; UPDATE runtime_scheduler_agent_change_state SET entries=entries+CASE WHEN EXISTS(SELECT 1 FROM runtime_scheduler_agent_changes WHERE id=NEW.id) THEN 0 ELSE 1 END,total=total-COALESCE((SELECT generation FROM runtime_scheduler_agent_changes WHERE id=NEW.id),0)+(SELECT value FROM runtime_agent_record_generation WHERE id=1) WHERE id=1; INSERT INTO runtime_scheduler_agent_changes(id,generation) SELECT NEW.id,value FROM runtime_agent_record_generation WHERE id=1 ON CONFLICT(id) DO UPDATE SET generation=excluded.generation; UPDATE runtime_scheduler_agent_change_state SET generation=(SELECT value FROM runtime_agent_record_generation WHERE id=1) WHERE id=1; END;
CREATE TRIGGER runtime_agent_record_generation_delete AFTER DELETE ON runtime_agents BEGIN UPDATE runtime_agent_record_generation SET value=value+1 WHERE id=1; UPDATE runtime_scheduler_agent_change_state SET entries=entries+CASE WHEN EXISTS(SELECT 1 FROM runtime_scheduler_agent_changes WHERE id=OLD.id) THEN 0 ELSE 1 END,total=total-COALESCE((SELECT generation FROM runtime_scheduler_agent_changes WHERE id=OLD.id),0)+(SELECT value FROM runtime_agent_record_generation WHERE id=1) WHERE id=1; INSERT INTO runtime_scheduler_agent_changes(id,generation) SELECT OLD.id,value FROM runtime_agent_record_generation WHERE id=1 ON CONFLICT(id) DO UPDATE SET generation=excluded.generation; UPDATE runtime_scheduler_agent_change_state SET generation=(SELECT value FROM runtime_agent_record_generation WHERE id=1) WHERE id=1; END;
CREATE TRIGGER sync_transcript_revision_runtime_agents_insert AFTER INSERT ON runtime_agents BEGIN INSERT INTO runtime_transcript_revisions(agent,revision)
                                    VALUES (NEW.id,1) ON CONFLICT(agent) DO UPDATE SET revision=revision+1; END;
CREATE TRIGGER sync_transcript_revision_runtime_agents_update AFTER UPDATE ON runtime_agents BEGIN INSERT INTO runtime_transcript_revisions(agent,revision)
                                    VALUES (NEW.id,1) ON CONFLICT(agent) DO UPDATE SET revision=revision+1;
                                    INSERT INTO runtime_transcript_revisions(agent,revision)
                                    SELECT OLD.id,1 WHERE OLD.id!=NEW.id
                                    ON CONFLICT(agent) DO UPDATE SET revision=revision+1; END;
CREATE TRIGGER sync_transcript_revision_runtime_agents_delete AFTER DELETE ON runtime_agents BEGIN INSERT INTO runtime_transcript_revisions(agent,revision)
                                    VALUES (OLD.id,1) ON CONFLICT(agent) DO UPDATE SET revision=revision+1; END;
CREATE TRIGGER sync_entity_scope_visibility_insert
AFTER INSERT ON sync_entities
WHEN NEW.collection='agentSummary' AND NEW.deleted=0
     AND json_extract(NEW.payload,'$.value.rootId') IS NOT NULL BEGIN
  INSERT INTO sync_entity_meta(key,value)
  VALUES('entity_scope_visibility:' || json_array('*',json_extract(NEW.payload,'$.value.rootId')),CAST(NEW.seq AS TEXT))
  ON CONFLICT(key) DO UPDATE SET value=CAST(MAX(CAST(sync_entity_meta.value AS INTEGER),CAST(excluded.value AS INTEGER)) AS TEXT);
END;
CREATE TRIGGER sync_entity_scope_visibility_update
AFTER UPDATE OF payload,deleted ON sync_entities
WHEN (OLD.deleted=0 AND NEW.deleted=0 AND (
        json_extract(OLD.payload,'$.value.agent') IS NOT json_extract(NEW.payload,'$.value.agent') OR
        json_extract(OLD.payload,'$.value.rootId') IS NOT json_extract(NEW.payload,'$.value.rootId')))
     OR (NEW.collection='agentSummary' AND OLD.deleted IS NOT NEW.deleted) BEGIN
  INSERT INTO sync_entity_meta(key,value)
  SELECT 'entity_scope_visibility:' || identity,CAST(NEW.seq AS TEXT)
  FROM (
    SELECT json_array(NEW.collection,json_extract(OLD.payload,'$.value.rootId')) AS identity
      WHERE json_extract(OLD.payload,'$.value.rootId') IS NOT NULL AND json_extract(OLD.payload,'$.value.rootId') IS NOT json_extract(NEW.payload,'$.value.rootId')
    UNION SELECT json_array(NEW.collection,json_extract(NEW.payload,'$.value.rootId'))
      WHERE json_extract(NEW.payload,'$.value.rootId') IS NOT NULL AND json_extract(OLD.payload,'$.value.rootId') IS NOT json_extract(NEW.payload,'$.value.rootId')
    UNION SELECT json_array(NEW.collection,selector,'team') FROM (
      SELECT json_extract(OLD.payload,'$.value.agent') AS selector
      UNION SELECT json_extract(NEW.payload,'$.value.agent')
      UNION SELECT json_extract(payload,'$.value.rootId') FROM sync_entities
        WHERE collection='agentSummary' AND deleted=0 AND id IN
        (json_extract(OLD.payload,'$.value.agent'),json_extract(NEW.payload,'$.value.agent'))
    ) WHERE selector IS NOT NULL AND json_extract(OLD.payload,'$.value.agent') IS NOT json_extract(NEW.payload,'$.value.agent')
  ) WHERE OLD.deleted=0 AND NEW.deleted=0
  ON CONFLICT(key) DO UPDATE SET value=CAST(MAX(CAST(sync_entity_meta.value AS INTEGER),CAST(excluded.value AS INTEGER)) AS TEXT);
  INSERT INTO sync_entity_meta(key,value)
  SELECT 'entity_scope_visibility:' || json_array('*',root),CAST(NEW.seq AS TEXT)
  FROM (
    SELECT json_extract(OLD.payload,'$.value.rootId') AS root WHERE OLD.deleted=0
    UNION SELECT json_extract(NEW.payload,'$.value.rootId') WHERE NEW.deleted=0
  ) WHERE NEW.collection='agentSummary' AND root IS NOT NULL
  ON CONFLICT(key) DO UPDATE SET value=CAST(MAX(CAST(sync_entity_meta.value AS INTEGER),CAST(excluded.value AS INTEGER)) AS TEXT);
END;
CREATE TRIGGER sync_entity_scope_tombstone_update
AFTER UPDATE OF deleted ON sync_entities
WHEN OLD.deleted IS NOT NEW.deleted AND NEW.collection!='agentSummary' BEGIN
  INSERT INTO sync_entity_meta(key,value)
  SELECT 'entity_scope_tombstone:' || json_array(OLD.collection,OLD.id), json_group_array(identity)
  FROM (
    SELECT json_array(OLD.collection,json_extract(OLD.payload,'$.value.rootId')) AS identity
      WHERE json_extract(OLD.payload,'$.value.rootId') IS NOT NULL
    UNION SELECT json_array(OLD.collection,selector,'team') FROM (
      SELECT json_extract(OLD.payload,'$.value.agent') AS selector
      UNION SELECT json_extract(payload,'$.value.rootId') FROM sync_entities
        WHERE collection='agentSummary' AND deleted=0 AND id=json_extract(OLD.payload,'$.value.agent')
    ) WHERE selector IS NOT NULL
  ) WHERE OLD.deleted=0 AND NEW.deleted=1
  HAVING COUNT(*)>0
  ON CONFLICT(key) DO UPDATE SET value=excluded.value;
  INSERT INTO sync_entity_meta(key,value)
  SELECT 'entity_scope_visibility:' || old.value,CAST(NEW.seq AS TEXT)
  FROM sync_entity_meta AS provenance,json_each(provenance.value) AS old
  WHERE provenance.key='entity_scope_tombstone:' || json_array(NEW.collection,NEW.id)
    AND OLD.deleted=1 AND NEW.deleted=0
    AND EXISTS (
      SELECT value FROM json_each(provenance.value)
      EXCEPT SELECT identity FROM (
        SELECT json_array(NEW.collection,json_extract(NEW.payload,'$.value.rootId')) AS identity
          WHERE json_extract(NEW.payload,'$.value.rootId') IS NOT NULL
        UNION SELECT json_array(NEW.collection,selector,'team') FROM (
          SELECT json_extract(NEW.payload,'$.value.agent') AS selector
          UNION SELECT json_extract(payload,'$.value.rootId') FROM sync_entities
            WHERE collection='agentSummary' AND deleted=0 AND id=json_extract(NEW.payload,'$.value.agent')
        ) WHERE selector IS NOT NULL
      )
    )
  ON CONFLICT(key) DO UPDATE SET value=CAST(MAX(CAST(sync_entity_meta.value AS INTEGER),CAST(excluded.value AS INTEGER)) AS TEXT);
  DELETE FROM sync_entity_meta
  WHERE key='entity_scope_tombstone:' || json_array(NEW.collection,NEW.id) AND NEW.deleted=0;
END;
CREATE TRIGGER sync_entity_scope_tombstone_delete
AFTER DELETE ON sync_entities BEGIN
  DELETE FROM sync_entity_meta WHERE key='entity_scope_tombstone:' || json_array(OLD.collection,OLD.id);
END;
CREATE TRIGGER sync_entity_event_INSERT
          AFTER INSERT ON runtime_events  BEGIN
          INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted)
          VALUES ('event',NEW.id,(SELECT max(seq)+1 FROM (
                    SELECT COALESCE(MAX(seq),0) seq FROM sync_entities UNION ALL
                    SELECT MAX(COALESCE(MAX(seq),0),COALESCE((SELECT value FROM sync_generation WHERE id=1),0)) FROM sync_documents UNION ALL
                    SELECT COALESCE(MAX(seq),0) FROM durable_rows)),
                  sync_entity_hash(sync_entity_payload('event',NEW.id,json_object('id',NEW.id,'agent',NEW.agent,'kind',NEW.kind,'status',NEW.status,'created',NEW.created,'error',NEW.error),0)),sync_entity_payload('event',NEW.id,json_object('id',NEW.id,'agent',NEW.agent,'kind',NEW.kind,'status',NEW.status,'created',NEW.created,'error',NEW.error),0),0)
          ON CONFLICT(collection,id) DO UPDATE SET
            seq=excluded.seq,hash=excluded.hash,payload=excluded.payload,deleted=excluded.deleted
          WHERE sync_entities.hash!=excluded.hash OR sync_entities.deleted!=excluded.deleted;
          INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted)
        SELECT 'event',e.id,(SELECT max(seq) FROM (
        SELECT COALESCE(MAX(seq),0) seq FROM sync_entities UNION ALL
        SELECT MAX(COALESCE(MAX(seq),0),COALESCE((SELECT value FROM sync_generation WHERE id=1),0)) FROM sync_documents UNION ALL
                    SELECT COALESCE(MAX(seq),0) FROM durable_rows))+ROW_NUMBER() OVER (ORDER BY e.created DESC,e.id),
               sync_entity_hash(sync_entity_payload('event',e.id,json_object('id',e.id,'agent',e.agent,'kind',e.kind,'status',e.status,'created',e.created,'error',e.error),0)),sync_entity_payload('event',e.id,json_object('id',e.id,'agent',e.agent,'kind',e.kind,'status',e.status,'created',e.created,'error',e.error),0),0
        FROM (SELECT id,agent,kind,status,created,error FROM runtime_events
        ORDER BY created DESC,id LIMIT 200) e
        WHERE 0 OR NOT EXISTS (SELECT 1 FROM sync_entities s
            WHERE s.collection='event' AND s.id=e.id AND s.deleted=0)
        ON CONFLICT(collection,id) DO UPDATE SET
            seq=excluded.seq,hash=excluded.hash,payload=excluded.payload,deleted=excluded.deleted
        WHERE sync_entities.hash!=excluded.hash OR sync_entities.deleted!=excluded.deleted;
          INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted)
        SELECT 'event',s.id,(SELECT max(seq) FROM (
        SELECT COALESCE(MAX(seq),0) seq FROM sync_entities UNION ALL
        SELECT MAX(COALESCE(MAX(seq),0),COALESCE((SELECT value FROM sync_generation WHERE id=1),0)) FROM sync_documents UNION ALL
                    SELECT COALESCE(MAX(seq),0) FROM durable_rows))+ROW_NUMBER() OVER (ORDER BY s.id),
               sync_entity_hash(sync_entity_payload('event',s.id,'{}',1)),sync_entity_payload('event',s.id,'{}',1),1
        FROM sync_entities s
        WHERE s.collection='event' AND s.deleted=0
          AND s.id NOT IN (SELECT id FROM (SELECT id,agent,kind,status,created,error FROM runtime_events
        ORDER BY created DESC,id LIMIT 200))
        ON CONFLICT(collection,id) DO UPDATE SET
            seq=excluded.seq,hash=excluded.hash,payload=excluded.payload,deleted=excluded.deleted
        WHERE sync_entities.hash!=excluded.hash OR sync_entities.deleted!=excluded.deleted;
          INSERT INTO sync_entity_meta(key,value)
        VALUES('event_window_seq',CAST((SELECT COALESCE(MAX(seq),0) FROM sync_entities
            WHERE collection='event') AS TEXT))
        ON CONFLICT(key) DO UPDATE SET value=excluded.value
        WHERE sync_entity_meta.value!=excluded.value;
        END;
CREATE TRIGGER sync_entity_event_UPDATE
          AFTER UPDATE OF id,agent,kind,status,created,error ON runtime_events WHEN (OLD.id IS NOT NEW.id OR OLD.agent IS NOT NEW.agent OR OLD.kind IS NOT NEW.kind OR OLD.status IS NOT NEW.status OR OLD.created IS NOT NEW.created OR OLD.error IS NOT NEW.error) AND NOT (CASE
        WHEN OLD.id IS NEW.id AND OLD.created IS NEW.created THEN 0
        ELSE (WITH old_member AS (SELECT EXISTS (SELECT 1 FROM (SELECT id FROM (
        SELECT id,created FROM runtime_events WHERE id!=NEW.id
        UNION ALL SELECT OLD.id,OLD.created
    ) ORDER BY created DESC,id LIMIT 200) e WHERE e.id=OLD.id) AS present),
                   new_member AS (SELECT EXISTS (SELECT 1 FROM (SELECT id FROM runtime_events ORDER BY created DESC,id LIMIT 200) e WHERE e.id=NEW.id) AS present)
              SELECT CASE WHEN OLD.id IS NEW.id
                          THEN old_member.present != new_member.present
                          ELSE old_member.present OR new_member.present
                     END
              FROM old_member,new_member)
    END) BEGIN
          INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted)
          VALUES ('event',NEW.id,(SELECT max(seq)+1 FROM (
                    SELECT COALESCE(MAX(seq),0) seq FROM sync_entities UNION ALL
                    SELECT MAX(COALESCE(MAX(seq),0),COALESCE((SELECT value FROM sync_generation WHERE id=1),0)) FROM sync_documents UNION ALL
                    SELECT COALESCE(MAX(seq),0) FROM durable_rows)),
                  sync_entity_hash(sync_entity_payload('event',NEW.id,json_object('id',NEW.id,'agent',NEW.agent,'kind',NEW.kind,'status',NEW.status,'created',NEW.created,'error',NEW.error),0)),sync_entity_payload('event',NEW.id,json_object('id',NEW.id,'agent',NEW.agent,'kind',NEW.kind,'status',NEW.status,'created',NEW.created,'error',NEW.error),0),0)
          ON CONFLICT(collection,id) DO UPDATE SET
            seq=excluded.seq,hash=excluded.hash,payload=excluded.payload,deleted=excluded.deleted
          WHERE sync_entities.hash!=excluded.hash OR sync_entities.deleted!=excluded.deleted;
          INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted)
        SELECT 'event',NEW.id,(SELECT max(seq)+1 FROM (
                    SELECT COALESCE(MAX(seq),0) seq FROM sync_entities UNION ALL
                    SELECT MAX(COALESCE(MAX(seq),0),COALESCE((SELECT value FROM sync_generation WHERE id=1),0)) FROM sync_documents UNION ALL
                    SELECT COALESCE(MAX(seq),0) FROM durable_rows)),
               sync_entity_hash(sync_entity_payload('event',NEW.id,'{}',1)),sync_entity_payload('event',NEW.id,'{}',1),1
        WHERE NOT EXISTS (SELECT 1 FROM (SELECT id FROM runtime_events ORDER BY created DESC,id LIMIT 200) e WHERE e.id=NEW.id)
        ON CONFLICT(collection,id) DO UPDATE SET
            seq=excluded.seq,hash=excluded.hash,payload=excluded.payload,deleted=excluded.deleted
        WHERE sync_entities.hash!=excluded.hash OR sync_entities.deleted!=excluded.deleted;
          INSERT INTO sync_entity_meta(key,value)
        VALUES('event_window_seq',CAST((SELECT COALESCE(MAX(seq),0) FROM sync_entities
            WHERE collection='event') AS TEXT))
        ON CONFLICT(key) DO UPDATE SET value=excluded.value
        WHERE sync_entity_meta.value!=excluded.value;
        END;
CREATE TRIGGER sync_entity_event_UPDATE_WINDOW
          AFTER UPDATE OF id,agent,kind,status,created,error ON runtime_events WHEN CASE
        WHEN OLD.id IS NEW.id AND OLD.created IS NEW.created THEN 0
        ELSE (WITH old_member AS (SELECT EXISTS (SELECT 1 FROM (SELECT id FROM (
        SELECT id,created FROM runtime_events WHERE id!=NEW.id
        UNION ALL SELECT OLD.id,OLD.created
    ) ORDER BY created DESC,id LIMIT 200) e WHERE e.id=OLD.id) AS present),
                   new_member AS (SELECT EXISTS (SELECT 1 FROM (SELECT id FROM runtime_events ORDER BY created DESC,id LIMIT 200) e WHERE e.id=NEW.id) AS present)
              SELECT CASE WHEN OLD.id IS NEW.id
                          THEN old_member.present != new_member.present
                          ELSE old_member.present OR new_member.present
                     END
              FROM old_member,new_member)
    END BEGIN
          INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted)
          VALUES ('event',NEW.id,(SELECT max(seq)+1 FROM (
                    SELECT COALESCE(MAX(seq),0) seq FROM sync_entities UNION ALL
                    SELECT MAX(COALESCE(MAX(seq),0),COALESCE((SELECT value FROM sync_generation WHERE id=1),0)) FROM sync_documents UNION ALL
                    SELECT COALESCE(MAX(seq),0) FROM durable_rows)),
                  sync_entity_hash(sync_entity_payload('event',NEW.id,json_object('id',NEW.id,'agent',NEW.agent,'kind',NEW.kind,'status',NEW.status,'created',NEW.created,'error',NEW.error),0)),sync_entity_payload('event',NEW.id,json_object('id',NEW.id,'agent',NEW.agent,'kind',NEW.kind,'status',NEW.status,'created',NEW.created,'error',NEW.error),0),0)
          ON CONFLICT(collection,id) DO UPDATE SET
            seq=excluded.seq,hash=excluded.hash,payload=excluded.payload,deleted=excluded.deleted
          WHERE sync_entities.hash!=excluded.hash OR sync_entities.deleted!=excluded.deleted;
          INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted)
        SELECT 'event',e.id,(SELECT max(seq) FROM (
        SELECT COALESCE(MAX(seq),0) seq FROM sync_entities UNION ALL
        SELECT MAX(COALESCE(MAX(seq),0),COALESCE((SELECT value FROM sync_generation WHERE id=1),0)) FROM sync_documents UNION ALL
                    SELECT COALESCE(MAX(seq),0) FROM durable_rows))+ROW_NUMBER() OVER (ORDER BY e.created DESC,e.id),
               sync_entity_hash(sync_entity_payload('event',e.id,json_object('id',e.id,'agent',e.agent,'kind',e.kind,'status',e.status,'created',e.created,'error',e.error),0)),sync_entity_payload('event',e.id,json_object('id',e.id,'agent',e.agent,'kind',e.kind,'status',e.status,'created',e.created,'error',e.error),0),0
        FROM (SELECT id,agent,kind,status,created,error FROM runtime_events
        ORDER BY created DESC,id LIMIT 200) e
        WHERE 0 OR NOT EXISTS (SELECT 1 FROM sync_entities s
            WHERE s.collection='event' AND s.id=e.id AND s.deleted=0)
        ON CONFLICT(collection,id) DO UPDATE SET
            seq=excluded.seq,hash=excluded.hash,payload=excluded.payload,deleted=excluded.deleted
        WHERE sync_entities.hash!=excluded.hash OR sync_entities.deleted!=excluded.deleted;
          INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted)
        SELECT 'event',s.id,(SELECT max(seq) FROM (
        SELECT COALESCE(MAX(seq),0) seq FROM sync_entities UNION ALL
        SELECT MAX(COALESCE(MAX(seq),0),COALESCE((SELECT value FROM sync_generation WHERE id=1),0)) FROM sync_documents UNION ALL
                    SELECT COALESCE(MAX(seq),0) FROM durable_rows))+ROW_NUMBER() OVER (ORDER BY s.id),
               sync_entity_hash(sync_entity_payload('event',s.id,'{}',1)),sync_entity_payload('event',s.id,'{}',1),1
        FROM sync_entities s
        WHERE s.collection='event' AND s.deleted=0
          AND s.id NOT IN (SELECT id FROM (SELECT id,agent,kind,status,created,error FROM runtime_events
        ORDER BY created DESC,id LIMIT 200))
        ON CONFLICT(collection,id) DO UPDATE SET
            seq=excluded.seq,hash=excluded.hash,payload=excluded.payload,deleted=excluded.deleted
        WHERE sync_entities.hash!=excluded.hash OR sync_entities.deleted!=excluded.deleted;
          INSERT INTO sync_entity_meta(key,value)
        VALUES('event_window_seq',CAST((SELECT COALESCE(MAX(seq),0) FROM sync_entities
            WHERE collection='event') AS TEXT))
        ON CONFLICT(key) DO UPDATE SET value=excluded.value
        WHERE sync_entity_meta.value!=excluded.value;
        END;
CREATE TRIGGER sync_entity_event_DELETE
          AFTER DELETE ON runtime_events  BEGIN
          INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted)
          VALUES ('event',OLD.id,(SELECT max(seq)+1 FROM (
                    SELECT COALESCE(MAX(seq),0) seq FROM sync_entities UNION ALL
                    SELECT MAX(COALESCE(MAX(seq),0),COALESCE((SELECT value FROM sync_generation WHERE id=1),0)) FROM sync_documents UNION ALL
                    SELECT COALESCE(MAX(seq),0) FROM durable_rows)),
                  sync_entity_hash(sync_entity_payload('event',OLD.id,'{}',1)),sync_entity_payload('event',OLD.id,'{}',1),1)
          ON CONFLICT(collection,id) DO UPDATE SET
            seq=excluded.seq,hash=excluded.hash,payload=excluded.payload,deleted=excluded.deleted
          WHERE sync_entities.hash!=excluded.hash OR sync_entities.deleted!=excluded.deleted;
          INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted)
        SELECT 'event',e.id,(SELECT max(seq) FROM (
        SELECT COALESCE(MAX(seq),0) seq FROM sync_entities UNION ALL
        SELECT MAX(COALESCE(MAX(seq),0),COALESCE((SELECT value FROM sync_generation WHERE id=1),0)) FROM sync_documents UNION ALL
                    SELECT COALESCE(MAX(seq),0) FROM durable_rows))+ROW_NUMBER() OVER (ORDER BY e.created DESC,e.id),
               sync_entity_hash(sync_entity_payload('event',e.id,json_object('id',e.id,'agent',e.agent,'kind',e.kind,'status',e.status,'created',e.created,'error',e.error),0)),sync_entity_payload('event',e.id,json_object('id',e.id,'agent',e.agent,'kind',e.kind,'status',e.status,'created',e.created,'error',e.error),0),0
        FROM (SELECT id,agent,kind,status,created,error FROM runtime_events
        ORDER BY created DESC,id LIMIT 200) e
        WHERE 0 OR NOT EXISTS (SELECT 1 FROM sync_entities s
            WHERE s.collection='event' AND s.id=e.id AND s.deleted=0)
        ON CONFLICT(collection,id) DO UPDATE SET
            seq=excluded.seq,hash=excluded.hash,payload=excluded.payload,deleted=excluded.deleted
        WHERE sync_entities.hash!=excluded.hash OR sync_entities.deleted!=excluded.deleted;
          INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted)
        SELECT 'event',s.id,(SELECT max(seq) FROM (
        SELECT COALESCE(MAX(seq),0) seq FROM sync_entities UNION ALL
        SELECT MAX(COALESCE(MAX(seq),0),COALESCE((SELECT value FROM sync_generation WHERE id=1),0)) FROM sync_documents UNION ALL
                    SELECT COALESCE(MAX(seq),0) FROM durable_rows))+ROW_NUMBER() OVER (ORDER BY s.id),
               sync_entity_hash(sync_entity_payload('event',s.id,'{}',1)),sync_entity_payload('event',s.id,'{}',1),1
        FROM sync_entities s
        WHERE s.collection='event' AND s.deleted=0
          AND s.id NOT IN (SELECT id FROM (SELECT id,agent,kind,status,created,error FROM runtime_events
        ORDER BY created DESC,id LIMIT 200))
        ON CONFLICT(collection,id) DO UPDATE SET
            seq=excluded.seq,hash=excluded.hash,payload=excluded.payload,deleted=excluded.deleted
        WHERE sync_entities.hash!=excluded.hash OR sync_entities.deleted!=excluded.deleted;
          INSERT INTO sync_entity_meta(key,value)
        VALUES('event_window_seq',CAST((SELECT COALESCE(MAX(seq),0) FROM sync_entities
            WHERE collection='event') AS TEXT))
        ON CONFLICT(key) DO UPDATE SET value=excluded.value
        WHERE sync_entity_meta.value!=excluded.value;
        END;
CREATE TRIGGER runtime_agent_volatile_owner_delete AFTER DELETE ON runtime_agents BEGIN DELETE FROM runtime_agent_volatile WHERE id=OLD.id; END;
CREATE TRIGGER work_history_results_insert AFTER INSERT ON runtime_work BEGIN
             SELECT sync_work_history_change(NEW.id,'results',j.key) FROM json_each(NEW.record,'$.results') j; END;
CREATE TRIGGER work_history_results_update AFTER UPDATE OF record ON runtime_work
             WHEN json_extract(OLD.record,'$.results') IS NOT json_extract(NEW.record,'$.results') OR json_extract(OLD.record,'$.rootId') IS NOT json_extract(NEW.record,'$.rootId') BEGIN
             SELECT sync_work_history_change(NEW.id,'results',changed.key) FROM (
               WITH old_values AS MATERIALIZED (SELECT key,value,type FROM json_each(OLD.record,'$.results')),
                    new_values AS MATERIALIZED (SELECT key,value,type FROM json_each(NEW.record,'$.results'))
               SELECT n.key FROM new_values n LEFT JOIN old_values o USING(key)
               WHERE o.key IS NULL OR o.type IS NOT n.type OR o.value IS NOT n.value OR json_extract(OLD.record,'$.rootId') IS NOT json_extract(NEW.record,'$.rootId')
               UNION SELECT o.key FROM old_values o LEFT JOIN new_values n USING(key) WHERE n.key IS NULL
             ) changed; END;
CREATE TRIGGER work_history_decisions_insert AFTER INSERT ON runtime_work BEGIN
             SELECT sync_work_history_change(NEW.id,'decisions',j.key) FROM json_each(NEW.record,'$.decisions') j; END;
CREATE TRIGGER work_history_decisions_update AFTER UPDATE OF record ON runtime_work
             WHEN json_extract(OLD.record,'$.decisions') IS NOT json_extract(NEW.record,'$.decisions') OR json_extract(OLD.record,'$.rootId') IS NOT json_extract(NEW.record,'$.rootId') BEGIN
             SELECT sync_work_history_change(NEW.id,'decisions',changed.key) FROM (
               WITH old_values AS MATERIALIZED (SELECT key,value,type FROM json_each(OLD.record,'$.decisions')),
                    new_values AS MATERIALIZED (SELECT key,value,type FROM json_each(NEW.record,'$.decisions'))
               SELECT n.key FROM new_values n LEFT JOIN old_values o USING(key)
               WHERE o.key IS NULL OR o.type IS NOT n.type OR o.value IS NOT n.value OR json_extract(OLD.record,'$.rootId') IS NOT json_extract(NEW.record,'$.rootId')
               UNION SELECT o.key FROM old_values o LEFT JOIN new_values n USING(key) WHERE n.key IS NULL
             ) changed; END;
CREATE TRIGGER work_history_releases_insert AFTER INSERT ON runtime_work BEGIN
             SELECT sync_work_history_change(NEW.id,'releases',j.key) FROM json_each(NEW.record,'$.releases') j; END;
CREATE TRIGGER work_history_releases_update AFTER UPDATE OF record ON runtime_work
             WHEN json_extract(OLD.record,'$.releases') IS NOT json_extract(NEW.record,'$.releases') OR json_extract(OLD.record,'$.rootId') IS NOT json_extract(NEW.record,'$.rootId') BEGIN
             SELECT sync_work_history_change(NEW.id,'releases',changed.key) FROM (
               WITH old_values AS MATERIALIZED (SELECT key,value,type FROM json_each(OLD.record,'$.releases')),
                    new_values AS MATERIALIZED (SELECT key,value,type FROM json_each(NEW.record,'$.releases'))
               SELECT n.key FROM new_values n LEFT JOIN old_values o USING(key)
               WHERE o.key IS NULL OR o.type IS NOT n.type OR o.value IS NOT n.value OR json_extract(OLD.record,'$.rootId') IS NOT json_extract(NEW.record,'$.rootId')
               UNION SELECT o.key FROM old_values o LEFT JOIN new_values n USING(key) WHERE n.key IS NULL
             ) changed; END;
CREATE TRIGGER work_identity_insert AFTER INSERT ON runtime_work BEGIN SELECT sync_work_identity_change(NULL,NEW.id); END;
CREATE TRIGGER work_identity_update AFTER UPDATE OF record ON runtime_work
         WHEN json_extract(OLD.record,'$.owner') IS NOT json_extract(NEW.record,'$.owner') OR json_extract(OLD.record,'$.version') IS NOT json_extract(NEW.record,'$.version') BEGIN SELECT sync_work_identity_change(NULL,NEW.id); END;
CREATE TRIGGER work_owner_identity_update AFTER UPDATE OF record ON runtime_agents
         WHEN json_extract(OLD.record,'$.epoch') IS NOT json_extract(NEW.record,'$.epoch')
           OR json_extract(OLD.record,'$.accountKey') IS NOT json_extract(NEW.record,'$.accountKey')
           OR json_extract(OLD.record,'$.threadId') IS NOT json_extract(NEW.record,'$.threadId')
           OR json_extract(OLD.record,'$.turnId') IS NOT json_extract(NEW.record,'$.turnId')
         BEGIN SELECT sync_work_identity_change(NEW.id,NULL); END;
CREATE TRIGGER work_attempt_identity_insert AFTER INSERT ON runtime_execution_runs BEGIN SELECT sync_work_identity_change(NEW.agent,NULL); END;
CREATE TRIGGER work_attempt_identity_delete AFTER DELETE ON runtime_execution_runs BEGIN SELECT sync_work_identity_change(OLD.agent,NULL); END;
CREATE TRIGGER work_attempt_identity_update AFTER UPDATE ON runtime_execution_runs
         WHEN OLD.agent IS NOT NEW.agent OR OLD.account IS NOT NEW.account OR OLD.epoch IS NOT NEW.epoch OR OLD.thread IS NOT NEW.thread OR OLD.turn IS NOT NEW.turn OR OLD.created IS NOT NEW.created
           OR json_extract(OLD.record,'$.latestAttemptId') IS NOT json_extract(NEW.record,'$.latestAttemptId')
           OR json_extract(OLD.record,'$.status') IS NOT json_extract(NEW.record,'$.status')
         BEGIN SELECT sync_work_identity_change(OLD.agent,NULL); SELECT sync_work_identity_change(NEW.agent,NULL); END;
CREATE TRIGGER summary_plan_delete AFTER DELETE ON runtime_plans BEGIN
             INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted)
             SELECT 'plan',OLD.id,(SELECT max(seq)+1 FROM (
                 SELECT COALESCE(MAX(seq),0) seq FROM sync_entities UNION ALL
                 SELECT MAX(COALESCE(MAX(seq),0),COALESCE((SELECT value FROM sync_generation WHERE id=1),0)) FROM sync_documents UNION ALL
                 SELECT COALESCE(MAX(seq),0) FROM durable_rows)),
                 sync_entity_hash(json_object('collection','plan','id',OLD.id,'value',json('{}'))),json_object('collection','plan','id',OLD.id,'value',json('{}')),1
             WHERE EXISTS(SELECT 1 FROM sync_entities WHERE collection='plan' AND id=OLD.id AND deleted=0)
             ON CONFLICT(collection,id) DO UPDATE SET seq=excluded.seq,hash=excluded.hash,payload=excluded.payload,deleted=1;
             END;
CREATE TRIGGER summary_annotation_delete AFTER DELETE ON runtime_annotations BEGIN
             INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted)
             SELECT 'annotation',OLD.id,(SELECT max(seq)+1 FROM (
                 SELECT COALESCE(MAX(seq),0) seq FROM sync_entities UNION ALL
                 SELECT MAX(COALESCE(MAX(seq),0),COALESCE((SELECT value FROM sync_generation WHERE id=1),0)) FROM sync_documents UNION ALL
                 SELECT COALESCE(MAX(seq),0) FROM durable_rows)),
                 sync_entity_hash(json_object('collection','annotation','id',OLD.id,'value',json('{}'))),json_object('collection','annotation','id',OLD.id,'value',json('{}')),1
             WHERE EXISTS(SELECT 1 FROM sync_entities WHERE collection='annotation' AND id=OLD.id AND deleted=0)
             ON CONFLICT(collection,id) DO UPDATE SET seq=excluded.seq,hash=excluded.hash,payload=excluded.payload,deleted=1;
             END;
CREATE TRIGGER summary_messageReceipt_delete AFTER DELETE ON runtime_events BEGIN
             INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted)
             SELECT 'messageReceipt',OLD.id,(SELECT max(seq)+1 FROM (
                 SELECT COALESCE(MAX(seq),0) seq FROM sync_entities UNION ALL
                 SELECT MAX(COALESCE(MAX(seq),0),COALESCE((SELECT value FROM sync_generation WHERE id=1),0)) FROM sync_documents UNION ALL
                 SELECT COALESCE(MAX(seq),0) FROM durable_rows)),
                 sync_entity_hash(json_object('collection','messageReceipt','id',OLD.id,'value',json('{}'))),json_object('collection','messageReceipt','id',OLD.id,'value',json('{}')),1
             WHERE EXISTS(SELECT 1 FROM sync_entities WHERE collection='messageReceipt' AND id=OLD.id AND deleted=0)
             ON CONFLICT(collection,id) DO UPDATE SET seq=excluded.seq,hash=excluded.hash,payload=excluded.payload,deleted=1;
             END;
CREATE TRIGGER summary_task_delete AFTER DELETE ON runtime_tasks BEGIN
             INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted)
             SELECT 'task',OLD.id,(SELECT max(seq)+1 FROM (
                 SELECT COALESCE(MAX(seq),0) seq FROM sync_entities UNION ALL
                 SELECT MAX(COALESCE(MAX(seq),0),COALESCE((SELECT value FROM sync_generation WHERE id=1),0)) FROM sync_documents UNION ALL
                 SELECT COALESCE(MAX(seq),0) FROM durable_rows)),
                 sync_entity_hash(json_object('collection','task','id',OLD.id,'value',json('{}'))),json_object('collection','task','id',OLD.id,'value',json('{}')),1
             WHERE EXISTS(SELECT 1 FROM sync_entities WHERE collection='task' AND id=OLD.id AND deleted=0)
             ON CONFLICT(collection,id) DO UPDATE SET seq=excluded.seq,hash=excluded.hash,payload=excluded.payload,deleted=1;
             END;
CREATE TRIGGER summary_questionHistory_delete AFTER DELETE ON runtime_requests BEGIN
             INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted)
             SELECT 'questionHistory',OLD.id,(SELECT max(seq)+1 FROM (
                 SELECT COALESCE(MAX(seq),0) seq FROM sync_entities UNION ALL
                 SELECT MAX(COALESCE(MAX(seq),0),COALESCE((SELECT value FROM sync_generation WHERE id=1),0)) FROM sync_documents UNION ALL
                 SELECT COALESCE(MAX(seq),0) FROM durable_rows)),
                 sync_entity_hash(json_object('collection','questionHistory','id',OLD.id,'value',json('{}'))),json_object('collection','questionHistory','id',OLD.id,'value',json('{}')),1
             WHERE EXISTS(SELECT 1 FROM sync_entities WHERE collection='questionHistory' AND id=OLD.id AND deleted=0)
             ON CONFLICT(collection,id) DO UPDATE SET seq=excluded.seq,hash=excluded.hash,payload=excluded.payload,deleted=1;
             END;
CREATE TRIGGER summary_retainedWorktree_delete AFTER DELETE ON runtime_agents BEGIN
             INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted)
             SELECT 'retainedWorktree',OLD.id,(SELECT max(seq)+1 FROM (
                 SELECT COALESCE(MAX(seq),0) seq FROM sync_entities UNION ALL
                 SELECT MAX(COALESCE(MAX(seq),0),COALESCE((SELECT value FROM sync_generation WHERE id=1),0)) FROM sync_documents UNION ALL
                 SELECT COALESCE(MAX(seq),0) FROM durable_rows)),
                 sync_entity_hash(json_object('collection','retainedWorktree','id',OLD.id,'value',json('{}'))),json_object('collection','retainedWorktree','id',OLD.id,'value',json('{}')),1
             WHERE EXISTS(SELECT 1 FROM sync_entities WHERE collection='retainedWorktree' AND id=OLD.id AND deleted=0)
             ON CONFLICT(collection,id) DO UPDATE SET seq=excluded.seq,hash=excluded.hash,payload=excluded.payload,deleted=1;
             END;
CREATE TRIGGER summary_receipt_meta_delete AFTER DELETE ON runtime_event_meta BEGIN
         UPDATE runtime_events SET id=id WHERE id=OLD.id AND kind='user'; END;
CREATE TRIGGER summary_receipt_item_delete AFTER DELETE ON runtime_items BEGIN
         UPDATE runtime_events SET id=id WHERE kind='user' AND agent=OLD.agent AND (
            OLD.id=agent || ':' || id OR id IN (SELECT id FROM runtime_event_meta
                WHERE json_extract(record,'$.transcriptItemId')=OLD.id)); END;
CREATE TRIGGER runtime_agent_volatile_journal_insert AFTER INSERT ON runtime_agent_volatile BEGIN UPDATE runtime_agent_record_generation SET value=value+1 WHERE id=1; UPDATE runtime_scheduler_agent_change_state SET entries=entries+CASE WHEN EXISTS(SELECT 1 FROM runtime_scheduler_agent_changes WHERE id=NEW.id) THEN 0 ELSE 1 END,total=total-COALESCE((SELECT generation FROM runtime_scheduler_agent_changes WHERE id=NEW.id),0)+(SELECT value FROM runtime_agent_record_generation WHERE id=1) WHERE id=1; INSERT INTO runtime_scheduler_agent_changes(id,generation) SELECT NEW.id,value FROM runtime_agent_record_generation WHERE id=1 ON CONFLICT(id) DO UPDATE SET generation=excluded.generation; UPDATE runtime_scheduler_agent_change_state SET generation=(SELECT value FROM runtime_agent_record_generation WHERE id=1) WHERE id=1; END;
CREATE TRIGGER runtime_agent_volatile_journal_update AFTER UPDATE ON runtime_agent_volatile BEGIN UPDATE runtime_agent_record_generation SET value=value+1 WHERE id=1; UPDATE runtime_scheduler_agent_change_state SET entries=entries+CASE WHEN EXISTS(SELECT 1 FROM runtime_scheduler_agent_changes WHERE id=NEW.id) THEN 0 ELSE 1 END,total=total-COALESCE((SELECT generation FROM runtime_scheduler_agent_changes WHERE id=NEW.id),0)+(SELECT value FROM runtime_agent_record_generation WHERE id=1) WHERE id=1; INSERT INTO runtime_scheduler_agent_changes(id,generation) SELECT NEW.id,value FROM runtime_agent_record_generation WHERE id=1 ON CONFLICT(id) DO UPDATE SET generation=excluded.generation; UPDATE runtime_scheduler_agent_change_state SET generation=(SELECT value FROM runtime_agent_record_generation WHERE id=1) WHERE id=1; END;
CREATE TRIGGER runtime_agent_volatile_transcript_insert AFTER INSERT ON runtime_agent_volatile BEGIN INSERT INTO runtime_transcript_revisions(agent,revision) VALUES (NEW.id,1) ON CONFLICT(agent) DO UPDATE SET revision=revision+1; END;
CREATE TRIGGER runtime_agent_volatile_transcript_update AFTER UPDATE ON runtime_agent_volatile BEGIN INSERT INTO runtime_transcript_revisions(agent,revision) VALUES (NEW.id,1) ON CONFLICT(agent) DO UPDATE SET revision=revision+1; END;
CREATE TRIGGER runtime_annotation_revision AFTER UPDATE OF record ON runtime_annotations WHEN OLD.record IS NOT NEW.record BEGIN UPDATE runtime_annotations SET rev=OLD.rev+1 WHERE id=NEW.id; END;
CREATE TRIGGER runtime_chat_message_revision AFTER UPDATE OF room,sender,text,created,deliveries ON runtime_chat_messages WHEN OLD.room IS NOT NEW.room OR OLD.sender IS NOT NEW.sender OR OLD.text IS NOT NEW.text OR OLD.created IS NOT NEW.created OR OLD.deliveries IS NOT NEW.deliveries BEGIN UPDATE runtime_chat_messages SET rev=OLD.rev+1 WHERE seq=NEW.seq; END;
CREATE TRIGGER runtime_event_revision AFTER UPDATE OF status,text ON runtime_events
         WHEN OLD.status IS NOT NEW.status OR OLD.text IS NOT NEW.text
         BEGIN INSERT INTO runtime_event_revisions(id,rev) VALUES(NEW.id,2)
               ON CONFLICT(id) DO UPDATE SET rev=rev+1; END;
CREATE TRIGGER runtime_event_revision_cleanup AFTER DELETE ON runtime_events
         BEGIN DELETE FROM runtime_event_revisions WHERE id=OLD.id; END;
CREATE TRIGGER durable_view_log_insert AFTER INSERT ON sync_entities
         WHEN NEW.collection IN ('agent','agentSummary','agentDetail','retainedWorktree','questionHistory','request','work','workDetail','workResult','workDecision','workRelease') BEGIN INSERT INTO durable_view_log(collection,id,seq,deleted)
         VALUES(NEW.collection,NEW.id,NEW.seq,NEW.deleted)
         ON CONFLICT(collection,id) DO UPDATE SET seq=excluded.seq,deleted=excluded.deleted; END;
CREATE TRIGGER durable_view_log_update AFTER UPDATE ON sync_entities
         WHEN NEW.collection IN ('agent','agentSummary','agentDetail','retainedWorktree','questionHistory','request','work','workDetail','workResult','workDecision','workRelease') BEGIN INSERT INTO durable_view_log(collection,id,seq,deleted)
         VALUES(NEW.collection,NEW.id,NEW.seq,NEW.deleted)
         ON CONFLICT(collection,id) DO UPDATE SET seq=excluded.seq,deleted=excluded.deleted; END;
CREATE TRIGGER durable_view_log_delete AFTER DELETE ON sync_entities
         WHEN OLD.collection IN ('agent','agentSummary','agentDetail','retainedWorktree','questionHistory','request','work','workDetail','workResult','workDecision','workRelease') BEGIN
             DELETE FROM durable_view_log WHERE collection=OLD.collection AND id=OLD.id; END;
CREATE TRIGGER durable_runtime_agents_insert AFTER INSERT ON runtime_agents BEGIN
  INSERT INTO durable_rows(tbl,id,seq,deleted,at) VALUES('runtime_agents',NEW.id,(SELECT MAX(entity_seq,document_seq,generation,durable_seq)+1 FROM (
    SELECT COALESCE(MAX(seq),0) AS entity_seq FROM sync_entities),
    (SELECT COALESCE(MAX(seq),0) AS document_seq FROM sync_documents),
    (SELECT COALESCE(MAX(value),0) AS generation FROM sync_generation WHERE id=1),
    (SELECT COALESCE(MAX(seq),0) AS durable_seq FROM durable_rows)),0,0)
             ON CONFLICT(tbl,id) DO UPDATE SET seq=excluded.seq,deleted=excluded.deleted,at=excluded.at;
END;
CREATE TRIGGER durable_runtime_agents_update AFTER UPDATE ON runtime_agents BEGIN
  INSERT INTO durable_rows(tbl,id,seq,deleted,at) VALUES('runtime_agents',NEW.id,(SELECT MAX(entity_seq,document_seq,generation,durable_seq)+1 FROM (
    SELECT COALESCE(MAX(seq),0) AS entity_seq FROM sync_entities),
    (SELECT COALESCE(MAX(seq),0) AS document_seq FROM sync_documents),
    (SELECT COALESCE(MAX(value),0) AS generation FROM sync_generation WHERE id=1),
    (SELECT COALESCE(MAX(seq),0) AS durable_seq FROM durable_rows)),0,0)
             ON CONFLICT(tbl,id) DO UPDATE SET seq=excluded.seq,deleted=excluded.deleted,at=excluded.at;
  INSERT INTO durable_rows(tbl,id,seq,deleted,at)
    SELECT 'runtime_agents',OLD.id,(SELECT MAX(entity_seq,document_seq,generation,durable_seq)+1 FROM (
    SELECT COALESCE(MAX(seq),0) AS entity_seq FROM sync_entities),
    (SELECT COALESCE(MAX(seq),0) AS document_seq FROM sync_documents),
    (SELECT COALESCE(MAX(value),0) AS generation FROM sync_generation WHERE id=1),
    (SELECT COALESCE(MAX(seq),0) AS durable_seq FROM durable_rows)),1,CAST(strftime('%s','now') AS INTEGER) WHERE OLD.id IS NOT NEW.id
    ON CONFLICT(tbl,id) DO UPDATE SET seq=excluded.seq,deleted=1,at=excluded.at;
END;
CREATE TRIGGER durable_runtime_agents_delete AFTER DELETE ON runtime_agents BEGIN
  UPDATE durable_floor SET floor=MAX(floor,
    (SELECT MAX(seq) FROM durable_rows WHERE deleted=1 AND at<CAST(strftime('%s','now') AS INTEGER)-7776000))
  WHERE id=1 AND EXISTS(SELECT 1 FROM durable_rows WHERE deleted=1 AND at<CAST(strftime('%s','now') AS INTEGER)-7776000);
  DELETE FROM durable_rows WHERE deleted=1 AND at<CAST(strftime('%s','now') AS INTEGER)-7776000;
  INSERT INTO durable_rows(tbl,id,seq,deleted,at) VALUES('runtime_agents',OLD.id,(SELECT MAX(entity_seq,document_seq,generation,durable_seq)+1 FROM (
    SELECT COALESCE(MAX(seq),0) AS entity_seq FROM sync_entities),
    (SELECT COALESCE(MAX(seq),0) AS document_seq FROM sync_documents),
    (SELECT COALESCE(MAX(value),0) AS generation FROM sync_generation WHERE id=1),
    (SELECT COALESCE(MAX(seq),0) AS durable_seq FROM durable_rows)),1,CAST(strftime('%s','now') AS INTEGER))
             ON CONFLICT(tbl,id) DO UPDATE SET seq=excluded.seq,deleted=excluded.deleted,at=excluded.at;
END;
CREATE TRIGGER durable_runtime_requests_insert AFTER INSERT ON runtime_requests BEGIN
  INSERT INTO durable_rows(tbl,id,seq,deleted,at) VALUES('runtime_requests',NEW.id,(SELECT MAX(entity_seq,document_seq,generation,durable_seq)+1 FROM (
    SELECT COALESCE(MAX(seq),0) AS entity_seq FROM sync_entities),
    (SELECT COALESCE(MAX(seq),0) AS document_seq FROM sync_documents),
    (SELECT COALESCE(MAX(value),0) AS generation FROM sync_generation WHERE id=1),
    (SELECT COALESCE(MAX(seq),0) AS durable_seq FROM durable_rows)),0,0)
             ON CONFLICT(tbl,id) DO UPDATE SET seq=excluded.seq,deleted=excluded.deleted,at=excluded.at;
END;
CREATE TRIGGER durable_runtime_requests_update AFTER UPDATE ON runtime_requests BEGIN
  INSERT INTO durable_rows(tbl,id,seq,deleted,at) VALUES('runtime_requests',NEW.id,(SELECT MAX(entity_seq,document_seq,generation,durable_seq)+1 FROM (
    SELECT COALESCE(MAX(seq),0) AS entity_seq FROM sync_entities),
    (SELECT COALESCE(MAX(seq),0) AS document_seq FROM sync_documents),
    (SELECT COALESCE(MAX(value),0) AS generation FROM sync_generation WHERE id=1),
    (SELECT COALESCE(MAX(seq),0) AS durable_seq FROM durable_rows)),0,0)
             ON CONFLICT(tbl,id) DO UPDATE SET seq=excluded.seq,deleted=excluded.deleted,at=excluded.at;
  INSERT INTO durable_rows(tbl,id,seq,deleted,at)
    SELECT 'runtime_requests',OLD.id,(SELECT MAX(entity_seq,document_seq,generation,durable_seq)+1 FROM (
    SELECT COALESCE(MAX(seq),0) AS entity_seq FROM sync_entities),
    (SELECT COALESCE(MAX(seq),0) AS document_seq FROM sync_documents),
    (SELECT COALESCE(MAX(value),0) AS generation FROM sync_generation WHERE id=1),
    (SELECT COALESCE(MAX(seq),0) AS durable_seq FROM durable_rows)),1,CAST(strftime('%s','now') AS INTEGER) WHERE OLD.id IS NOT NEW.id
    ON CONFLICT(tbl,id) DO UPDATE SET seq=excluded.seq,deleted=1,at=excluded.at;
END;
CREATE TRIGGER durable_runtime_requests_delete AFTER DELETE ON runtime_requests BEGIN
  UPDATE durable_floor SET floor=MAX(floor,
    (SELECT MAX(seq) FROM durable_rows WHERE deleted=1 AND at<CAST(strftime('%s','now') AS INTEGER)-7776000))
  WHERE id=1 AND EXISTS(SELECT 1 FROM durable_rows WHERE deleted=1 AND at<CAST(strftime('%s','now') AS INTEGER)-7776000);
  DELETE FROM durable_rows WHERE deleted=1 AND at<CAST(strftime('%s','now') AS INTEGER)-7776000;
  INSERT INTO durable_rows(tbl,id,seq,deleted,at) VALUES('runtime_requests',OLD.id,(SELECT MAX(entity_seq,document_seq,generation,durable_seq)+1 FROM (
    SELECT COALESCE(MAX(seq),0) AS entity_seq FROM sync_entities),
    (SELECT COALESCE(MAX(seq),0) AS document_seq FROM sync_documents),
    (SELECT COALESCE(MAX(value),0) AS generation FROM sync_generation WHERE id=1),
    (SELECT COALESCE(MAX(seq),0) AS durable_seq FROM durable_rows)),1,CAST(strftime('%s','now') AS INTEGER))
             ON CONFLICT(tbl,id) DO UPDATE SET seq=excluded.seq,deleted=excluded.deleted,at=excluded.at;
END;
CREATE TRIGGER durable_runtime_work_insert AFTER INSERT ON runtime_work BEGIN
  INSERT INTO durable_rows(tbl,id,seq,deleted,at) VALUES('runtime_work',NEW.id,(SELECT MAX(entity_seq,document_seq,generation,durable_seq)+1 FROM (
    SELECT COALESCE(MAX(seq),0) AS entity_seq FROM sync_entities),
    (SELECT COALESCE(MAX(seq),0) AS document_seq FROM sync_documents),
    (SELECT COALESCE(MAX(value),0) AS generation FROM sync_generation WHERE id=1),
    (SELECT COALESCE(MAX(seq),0) AS durable_seq FROM durable_rows)),0,0)
             ON CONFLICT(tbl,id) DO UPDATE SET seq=excluded.seq,deleted=excluded.deleted,at=excluded.at;
END;
CREATE TRIGGER durable_runtime_work_update AFTER UPDATE ON runtime_work BEGIN
  INSERT INTO durable_rows(tbl,id,seq,deleted,at) VALUES('runtime_work',NEW.id,(SELECT MAX(entity_seq,document_seq,generation,durable_seq)+1 FROM (
    SELECT COALESCE(MAX(seq),0) AS entity_seq FROM sync_entities),
    (SELECT COALESCE(MAX(seq),0) AS document_seq FROM sync_documents),
    (SELECT COALESCE(MAX(value),0) AS generation FROM sync_generation WHERE id=1),
    (SELECT COALESCE(MAX(seq),0) AS durable_seq FROM durable_rows)),0,0)
             ON CONFLICT(tbl,id) DO UPDATE SET seq=excluded.seq,deleted=excluded.deleted,at=excluded.at;
  INSERT INTO durable_rows(tbl,id,seq,deleted,at)
    SELECT 'runtime_work',OLD.id,(SELECT MAX(entity_seq,document_seq,generation,durable_seq)+1 FROM (
    SELECT COALESCE(MAX(seq),0) AS entity_seq FROM sync_entities),
    (SELECT COALESCE(MAX(seq),0) AS document_seq FROM sync_documents),
    (SELECT COALESCE(MAX(value),0) AS generation FROM sync_generation WHERE id=1),
    (SELECT COALESCE(MAX(seq),0) AS durable_seq FROM durable_rows)),1,CAST(strftime('%s','now') AS INTEGER) WHERE OLD.id IS NOT NEW.id
    ON CONFLICT(tbl,id) DO UPDATE SET seq=excluded.seq,deleted=1,at=excluded.at;
END;
CREATE TRIGGER durable_runtime_work_delete AFTER DELETE ON runtime_work BEGIN
  UPDATE durable_floor SET floor=MAX(floor,
    (SELECT MAX(seq) FROM durable_rows WHERE deleted=1 AND at<CAST(strftime('%s','now') AS INTEGER)-7776000))
  WHERE id=1 AND EXISTS(SELECT 1 FROM durable_rows WHERE deleted=1 AND at<CAST(strftime('%s','now') AS INTEGER)-7776000);
  DELETE FROM durable_rows WHERE deleted=1 AND at<CAST(strftime('%s','now') AS INTEGER)-7776000;
  INSERT INTO durable_rows(tbl,id,seq,deleted,at) VALUES('runtime_work',OLD.id,(SELECT MAX(entity_seq,document_seq,generation,durable_seq)+1 FROM (
    SELECT COALESCE(MAX(seq),0) AS entity_seq FROM sync_entities),
    (SELECT COALESCE(MAX(seq),0) AS document_seq FROM sync_documents),
    (SELECT COALESCE(MAX(value),0) AS generation FROM sync_generation WHERE id=1),
    (SELECT COALESCE(MAX(seq),0) AS durable_seq FROM durable_rows)),1,CAST(strftime('%s','now') AS INTEGER))
             ON CONFLICT(tbl,id) DO UPDATE SET seq=excluded.seq,deleted=excluded.deleted,at=excluded.at;
END;
CREATE TRIGGER durable_runtime_items_insert AFTER INSERT ON runtime_items BEGIN
  INSERT INTO durable_rows(tbl,id,seq,deleted,at) VALUES('runtime_items',NEW.id,(SELECT MAX(entity_seq,document_seq,generation,durable_seq)+1 FROM (
    SELECT COALESCE(MAX(seq),0) AS entity_seq FROM sync_entities),
    (SELECT COALESCE(MAX(seq),0) AS document_seq FROM sync_documents),
    (SELECT COALESCE(MAX(value),0) AS generation FROM sync_generation WHERE id=1),
    (SELECT COALESCE(MAX(seq),0) AS durable_seq FROM durable_rows)),0,0)
             ON CONFLICT(tbl,id) DO UPDATE SET seq=excluded.seq,deleted=excluded.deleted,at=excluded.at;
END;
CREATE TRIGGER durable_runtime_items_update AFTER UPDATE ON runtime_items BEGIN
  INSERT INTO durable_rows(tbl,id,seq,deleted,at) VALUES('runtime_items',NEW.id,(SELECT MAX(entity_seq,document_seq,generation,durable_seq)+1 FROM (
    SELECT COALESCE(MAX(seq),0) AS entity_seq FROM sync_entities),
    (SELECT COALESCE(MAX(seq),0) AS document_seq FROM sync_documents),
    (SELECT COALESCE(MAX(value),0) AS generation FROM sync_generation WHERE id=1),
    (SELECT COALESCE(MAX(seq),0) AS durable_seq FROM durable_rows)),0,0)
             ON CONFLICT(tbl,id) DO UPDATE SET seq=excluded.seq,deleted=excluded.deleted,at=excluded.at;
  INSERT INTO durable_rows(tbl,id,seq,deleted,at)
    SELECT 'runtime_items',OLD.id,(SELECT MAX(entity_seq,document_seq,generation,durable_seq)+1 FROM (
    SELECT COALESCE(MAX(seq),0) AS entity_seq FROM sync_entities),
    (SELECT COALESCE(MAX(seq),0) AS document_seq FROM sync_documents),
    (SELECT COALESCE(MAX(value),0) AS generation FROM sync_generation WHERE id=1),
    (SELECT COALESCE(MAX(seq),0) AS durable_seq FROM durable_rows)),1,CAST(strftime('%s','now') AS INTEGER) WHERE OLD.id IS NOT NEW.id
    ON CONFLICT(tbl,id) DO UPDATE SET seq=excluded.seq,deleted=1,at=excluded.at;
END;
CREATE TRIGGER durable_runtime_items_delete AFTER DELETE ON runtime_items BEGIN
  UPDATE durable_floor SET floor=MAX(floor,
    (SELECT MAX(seq) FROM durable_rows WHERE deleted=1 AND at<CAST(strftime('%s','now') AS INTEGER)-7776000))
  WHERE id=1 AND EXISTS(SELECT 1 FROM durable_rows WHERE deleted=1 AND at<CAST(strftime('%s','now') AS INTEGER)-7776000);
  DELETE FROM durable_rows WHERE deleted=1 AND at<CAST(strftime('%s','now') AS INTEGER)-7776000;
  INSERT INTO durable_rows(tbl,id,seq,deleted,at) VALUES('runtime_items',OLD.id,(SELECT MAX(entity_seq,document_seq,generation,durable_seq)+1 FROM (
    SELECT COALESCE(MAX(seq),0) AS entity_seq FROM sync_entities),
    (SELECT COALESCE(MAX(seq),0) AS document_seq FROM sync_documents),
    (SELECT COALESCE(MAX(value),0) AS generation FROM sync_generation WHERE id=1),
    (SELECT COALESCE(MAX(seq),0) AS durable_seq FROM durable_rows)),1,CAST(strftime('%s','now') AS INTEGER))
             ON CONFLICT(tbl,id) DO UPDATE SET seq=excluded.seq,deleted=excluded.deleted,at=excluded.at;
END;
CREATE TRIGGER chats_check_insert BEFORE INSERT ON chats WHEN NOT (NEW.state IN ('active','paused','archived') AND NEW.status IN ('unknown','idle','queued','starting','running','waiting','waiting_on_monitor','approval','completed','interrupted','failed','blocked','paused','parked','abandoned','capacity-retry') AND COALESCE(NEW.auto_wake,0) IN (0,1) AND COALESCE(NEW.usage_resume_enabled,0) IN (0,1) AND (NEW.execution_mode IS NULL OR NEW.execution_mode IN ('native','host','vm')))
             BEGIN SELECT RAISE(ABORT,'invalid chats state'); END;
CREATE TRIGGER chats_check_update BEFORE UPDATE ON chats WHEN NOT (NEW.state IN ('active','paused','archived') AND NEW.status IN ('unknown','idle','queued','starting','running','waiting','waiting_on_monitor','approval','completed','interrupted','failed','blocked','paused','parked','abandoned','capacity-retry') AND COALESCE(NEW.auto_wake,0) IN (0,1) AND COALESCE(NEW.usage_resume_enabled,0) IN (0,1) AND (NEW.execution_mode IS NULL OR NEW.execution_mode IN ('native','host','vm')))
             BEGIN SELECT RAISE(ABORT,'invalid chats state'); END;
CREATE TRIGGER turns_check_insert BEFORE INSERT ON turns WHEN NOT (NEW.state IN ('starting','running','completed','failed','interrupted') AND (NEW.state='running')=(NEW.session IS NOT NULL) AND (NEW.interrupt_ticket IS NULL OR NEW.attempt IS NOT NULL))
             BEGIN SELECT RAISE(ABORT,'invalid turns state'); END;
CREATE TRIGGER turns_check_update BEFORE UPDATE ON turns WHEN NOT (NEW.state IN ('starting','running','completed','failed','interrupted') AND (NEW.state='running')=(NEW.session IS NOT NULL) AND (NEW.interrupt_ticket IS NULL OR NEW.attempt IS NOT NULL))
             BEGIN SELECT RAISE(ABORT,'invalid turns state'); END;
CREATE TRIGGER sessions_check_insert BEFORE INSERT ON sessions WHEN NOT (NEW.state IN ('starting','ready','lost'))
             BEGIN SELECT RAISE(ABORT,'invalid sessions state'); END;
CREATE TRIGGER sessions_check_update BEFORE UPDATE ON sessions WHEN NOT (NEW.state IN ('starting','ready','lost'))
             BEGIN SELECT RAISE(ABORT,'invalid sessions state'); END;
CREATE TRIGGER chats_owner_delete AFTER DELETE ON runtime_agents BEGIN
           UPDATE turns SET state='interrupted',session=NULL,attempt=NULL,interrupt_ticket=NULL
             WHERE chat=OLD.id AND state IN ('starting','running');
           DELETE FROM sessions WHERE chat=OLD.id;
           DELETE FROM chats WHERE id=OLD.id;
         END;
CREATE TRIGGER runtime_completed_turns_read_only_insert BEFORE INSERT ON runtime_completed_turns
             BEGIN SELECT RAISE(ABORT,'runtime_completed_turns is read only; use turns'); END;
CREATE TRIGGER runtime_completed_turns_read_only_update BEFORE UPDATE ON runtime_completed_turns
             BEGIN SELECT RAISE(ABORT,'runtime_completed_turns is read only; use turns'); END;
CREATE TRIGGER runtime_completed_turns_read_only_delete BEFORE DELETE ON runtime_completed_turns
             BEGIN SELECT RAISE(ABORT,'runtime_completed_turns is read only; use turns'); END;
CREATE INDEX sync_entities_seq ON sync_entities(seq);
CREATE INDEX sync_entities_collection_seq ON sync_entities(collection,seq);
CREATE INDEX sync_entities_non_transcript_seq ON sync_entities(seq)
        WHERE collection NOT LIKE 'transcript:%';
CREATE INDEX sync_entities_collection_deleted
        ON sync_entities(collection,deleted);
CREATE INDEX sync_entities_tombstone_order
        ON sync_entities(seq,collection,id)
        WHERE deleted=1 AND collection NOT LIKE 'transcript:%';
CREATE INDEX sync_scope_seq ON sync_documents(scope,seq);
CREATE INDEX runtime_server_outbox_due
                    ON runtime_server_outbox(state,next_at);
CREATE INDEX runtime_server_links_home_parent
                    ON runtime_server_links(json_extract(record,'$.parent'))
                    WHERE json_extract(record,'$.side')='home';
CREATE INDEX runtime_server_output_tools_expiry
                    ON runtime_server_output_tools(expires);
CREATE INDEX runtime_access_nonce_age ON runtime_access_nonces(created);
CREATE INDEX runtime_access_outbound_age
          ON runtime_access_outbound(json_extract(record,'$.completed'))
          WHERE json_extract(record,'$.outcome')='complete';
CREATE INDEX runtime_usage_resume_agent ON runtime_usage_resumes(agent);
CREATE INDEX runtime_usage_resume_due ON runtime_usage_resumes(json_extract(record,'$.status'),json_extract(record,'$.dueAt'));
CREATE INDEX runtime_usage_resume_account ON runtime_usage_resumes(json_extract(record,'$.accountKey'),json_extract(record,'$.status'));
CREATE INDEX runtime_event_queue ON runtime_events(status, agent, created);
CREATE INDEX runtime_event_repair_candidates_v2 ON runtime_events(agent,created)
                  WHERE status='delivered' AND kind IN ('monitor_exit','agent_message','work_review','work_decision');
CREATE INDEX runtime_event_user_turns ON runtime_events(agent,kind,status,turn_id);
CREATE INDEX runtime_context_manifest_latest
                  ON runtime_context_manifests(agent,sequence DESC);
CREATE INDEX runtime_item_agent ON runtime_items(agent, created);
CREATE INDEX runtime_events_agent_epoch_status_created
                  ON runtime_events(agent,epoch,status,created,id);
CREATE INDEX runtime_task_status ON runtime_tasks(json_extract(record,'$.status'), json_extract(record,'$.created'));
CREATE INDEX runtime_task_history ON runtime_tasks(json_extract(record,'$.created') DESC, json_extract(record,'$.agent')) WHERE json_extract(record,'$.status')!='running';
CREATE INDEX runtime_task_agent_created_id ON runtime_tasks(
                    json_extract(record,'$.agent'), json_extract(record,'$.created') DESC, id DESC);
CREATE INDEX runtime_task_agent_updated_id ON runtime_tasks(
                    json_extract(record,'$.agent'),
                    CASE WHEN COALESCE(json_extract(record,'$.finished'),0) > COALESCE(json_extract(record,'$.created'),0)
                         THEN json_extract(record,'$.finished') ELSE json_extract(record,'$.created') END,
                    id);
CREATE INDEX runtime_request_native_question ON runtime_requests(
                    json_extract(record,'$.agent'),json_extract(record,'$.params.itemId'),
                    json_extract(record,'$.status'))
                    WHERE json_valid(record) AND json_extract(record,'$.method')='item/tool/requestUserInput';
CREATE INDEX runtime_request_native_approval ON runtime_requests(
                    json_extract(record,'$.agent'),json_extract(record,'$.params.itemId'),
                    json_extract(record,'$.status'))
                    WHERE json_valid(record) AND json_extract(record,'$.method') IN
                    ('item/commandExecution/requestApproval','item/fileChange/requestApproval');
CREATE INDEX runtime_chat_room ON runtime_chat_messages(room, seq);
CREATE INDEX runtime_scheduler_agent_change_generation ON runtime_scheduler_agent_changes(generation);
CREATE INDEX runtime_execution_run_agent ON runtime_execution_runs(agent,created DESC);
CREATE INDEX runtime_execution_run_created ON runtime_execution_runs(created DESC);
CREATE INDEX runtime_execution_run_message ON runtime_execution_runs(agent,thread,turn);
CREATE INDEX runtime_execution_run_native ON runtime_execution_runs(agent,account,thread,turn);
CREATE INDEX runtime_execution_attempt_run ON runtime_execution_attempts(run);
CREATE INDEX runtime_execution_input_event ON runtime_execution_inputs(event);
CREATE INDEX runtime_execution_effect_run ON runtime_execution_effects(run);
CREATE INDEX runtime_execution_node_run ON runtime_execution_nodes(run);
CREATE INDEX runtime_federation_outbox_peer
          ON runtime_federation_outbox(peer_id);
CREATE INDEX runtime_event_created_id ON runtime_events(created DESC,id);
CREATE INDEX runtime_work_root_status
                ON runtime_work(json_extract(record,'$.rootId'),json_extract(record,'$.status'));
CREATE INDEX runtime_work_owner_status
                ON runtime_work(json_extract(record,'$.owner'),json_extract(record,'$.status'));
CREATE INDEX runtime_work_owner_root_status_id ON runtime_work(
                json_extract(record,'$.owner'),json_extract(record,'$.rootId'),
                json_extract(record,'$.status'),id);
CREATE INDEX runtime_tool_request_actor
                ON runtime_tool_requests(json_extract(record,'$.agent'), json_extract(record,'$.updated') DESC);
CREATE INDEX runtime_workspace_operation_phase ON runtime_workspace_operations(
                json_extract(record,'$.phase'), json_extract(record,'$.agent'));
CREATE INDEX runtime_rule_ready_tick ON runtime_rules(json_extract(record,'$.status')) WHERE NOT COALESCE(json_extract(record,'$.inFlight'),0);
CREATE INDEX runtime_request_agent_status ON runtime_requests(json_extract(record,'$.agent'),json_extract(record,'$.status'));
CREATE INDEX runtime_rule_agent_inflight ON runtime_rules(json_extract(record,'$.agent'),json_extract(record,'$.inFlight'));
CREATE INDEX runtime_conversion_rules_rootId ON runtime_rules(json_extract(record,'$.rootId')) WHERE json_extract(record,'$.rootId') IS NOT NULL;
CREATE INDEX runtime_workspace_operation_agent_phase ON runtime_workspace_operations(json_extract(record,'$.agent'),json_extract(record,'$.phase'));
CREATE INDEX runtime_room_root ON runtime_rooms(json_extract(record,'$.rootId'));
CREATE INDEX runtime_room_member_first ON runtime_rooms(json_extract(record,'$.members[0]'));
CREATE INDEX runtime_room_member_second ON runtime_rooms(json_extract(record,'$.members[1]'));
CREATE INDEX runtime_room_many_members ON runtime_rooms(json_array_length(record,'$.members')) WHERE json_array_length(record,'$.members')>2;
CREATE INDEX runtime_conversion_plans_rootId ON runtime_plans(json_extract(record,'$.rootId')) WHERE json_extract(record,'$.rootId') IS NOT NULL;
CREATE INDEX runtime_conversion_annotations_rootId ON runtime_annotations(json_extract(record,'$.rootId')) WHERE json_extract(record,'$.rootId') IS NOT NULL;
CREATE INDEX runtime_conversion_complaints_leadId ON runtime_complaints(json_extract(record,'$.leadId')) WHERE json_extract(record,'$.leadId') IS NOT NULL;
CREATE INDEX runtime_conversion_event_meta_rootId ON runtime_event_meta(json_extract(record,'$.rootId')) WHERE json_extract(record,'$.rootId') IS NOT NULL;
CREATE INDEX runtime_conversion_event_meta_leadId ON runtime_event_meta(json_extract(record,'$.leadId')) WHERE json_extract(record,'$.leadId') IS NOT NULL;
CREATE INDEX runtime_account_transfer_status ON runtime_account_transfers(json_extract(record,'$.status'));
CREATE INDEX durable_rows_seq ON durable_rows(seq);
CREATE INDEX durable_rows_tombstone_at ON durable_rows(at) WHERE deleted=1;
CREATE INDEX runtime_agent_account_wake_candidates_v6 ON runtime_agents(account_key,id) WHERE deleted_at IS NULL AND COALESCE(remote_worker,0)=0 AND COALESCE(remote_anchor,0)=0 AND ((status='queued' AND auto_wake=1 AND COALESCE(in_flight,0)=0 AND COALESCE(native_failure_hold,0)=0) OR (status='running' AND auto_wake=1 AND in_flight=1 AND turn_id IS NOT NULL AND COALESCE(steer_rejected_turn_id,'')<>turn_id AND COALESCE(native_failure_hold,0)=0 AND COALESCE(execution_mode,'')<>'vm') OR ((provider='codex' AND status IN ('starting','running','waiting','paused','interrupted')) OR (has_disconnect_recovery=1 AND ( COALESCE(status,'') NOT IN ('completed','waiting','paused','parked') OR disconnect_recovery_stage IS NOT NULL)) OR (has_restart_recovery=1 AND ( COALESCE(status,'') NOT IN ('completed','waiting','paused','parked') OR COALESCE(restart_recovery_stage,'') NOT IN ('finished','continued','reattached','input_restored')))));
CREATE INDEX runtime_event_child_result_pending ON runtime_events(created) WHERE kind='child_result' AND status='pending';
CREATE INDEX runtime_agent_parent_v2 ON runtime_agents(parent_id);
CREATE INDEX runtime_agent_reviewer_archive_pending_v2 ON runtime_agents(parent_id,id) WHERE role='reviewer' AND status='completed' AND NOT agent_archived AND NOT review_archive_failed AND NOT review_archive_scheduled;
CREATE INDEX runtime_agent_reviewer_archive_retry_due_v2 ON runtime_agents(COALESCE(review_archive_next_at,0),id) WHERE role='reviewer' AND status='completed' AND NOT agent_archived AND review_archive_marked;
CREATE INDEX runtime_event_reviewer_archive_delivery ON runtime_events(agent, CASE WHEN json_valid(text) THEN json_extract(text,'$.agent_id') END,id) WHERE kind='child_result' AND status='delivered';
CREATE INDEX runtime_agent_vm_stream_owner ON runtime_agents(account_key,id) WHERE execution_mode='vm' AND hold_status='clear';
CREATE INDEX runtime_monitor_agent_status ON runtime_monitors(
                    json_extract(record,'$.agent'),json_extract(record,'$.status'));
CREATE INDEX runtime_monitor_status ON runtime_monitors(json_extract(record,'$.status'),json_extract(record,'$.created'));
CREATE INDEX messages_room ON messages(room, at);
CREATE INDEX voice_records_agent ON voice_records(agent,seq);
CREATE INDEX user_terminal_output_end ON user_terminal_output (terminal,end);
CREATE INDEX runtime_agent_sweep_busy_v2 ON runtime_agents(account_key) WHERE provider='codex' AND (context_repair_phase IN ('preparing','submitted','unknown','ready') OR COALESCE(native_tool_refresh_id,'')!='');
CREATE INDEX runtime_agent_callback_context_v3 ON runtime_agents(account_key,thread_id,turn_id) WHERE status='running' AND auto_wake=1;
CREATE INDEX runtime_agent_sweep_cleanup_v2 ON runtime_agents(source_cleanup_account_key,source_cleanup_thread_id) WHERE source_cleanup_phase IN ('planned','submitted');
CREATE INDEX runtime_agent_unsettled_legacy ON runtime_agents(id) WHERE legacy_unsettled=1;
CREATE INDEX runtime_idle_compaction_deadline ON runtime_agents(idle_episode_type,idle_episode_fire_at,id,idle_episode_id) WHERE idle_episode_type='object';
CREATE INDEX runtime_server_exec_input_handle
            ON runtime_server_exec_input(handle);
CREATE INDEX runtime_receipt_transcript_item ON runtime_event_meta(json_extract(record,'$.transcriptItemId'));
CREATE INDEX runtime_monitor_queued_stream ON runtime_monitors(id) WHERE stream_pending=1;
CREATE INDEX runtime_monitor_studio_error_v2 ON runtime_monitors(monitor_created) WHERE monitor_error_candidate=1;
CREATE INDEX runtime_agent_capacity_slots_v3 ON runtime_agents(COALESCE(remote_worker,0),id) WHERE status IN ('starting','running','waiting','paused','approval') AND deleted_at IS NULL;
CREATE INDEX runtime_agent_root_v2 ON runtime_agents(root_id);
CREATE INDEX runtime_agent_account ON runtime_agents(account_key,deleted_at);
CREATE INDEX runtime_agent_thread ON runtime_agents(thread_id,account_key);
CREATE INDEX runtime_agent_active ON runtime_agents(root_id) WHERE (in_flight=1 OR status IN ('running','starting','approval'));
CREATE INDEX runtime_agent_live_root_v2 ON runtime_agents(root_id,id) WHERE deleted_at IS NULL;
CREATE INDEX runtime_agent_workspace_operation ON runtime_agents(cwd) WHERE typeof(workspace_operation)='text' AND workspace_operation<>'';
CREATE INDEX runtime_outbox_pending ON runtime_outbox(kind,status,next_at,id) WHERE status='pending';
CREATE INDEX runtime_outbox_owner ON runtime_outbox(owner_kind,owner_id,kind,id);
CREATE INDEX runtime_outbox_account ON runtime_outbox(account,kind,status,ordinal,id);
CREATE INDEX runtime_outbox_stream ON runtime_outbox(stream_id,generation,ordinal,id);
CREATE INDEX runtime_outbox_stream_ordered ON runtime_outbox(stream_id,kind,ordinal,id);
CREATE INDEX runtime_outbox_request ON runtime_outbox(request_id,id) WHERE request_id IS NOT NULL;
CREATE INDEX runtime_error_incidents_created ON runtime_error_incidents(created DESC,id);
CREATE INDEX runtime_error_incidents_owner ON runtime_error_incidents(owner_kind,owner_id,created DESC,id);
CREATE INDEX runtime_error_incidents_open ON runtime_error_incidents(last_seen DESC,id) WHERE dismissed_at IS NULL;
CREATE INDEX runtime_error_incidents_dismissed ON runtime_error_incidents(id,dismissed_at) WHERE dismissed_at IS NOT NULL;
CREATE INDEX request_results_created ON request_results(created,id);
CREATE INDEX request_results_tool_actor_created
           ON request_results(json_extract(result,'$.actor'),created DESC,id)
           WHERE json_extract(result,'$.actor') IS NOT NULL;
CREATE INDEX durable_view_log_seq ON durable_view_log(seq);
CREATE INDEX runtime_monitor_startup_active ON runtime_monitors(id) WHERE monitor_status IN ('running','starting','approval');
CREATE INDEX sessions_chat ON sessions(chat);
CREATE UNIQUE INDEX turns_one_attempt ON turns(chat) WHERE attempt IS NOT NULL;
CREATE INDEX turns_callback_context
            ON turns(attempt_connection_generation,chat) WHERE attempt IS NOT NULL;
CREATE UNIQUE INDEX turns_one_unfinished
            ON turns(chat) WHERE state IN ('starting','running');
CREATE INDEX turns_provider_turn ON turns(chat,provider_turn);
CREATE UNIQUE INDEX runtime_tool_request_origin ON runtime_tool_requests(origin_key) WHERE origin_key IS NOT NULL;
CREATE INDEX runtime_agent_holds ON runtime_agents(id) WHERE hold_status!='clear';
CREATE INDEX runtime_work_archive_due ON runtime_work(archive_next_at,id) WHERE archive_status IN ('pending','retryable','unknown');
CREATE INDEX runtime_tool_request_current_actor ON runtime_tool_requests(agent,updated DESC,id);
CREATE INDEX runtime_tool_request_current_status ON runtime_tool_requests(status,agent,id);
CREATE UNIQUE INDEX runtime_execution_terminal_source ON runtime_execution_attempts(json_extract(record,'$.terminalRepair.source.accountKey'),json_extract(record,'$.terminalRepair.source.threadId'),json_extract(record,'$.terminalRepair.proof.turnId')) WHERE json_type(record,'$.terminalRepair')='object';
CREATE INDEX runtime_request_startup_unsettled ON runtime_requests(id) WHERE CASE WHEN json_valid(record) THEN json_extract(record,'$.status') IN ('answering','pending') ELSE 0 END;
CREATE INDEX runtime_tool_request_startup_unsettled_v2 ON runtime_tool_requests(id) WHERE kind='managed_tool' AND status IN ('queued','running');
CREATE INDEX runtime_agent_cleanup_intent_v2 ON runtime_agents(id) WHERE has_workspace_cleanup_intent=1;
CREATE INDEX runtime_agent_deleted_unsettled_v2 ON runtime_agents(id) WHERE COALESCE(deleted_at,0)!=0 AND (COALESCE(in_flight,0)!=0 OR COALESCE(turn_id,0)!=0 OR status IN ('running','starting','approval') OR 0);
PRAGMA writable_schema=OFF;
COMMIT;
PRAGMA journal_mode=WAL;
PRAGMA synchronous=NORMAL;
