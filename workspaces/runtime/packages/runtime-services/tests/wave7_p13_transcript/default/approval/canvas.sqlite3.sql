/* WARNING: Script requires that SQLITE_DBCONFIG_DEFENSIVE be disabled */
PRAGMA foreign_keys=OFF;
BEGIN TRANSACTION;
CREATE TABLE runtime_account_transfers(id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_agent_moves(id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_agent_record_generation (
                    id INTEGER PRIMARY KEY CHECK(id=1), value INTEGER NOT NULL);
INSERT INTO runtime_agent_record_generation VALUES(1,17);
CREATE TABLE runtime_annotations (id TEXT PRIMARY KEY, record TEXT NOT NULL, rev INTEGER NOT NULL DEFAULT 1);
CREATE TABLE runtime_assets (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_budget (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_capacity_retries (id TEXT PRIMARY KEY, agent TEXT NOT NULL, record TEXT NOT NULL);
CREATE TABLE runtime_chat_messages (
                  seq INTEGER PRIMARY KEY AUTOINCREMENT, id TEXT UNIQUE NOT NULL,
                  room TEXT NOT NULL, sender TEXT NOT NULL, text TEXT NOT NULL,
                  created REAL NOT NULL, deliveries TEXT NOT NULL, rev INTEGER NOT NULL DEFAULT 1);
INSERT INTO runtime_chat_messages VALUES(1,'chat','private','child','needle room',210,'{}',1);
CREATE TABLE runtime_checkpoints (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_compactions (id TEXT PRIMARY KEY, agent TEXT NOT NULL);
INSERT INTO runtime_compactions VALUES('agent:compact','agent');
CREATE TABLE runtime_complaints (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_completed_turns (id TEXT PRIMARY KEY);
INSERT INTO runtime_completed_turns VALUES('agent:turn');
CREATE TABLE runtime_context_manifests (
                  agent TEXT NOT NULL, thread_id TEXT NOT NULL, compactions INTEGER NOT NULL,
                  sequence INTEGER NOT NULL, event_id TEXT NOT NULL, record TEXT NOT NULL,
                  PRIMARY KEY(agent,thread_id,compactions));
CREATE TABLE runtime_context_reminders (
                  agent TEXT NOT NULL, version TEXT NOT NULL, event_id TEXT NOT NULL,
                  PRIMARY KEY(agent,version));
CREATE TABLE runtime_event_meta (id TEXT PRIMARY KEY, record TEXT NOT NULL);
INSERT INTO runtime_event_meta VALUES('input','{"delivery":"current_turn","remoteRequestId":null,"timing":{"enqueuedAt":228868352214868,"fastSkipReason":"scheduleOverride"},"transcriptItemId":"agent:input"}');
CREATE TABLE runtime_events (
                  id TEXT PRIMARY KEY, agent TEXT NOT NULL, kind TEXT NOT NULL,
                  text TEXT NOT NULL, status TEXT NOT NULL, created REAL NOT NULL,
                  epoch INTEGER NOT NULL, turn_id TEXT, error TEXT, receipt_enum_guard INTEGER GENERATED ALWAYS AS (NULL) VIRTUAL CHECK("kind" IN ('user','followup','agent_message','child_result','child_stopped','complaint','complaint_response','chat_review','work_review','work_decision','work_ready','monitor_exit','monitor_stall','rule','rule_missed','rule_stall','radio_turn','browser_declined','browser_joined','browser_outcome','browser_unanswered','event_wake','monitor_cancelled','monitor_output','plan_update','shell_policy','work_released','work_result_invalid','context_repair_wait','monitor_correction','rule_hold')) CHECK("status" IN ('stored_only','pending','reserved','dispatching','delivered','uncertain','cancelled','failed')));
INSERT INTO runtime_events VALUES('input','agent','user','Needle submitted input','delivered',200,1,NULL,NULL);
CREATE TABLE runtime_execution_attempts (id TEXT PRIMARY KEY, run TEXT NOT NULL, record TEXT NOT NULL, submission TEXT GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN record->>'$.submission' END) VIRTUAL CHECK("submission" IN ('unsent','pending','accepted','observed','rejected','unknown')), turn_status TEXT GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN record->>'$.turnStatus' END) VIRTUAL CHECK("turn_status" IN ('unsent','pending','accepted','observed','running','unknown','rejected','completed','failed','interrupted','cancelled')));
CREATE TABLE runtime_execution_effects (id TEXT PRIMARY KEY, run TEXT NOT NULL, kind TEXT NOT NULL, reference TEXT NOT NULL, record TEXT NOT NULL, status TEXT GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN record->>'$.status' END) VIRTUAL CHECK("status" IN ('unknown','idle','queued','starting','running','waiting','waiting_on_monitor','approval','completed','interrupted','failed','blocked','paused','parked','abandoned','capacity-retry','pending','cancelled','answering','answered','uncertain','resolved','lost','expired','deleted')), receipt_enum_guard INTEGER GENERATED ALWAYS AS (NULL) VIRTUAL CHECK("kind" IN ('spawn','tool_requests','requests','monitors','task_submit','unrecognized')));
INSERT INTO runtime_execution_effects VALUES('spawn:child','run:543b6d63-ba17-5996-aa5b-7bc7c6764369','spawn','child','{"id": "spawn:child", "runId": "run:543b6d63-ba17-5996-aa5b-7bc7c6764369", "kind": "spawn", "referenceId": "child", "attemptId": null, "status": "running"}');
CREATE TABLE runtime_execution_inputs (run TEXT NOT NULL, event TEXT NOT NULL, PRIMARY KEY(run,event));
CREATE TABLE runtime_execution_nodes (id TEXT PRIMARY KEY, run TEXT NOT NULL, record TEXT NOT NULL, status TEXT GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN record->>'$.status' END) VIRTUAL CHECK("status" IN ('unknown','idle','queued','starting','running','waiting','waiting_on_monitor','approval','completed','interrupted','failed','blocked','paused','parked','abandoned','capacity-retry')));
INSERT INTO runtime_execution_nodes VALUES('worker:child','run:543b6d63-ba17-5996-aa5b-7bc7c6764369','{"id": "worker:child", "runId": "run:543b6d63-ba17-5996-aa5b-7bc7c6764369", "kind": "managed_worker", "agentId": "child", "status": "running"}');
CREATE TABLE runtime_execution_runs (id TEXT PRIMARY KEY, agent TEXT NOT NULL, account TEXT NOT NULL, epoch INTEGER NOT NULL, thread TEXT, turn TEXT, created REAL NOT NULL, record TEXT NOT NULL, status TEXT GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN record->>'$.status' END) VIRTUAL CHECK("status" IN ('unsent','pending','accepted','observed','running','unknown','rejected','completed','failed','interrupted','cancelled')));
INSERT INTO runtime_execution_runs VALUES('run:543b6d63-ba17-5996-aa5b-7bc7c6764369','agent','default',1,'thread','turn',1792689634.367718,'{"id": "run:543b6d63-ba17-5996-aa5b-7bc7c6764369", "agent": "agent", "accountKey": "default", "epoch": 1, "threadId": "thread", "turnId": "turn", "created": 1792689634.367718, "status": "running", "firstAttemptId": null, "rootAttemptId": null, "latestAttemptId": null, "reconciled": true}');
INSERT INTO runtime_execution_runs VALUES('run:48f065a1-8be0-543a-9b6f-69bc8581573d','other','default',1,'other-thread','turn',1792689634.369062,'{"id": "run:48f065a1-8be0-543a-9b6f-69bc8581573d", "agent": "other", "accountKey": "default", "epoch": 1, "threadId": "other-thread", "turnId": "turn", "created": 1792689634.3690617, "status": "running", "firstAttemptId": null, "rootAttemptId": null, "latestAttemptId": null, "reconciled": true}');
INSERT INTO runtime_execution_runs VALUES('run:24b2429b-e38b-5348-a024-3759e25043e1','child','default',1,'child-thread','turn',1792689634.369747,'{"id": "run:24b2429b-e38b-5348-a024-3759e25043e1", "agent": "child", "accountKey": "default", "epoch": 1, "threadId": "child-thread", "turnId": "turn", "created": 1792689634.3697467, "status": "running", "firstAttemptId": null, "rootAttemptId": null, "latestAttemptId": null, "reconciled": true}');
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
INSERT INTO runtime_item_fulltext VALUES('agent:old-0','needle history 0');
INSERT INTO runtime_item_fulltext VALUES('agent:old-1','needle history 1');
INSERT INTO runtime_item_fulltext VALUES('agent:old-2','needle history 2');
INSERT INTO runtime_item_fulltext VALUES('agent:old-3','needle history 3');
INSERT INTO runtime_item_fulltext VALUES('agent:old-4','needle history 4');
INSERT INTO runtime_item_fulltext VALUES('agent:old-5','needle history 5');
INSERT INTO runtime_item_fulltext VALUES('agent:old-6','needle history 6');
INSERT INTO runtime_item_fulltext VALUES('agent:old-7','needle history 7');
INSERT INTO runtime_item_fulltext VALUES('other:other','needle foreign');
INSERT INTO runtime_item_fulltext VALUES('agent:input','Needle submitted input');
INSERT INTO runtime_item_fulltext VALUES('agent:command','{"id": "command", "type": "commandExecution", "exitCode": 0, "aggregatedOutput": "needle tool output", "outputTruncated": false, "durationMs": 0}');
INSERT INTO runtime_item_fulltext VALUES('agent:compact','{"id": "compact", "type": "contextCompaction", "durationMs": 0}');
CREATE TABLE runtime_items (
                  id TEXT PRIMARY KEY, agent TEXT NOT NULL, record TEXT NOT NULL, created REAL NOT NULL);
INSERT INTO runtime_items VALUES('agent:old-0','agent','{"id": "agent:old-0", "role": "assistant", "title": "Agent", "text": "needle history 0", "truncated": false, "at": 100.0, "turnId": "old-turn"}',100);
INSERT INTO runtime_items VALUES('agent:old-1','agent','{"id": "agent:old-1", "role": "assistant", "title": "Agent", "text": "needle history 1", "truncated": false, "at": 101.0, "turnId": "old-turn"}',101);
INSERT INTO runtime_items VALUES('agent:old-2','agent','{"id": "agent:old-2", "role": "assistant", "title": "Agent", "text": "needle history 2", "truncated": false, "at": 102.0, "turnId": "old-turn"}',102);
INSERT INTO runtime_items VALUES('agent:old-3','agent','{"id": "agent:old-3", "role": "assistant", "title": "Agent", "text": "needle history 3", "truncated": false, "at": 103.0, "turnId": "old-turn"}',103);
INSERT INTO runtime_items VALUES('agent:old-4','agent','{"id": "agent:old-4", "role": "assistant", "title": "Agent", "text": "needle history 4", "truncated": false, "at": 104.0, "turnId": "old-turn"}',104);
INSERT INTO runtime_items VALUES('agent:old-5','agent','{"id": "agent:old-5", "role": "assistant", "title": "Agent", "text": "needle history 5", "truncated": false, "at": 105.0, "turnId": "old-turn"}',105);
INSERT INTO runtime_items VALUES('agent:old-6','agent','{"id": "agent:old-6", "role": "assistant", "title": "Agent", "text": "needle history 6", "truncated": false, "at": 106.0, "turnId": "old-turn"}',106);
INSERT INTO runtime_items VALUES('agent:old-7','agent','{"id": "agent:old-7", "role": "assistant", "title": "Agent", "text": "needle history 7", "truncated": false, "at": 107.0, "turnId": "old-turn"}',107);
INSERT INTO runtime_items VALUES('other:other','other','{"id": "other:other", "role": "assistant", "title": "Agent", "text": "needle foreign", "truncated": false, "at": 1792689634.381195}',1792689634.381205);
INSERT INTO runtime_items VALUES('agent:input','agent','{"id": "agent:input", "role": "user", "title": "You", "text": "Needle submitted input", "truncated": false, "at": 200.0, "turnId": "turn", "inputs": [{"id": "input", "at": 200.0, "kind": "user", "text": "Needle submitted input", "truncated": false, "assets": []}]}',200);
INSERT INTO runtime_items VALUES('agent:answer','agent','{"id": "agent:answer", "role": "assistant", "title": "Assistant", "text": "Needle streamed answer", "truncated": false, "at": 300.0, "streaming": true, "turnId": "turn", "phase": null}',300);
INSERT INTO runtime_items VALUES('agent:command','agent','{"id": "agent:command", "role": "output", "title": "commandExecution", "text": "{\"id\": \"command\", \"type\": \"commandExecution\", \"exitCode\": 0, \"aggregatedOutput\": \"needle tool output\", \"outputTruncated\": false, \"durationMs\": 0}", "truncated": false, "at": 300.0, "toolStatus": "completed", "turnId": "turn"}',300);
INSERT INTO runtime_items VALUES('agent:compact','agent','{"id": "agent:compact", "role": "output", "title": "contextCompaction", "text": "{\"id\": \"compact\", \"type\": \"contextCompaction\", \"durationMs\": 0}", "truncated": false, "at": 300.0, "toolStatus": "completed", "turnId": "turn"}',300);
CREATE TABLE runtime_lead_requests (id TEXT PRIMARY KEY, agent TEXT NOT NULL, signature TEXT NOT NULL);
CREATE TABLE runtime_plans (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_profiles (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_projects (id TEXT PRIMARY KEY, record TEXT NOT NULL, removal_reason TEXT GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN record->>'$.removal.reason' END) VIRTUAL CHECK("removal_reason" IN ('delete')));
CREATE TABLE runtime_requests (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_rooms (id TEXT PRIMARY KEY, record TEXT NOT NULL);
INSERT INTO runtime_rooms VALUES('private','{"id": "private", "kind": "private", "rootId": "agent", "members": ["agent", "child"], "updated": 1.0}');
CREATE TABLE runtime_rules (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_scheduler_agent_change_state (id INTEGER PRIMARY KEY CHECK(id=1),floor INTEGER NOT NULL,generation INTEGER NOT NULL,entries INTEGER NOT NULL,total INTEGER NOT NULL);
INSERT INTO runtime_scheduler_agent_change_state VALUES(1,0,17,3,48);
CREATE TABLE runtime_scheduler_agent_changes (id TEXT PRIMARY KEY,generation INTEGER NOT NULL CHECK(typeof(generation)='integer' AND generation>=0));
INSERT INTO runtime_scheduler_agent_changes VALUES('agent',15);
INSERT INTO runtime_scheduler_agent_changes VALUES('other',16);
INSERT INTO runtime_scheduler_agent_changes VALUES('child',17);
PRAGMA writable_schema=ON;
INSERT INTO sqlite_schema(type,name,tbl_name,rootpage,sql)VALUES('table','runtime_search_next','runtime_search_next',0,'CREATE VIRTUAL TABLE runtime_search_next USING fts5(body,content='''',contentless_delete=1,tokenize=''unicode61'')');
CREATE TABLE IF NOT EXISTS 'runtime_search_next_config'(k PRIMARY KEY, v) WITHOUT ROWID;
INSERT INTO runtime_search_next_config VALUES('version',4);
CREATE TABLE IF NOT EXISTS 'runtime_search_next_data'(id INTEGER PRIMARY KEY, block BLOB);
INSERT INTO runtime_search_next_data VALUES(1,x'0c31');
INSERT INTO runtime_search_next_data VALUES(10,x'00000000ff000001010d0f000d0101010101000001020101020200000103010103030000010401010404000001050101050500000106010106060000010701010707000001080101080800000109010109090000010a01010a0a0000010b01010b0b0102010c01010c0c0000010d01010d0d000001');
INSERT INTO runtime_search_next_data VALUES(137438953473,x'000000210230300102040107686973746f727901020301066e6565646c6501020204060c');
INSERT INTO runtime_search_next_data VALUES(274877906945,x'000000210230310202040107686973746f727902020301066e6565646c6502020204060c');
INSERT INTO runtime_search_next_data VALUES(412316860417,x'000000210230320302040107686973746f727903020301066e6565646c6503020204060c');
INSERT INTO runtime_search_next_data VALUES(549755813889,x'000000210230330402040107686973746f727904020301066e6565646c6504020204060c');
INSERT INTO runtime_search_next_data VALUES(687194767361,x'000000210230340502040107686973746f727905020301066e6565646c6505020204060c');
INSERT INTO runtime_search_next_data VALUES(824633720833,x'000000210230350602040107686973746f727906020301066e6565646c6506020204060c');
INSERT INTO runtime_search_next_data VALUES(962072674305,x'000000210230360702040107686973746f727907020301066e6565646c6507020204060c');
INSERT INTO runtime_search_next_data VALUES(1099511627777,x'000000210230370802040107686973746f727908020301066e6565646c6508020204060c');
INSERT INTO runtime_search_next_data VALUES(1236950581249,x'0000001b0830666f726569676e09020301066e6565646c65090202040c');
INSERT INTO runtime_search_next_data VALUES(1374389534721,x'000000270630696e7075740a020401066e6565646c650a020201097375626d69747465640a0203040a0b');
INSERT INTO runtime_search_next_data VALUES(1511828488193,x'00000058033034320c020a0107636f6d6d616e640c0403050809657865637574696f6e0c0205010269640c020201066e6565646c650c020801067072696e74660c020703076f6365737369640c02090104747970650c020404070d0e070b0b0c');
INSERT INTO runtime_search_next_data VALUES(1649267441665,x'0000009c0230300c04070a0110616767726567617465646f75747075740c02080107636f6d6d616e640c02030809657865637574696f6e0c0205010a6475726174696f6e6d730c020e010865786974636f64650c0206010566616c73650c020d010269640c020201066e6565646c650c020901066f75747075740c020b07097472756e63617465640c020c0104746f6f6c0c020a02037970650c02040407150c0e0f0d0a070b0b0e09');
INSERT INTO runtime_search_next_data VALUES(1786706395137,x'000000490230300d02070107636f6d706163740d0203030f6e74657874636f6d70616374696f6e0d0205010a6475726174696f6e6d730d0206010269640d02020104747970650d020404060c140f07');
INSERT INTO runtime_search_next_data VALUES(9008711083229184,x'040000000000000200000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000b0000000c00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000');
CREATE TABLE IF NOT EXISTS 'runtime_search_next_docsize'(id INTEGER PRIMARY KEY, sz BLOB, origin INTEGER);
INSERT INTO runtime_search_next_docsize VALUES(1,x'03',1);
INSERT INTO runtime_search_next_docsize VALUES(2,x'03',2);
INSERT INTO runtime_search_next_docsize VALUES(3,x'03',3);
INSERT INTO runtime_search_next_docsize VALUES(4,x'03',4);
INSERT INTO runtime_search_next_docsize VALUES(5,x'03',5);
INSERT INTO runtime_search_next_docsize VALUES(6,x'03',6);
INSERT INTO runtime_search_next_docsize VALUES(7,x'03',7);
INSERT INTO runtime_search_next_docsize VALUES(8,x'03',8);
INSERT INTO runtime_search_next_docsize VALUES(9,x'02',9);
INSERT INTO runtime_search_next_docsize VALUES(10,x'03',10);
INSERT INTO runtime_search_next_docsize VALUES(12,x'0e',12);
INSERT INTO runtime_search_next_docsize VALUES(13,x'06',13);
CREATE TABLE IF NOT EXISTS 'runtime_search_next_idx'(segid, term, pgno, PRIMARY KEY(segid, term)) WITHOUT ROWID;
INSERT INTO runtime_search_next_idx VALUES(1,x'',2);
INSERT INTO runtime_search_next_idx VALUES(2,x'',2);
INSERT INTO runtime_search_next_idx VALUES(3,x'',2);
INSERT INTO runtime_search_next_idx VALUES(4,x'',2);
INSERT INTO runtime_search_next_idx VALUES(5,x'',2);
INSERT INTO runtime_search_next_idx VALUES(6,x'',2);
INSERT INTO runtime_search_next_idx VALUES(7,x'',2);
INSERT INTO runtime_search_next_idx VALUES(8,x'',2);
INSERT INTO runtime_search_next_idx VALUES(9,x'',2);
INSERT INTO runtime_search_next_idx VALUES(10,x'',2);
INSERT INTO runtime_search_next_idx VALUES(11,x'',2);
INSERT INTO runtime_search_next_idx VALUES(12,x'',2);
INSERT INTO runtime_search_next_idx VALUES(13,x'',2);
CREATE TABLE runtime_search_next_meta (
                id TEXT PRIMARY KEY, agent TEXT NOT NULL, kind TEXT NOT NULL,
                search_rowid INTEGER NOT NULL UNIQUE);
INSERT INTO runtime_search_next_meta VALUES('agent:old-0','agent','Agent',1);
INSERT INTO runtime_search_next_meta VALUES('agent:old-1','agent','Agent',2);
INSERT INTO runtime_search_next_meta VALUES('agent:old-2','agent','Agent',3);
INSERT INTO runtime_search_next_meta VALUES('agent:old-3','agent','Agent',4);
INSERT INTO runtime_search_next_meta VALUES('agent:old-4','agent','Agent',5);
INSERT INTO runtime_search_next_meta VALUES('agent:old-5','agent','Agent',6);
INSERT INTO runtime_search_next_meta VALUES('agent:old-6','agent','Agent',7);
INSERT INTO runtime_search_next_meta VALUES('agent:old-7','agent','Agent',8);
INSERT INTO runtime_search_next_meta VALUES('other:other','other','Agent',9);
INSERT INTO runtime_search_next_meta VALUES('agent:input','agent','You',10);
INSERT INTO runtime_search_next_meta VALUES('agent:command','agent','commandExecution',12);
INSERT INTO runtime_search_next_meta VALUES('agent:compact','agent','contextCompaction',13);
CREATE TABLE runtime_search_rollout (
                id INTEGER PRIMARY KEY CHECK(id=1), phase TEXT NOT NULL,
                cursor INTEGER NOT NULL DEFAULT 0, updated REAL NOT NULL);
INSERT INTO runtime_search_rollout VALUES(1,'complete',0,1792689634.231257);
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
INSERT INTO runtime_tasks VALUES('agent:command','{"id": "agent:command", "agent": "agent", "itemId": "command", "turnId": "turn", "created": 300.0, "kind": "command", "ownerEpoch": 1, "sourceAccountKey": "default", "sourceConnectionId": "connection-7", "sourceThreadId": "thread", "sourceProvider": "codex", "type": "commandExecution", "name": "commandExecution", "status": "completed", "command": "printf needle", "processId": "42", "tail": "needle tool output", "outputTruncated": false, "exitCode": 0, "durationMs": 0, "finished": 300.0}');
INSERT INTO runtime_tasks VALUES('agent:compact','{"id": "agent:compact", "agent": "agent", "itemId": "compact", "turnId": "turn", "created": 300.0, "kind": "tool", "ownerEpoch": 1, "sourceAccountKey": "default", "sourceConnectionId": "connection-7", "sourceThreadId": "thread", "sourceProvider": "codex", "type": "contextCompaction", "name": "contextCompaction", "status": "completed", "durationMs": 0, "finished": 300.0}');
CREATE TABLE runtime_tool_requests (id TEXT PRIMARY KEY, record TEXT NOT NULL, origin_key TEXT GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN record->>'$.originKey' END) VIRTUAL, agent TEXT GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN CASE record->>'$.kind' WHEN 'managed_tool' THEN record->>'$.agent' WHEN 'provider_callback' THEN record->>'$.payload.identity.context.agent' END END) VIRTUAL, kind TEXT GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN record->>'$.kind' END) VIRTUAL CHECK("kind" IN ('managed_tool','provider_callback','unrecognized')), status TEXT GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN CASE record->>'$.kind' WHEN 'managed_tool' THEN record->>'$.stage' WHEN 'provider_callback' THEN record->>'$.status' END END) VIRTUAL CHECK("status" IN ('queued','running','cancelled','interrupted','failed','unknown','completed')), request_id TEXT GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN CASE record->>'$.kind' WHEN 'managed_tool' THEN record->>'$.admitted.id' WHEN 'provider_callback' THEN record->>'$.id' END END) VIRTUAL, updated INTEGER GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN record->>'$.updated' END) VIRTUAL);
CREATE TABLE runtime_transcript_revisions(
                        agent TEXT PRIMARY KEY, revision INTEGER NOT NULL DEFAULT 0);
INSERT INTO runtime_transcript_revisions VALUES('agent',36);
INSERT INTO runtime_transcript_revisions VALUES('other',4);
INSERT INTO runtime_transcript_revisions VALUES('child',3);
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
INSERT INTO sync_entities VALUES('workspace','current',20,'2a4db84923fdffae730da9dac1740d104f1aea8eea0c9b8afbee67b2489b66a8','{"collection":"workspace","id":"current","value":{"connected":false,"nativeNotices":[],"peerTeamsVersion":1,"projectOrganizationVersion":1,"sidebarOrder":{"groups":null,"revision":0},"stateDir":"@FIXTURE_ROOT@/python","tasksHistoryLimit":100}}',0);
INSERT INTO sync_entities VALUES('agent','agent',19,'9f5239afbcb0196f287d754989d7e0b299e71dd14f9d88725750648423ef2ed5','{"collection":"agent","id":"agent","value":{"accountKey":"default","activity":{"at":300.0,"phase":"writing"},"agentMode":"multi","agentModeRevision":0,"agentModeSupported":true,"autoWake":true,"canSend":true,"compactions":1,"concurrency":32,"contextUsage":null,"cwd":"@FIXTURE_ROOT@","empty":false,"epoch":1,"id":"agent","inFlight":true,"isLead":true,"kind":"agent","lastEvent":"2026-10-22T17:20:34Z","launcherAlive":true,"name":"Transcript fixture","nextTurnSettingsSupported":true,"readStateSupported":true,"rootId":"agent","serverId":"fixture-server","source":"managed","status":"approval","subagentConcurrencyVersion":2,"tail":"Needle streamed answer","threadId":"thread","turnId":"turn"}}',0);
INSERT INTO sync_entities VALUES('agent','other',3,'236af8647d4d27d53f9ae53e681cac59cf447d38b524fc37bae7480f62c6bbbf','{"collection":"agent","id":"other","value":{"accountKey":"default","agentMode":"multi","agentModeRevision":0,"agentModeSupported":true,"autoWake":true,"canSend":true,"concurrency":32,"cwd":"@FIXTURE_ROOT@","empty":false,"epoch":1,"id":"other","inFlight":true,"isLead":true,"kind":"agent","launcherAlive":true,"name":"Transcript fixture","nextTurnSettingsSupported":true,"readStateSupported":true,"rootId":"other","serverId":"fixture-server","source":"managed","status":"running","subagentConcurrencyVersion":2,"threadId":"other-thread","turnId":"turn"}}',0);
INSERT INTO sync_entities VALUES('agent','child',4,'d3cdd49d39cf9ab73c764ae1461b10c3f17639921e00c1975babd46171131ad9','{"collection":"agent","id":"child","value":{"accountKey":"default","agentMode":"multi","agentModeRevision":0,"agentModeSupported":true,"autoWake":true,"canSend":true,"concurrency":32,"cwd":"@FIXTURE_ROOT@","epoch":1,"id":"child","inFlight":true,"isLead":false,"kind":"agent","launcherAlive":true,"name":"Transcript fixture","nextTurnSettingsSupported":true,"overview":{"result":"","resultFile":null,"resultTruncated":false,"resultTurnId":null,"task":"","taskTruncated":false},"parentId":"agent","readStateSupported":true,"rootId":"agent","serverId":"fixture-server","source":"managed","status":"running","subagentConcurrencyVersion":2,"threadId":"child-thread","turnId":"turn"}}',0);
INSERT INTO sync_entities VALUES('event','input',6,'bff206d03823a5b769e63060cb0763453b2b719399478130fd16096b8e289f95','{"collection":"event","id":"input","value":{"agent":"agent","created":200.0,"error":null,"id":"input","kind":"user","status":"delivered"}}',0);
INSERT INTO sync_entities VALUES('room','private',7,'53df58b94e00bc7d1706ac8d79cae6fecdbce6e7f494160687490829200aa899','{"collection":"room","id":"private","value":{"id":"private","kind":"private","lastMessage":null,"members":["agent","child"],"name":"Transcript fixture ↔ Transcript fixture","rootId":"agent","updated":1.0}}',0);
INSERT INTO sync_entities VALUES('task','agent:command',12,'2af7a8c98cc5344143fa75ebea6446595058e8a29e9cd224fd9f23aada6ff6a3','{"collection":"task","id":"agent:command","value":{"agent":"agent","command":"printf needle","created":300.0,"durationMs":0,"exitCode":0,"finished":300.0,"id":"agent:command","kind":"command","name":"commandExecution","outputTruncated":false,"processId":"42","status":"completed","turnId":"turn"}}',0);
INSERT INTO sync_entities VALUES('task','agent:compact',14,'aafd76c2984f447747e28ce0d00ca6c04a0704bb86cfcddcc757ec57ec637f30','{"collection":"task","id":"agent:compact","value":{"agent":"agent","created":300.0,"durationMs":0,"finished":300.0,"id":"agent:compact","kind":"tool","name":"contextCompaction","status":"completed","turnId":"turn"}}',0);
CREATE TABLE sync_entity_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
INSERT INTO sync_entity_meta VALUES('entity_tombstone_count','0');
INSERT INTO sync_entity_meta VALUES('task_window_migrated','1');
INSERT INTO sync_entity_meta VALUES('event_window_seq','6');
INSERT INTO sync_entity_meta VALUES('seeded','1');
INSERT INTO sync_entity_meta VALUES('agent_organization_fields','2');
CREATE TABLE sync_generation (id INTEGER PRIMARY KEY, value INTEGER NOT NULL);
INSERT INTO sync_generation VALUES(1,0);
CREATE TABLE sync_identity (id TEXT PRIMARY KEY);
INSERT INTO sync_identity VALUES('5ca324ab81dc4a9abb838dcbdbb27cc2');
CREATE TABLE IF NOT EXISTS "runtime_agents"(id TEXT PRIMARY KEY,"root_id" TEXT GENERATED ALWAYS AS (record->>'$.rootId') STORED ,"parent_id" TEXT GENERATED ALWAYS AS (record->>'$.parentId') STORED ,"status" TEXT GENERATED ALWAYS AS (record->>'$.status') STORED ,"account_key" TEXT GENERATED ALWAYS AS (COALESCE(record->>'$.accountKey','default')) STORED NOT NULL,"provider" TEXT GENERATED ALWAYS AS (record->>'$.provider') STORED NOT NULL,"role" TEXT GENERATED ALWAYS AS (record->>'$.role') STORED ,"deleted_at"  GENERATED ALWAYS AS (record->>'$.deletedAt') STORED ,"thread_id" TEXT GENERATED ALWAYS AS (record->>'$.threadId') STORED ,"turn_id" TEXT GENERATED ALWAYS AS (record->>'$.turnId') STORED ,"cwd" TEXT GENERATED ALWAYS AS (record->>'$.cwd') STORED ,"workspace_operation"  GENERATED ALWAYS AS (record->>'$.workspaceOperation') STORED ,"in_flight" INTEGER GENERATED ALWAYS AS (record->>'$.inFlight') STORED ,"auto_wake" INTEGER GENERATED ALWAYS AS (record->>'$.autoWake') STORED ,"remote_worker"  GENERATED ALWAYS AS (record->>'$.remoteWorker') STORED ,"remote_anchor"  GENERATED ALWAYS AS (record->>'$.remoteAnchor') STORED ,"native_failure_hold"  GENERATED ALWAYS AS (record->>'$.nativeFailureHold') STORED ,"steer_rejected_turn_id" TEXT GENERATED ALWAYS AS (record->>'$.steerRejectedTurnId') STORED ,"execution_mode" TEXT GENERATED ALWAYS AS (record->>'$.executionMode') STORED ,"native_tool_refresh_id" TEXT GENERATED ALWAYS AS (record->>'$.nativeToolRefreshId') STORED ,"context_repair_phase" TEXT GENERATED ALWAYS AS (record->>'$.contextRepair.phase') STORED ,"source_cleanup_phase" TEXT GENERATED ALWAYS AS (record->>'$.contextRepair.sourceCleanup.phase') STORED ,"source_cleanup_account_key" TEXT GENERATED ALWAYS AS (record->>'$.contextRepair.sourceCleanup.accountKey') STORED ,"source_cleanup_thread_id" TEXT GENERATED ALWAYS AS (record->>'$.contextRepair.sourceCleanup.threadId') STORED ,"has_disconnect_recovery" INTEGER GENERATED ALWAYS AS (COALESCE(json_type(record,'$.disconnectRecovery')='object',0)) STORED NOT NULL,"disconnect_recovery_stage" TEXT GENERATED ALWAYS AS (record->>'$.disconnectRecovery.stage') STORED ,"has_restart_recovery" INTEGER GENERATED ALWAYS AS (COALESCE(json_type(record,'$.restartRecovery')='object',0)) STORED NOT NULL,"restart_recovery_stage" TEXT GENERATED ALWAYS AS (record->>'$.restartRecovery.stage') STORED ,"has_workspace_cleanup_intent" INTEGER GENERATED ALWAYS AS (COALESCE(json_type(record,'$.workspaceCleanupIntent')='object',0)) STORED NOT NULL,"agent_archived" INTEGER GENERATED ALWAYS AS (NOT (json_type(record,'$.agentArchive') IS NULL OR json_type(record,'$.agentArchive') IN ('null','false') OR (json_type(record,'$.agentArchive') IN ('integer','real') AND record->>'$.agentArchive'=0) OR (json_type(record,'$.agentArchive')='text' AND record->>'$.agentArchive'='') OR (json_type(record,'$.agentArchive')='array' AND json_array_length(record,'$.agentArchive')=0) OR (json_type(record,'$.agentArchive')='object' AND record->>'$.agentArchive'='{}'))) STORED NOT NULL,"review_archive_failed" INTEGER GENERATED ALWAYS AS (NOT (json_type(record,'$.reviewArchiveError') IS NULL OR json_type(record,'$.reviewArchiveError') IN ('null','false') OR (json_type(record,'$.reviewArchiveError') IN ('integer','real') AND record->>'$.reviewArchiveError'=0) OR (json_type(record,'$.reviewArchiveError')='text' AND record->>'$.reviewArchiveError'='') OR (json_type(record,'$.reviewArchiveError')='array' AND json_array_length(record,'$.reviewArchiveError')=0) OR (json_type(record,'$.reviewArchiveError')='object' AND record->>'$.reviewArchiveError'='{}'))) STORED NOT NULL,"review_archive_marked" INTEGER GENERATED ALWAYS AS (record->>'$.reviewArchiveScheduled' IS NOT NULL) STORED NOT NULL,"review_archive_scheduled" INTEGER GENERATED ALWAYS AS (COALESCE(json_type(record,'$.reviewArchiveScheduled')='text' AND record->>'$.reviewArchiveScheduled'<>'',0)) STORED NOT NULL,"review_archive_next_at"  GENERATED ALWAYS AS (record->>'$.reviewArchiveNextAt') STORED ,"idle_episode_type" TEXT GENERATED ALWAYS AS (json_type(record,'$.idleCompactionEpisode')) STORED ,"idle_episode_fire_at"  GENERATED ALWAYS AS (record->>'$.idleCompactionEpisode.fireAt') STORED ,"idle_episode_id"  GENERATED ALWAYS AS (record->>'$.idleCompactionEpisode.id') STORED ,"legacy_unsettled" INTEGER GENERATED ALWAYS AS (COALESCE(record->>'$.workspaceMode'='image' OR record->>'$.imageWorkspace'=1,0) AND COALESCE(record->>'$.legacyWorkspaceSettled',0)=0) STORED NOT NULL,record TEXT NOT NULL,hold_status TEXT NOT NULL DEFAULT 'clear' CHECK("hold_status" IN ('clear','invalid','unrecognized')),hold_incident TEXT,title_status TEXT GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN record->>'$.titleJob.status' END) VIRTUAL CHECK("title_status" IN ('scheduled','written','invalid','discarded','pending','submitted','unknown','not_sent','applied','failed')),vm_launch_status TEXT GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN record->>'$.vmLaunch.status' END) VIRTUAL CHECK("vm_launch_status" IN ('completed','terminal','pending','retryable','submitted','unknown','not_sent','complete','applied','failed','cancelled')),idle_episode_outcome TEXT GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN record->>'$.idleCompactionEpisode.outcome' END) VIRTUAL CHECK("idle_episode_outcome" IN ('compacted','disabled','compaction_off','idle_compaction_off','prefix_changed','started','dispatched','failed','ttl_changed','lapsed','small','newer_request','near_limit','active','queued_input','stopped','no_host','would_compact')),provider_stream_generation INTEGER GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN record->>'$.providerStream.generation' END) VIRTUAL,provider_stream_ack INTEGER GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN record->>'$.providerStream.ack' END) VIRTUAL);
INSERT INTO runtime_agents VALUES('agent','{"accountKey":"default","activeExecution":{"attemptId":null,"runId":"run:543b6d63-ba17-5996-aa5b-7bc7c6764369"},"agentMode":"multi","agentModeRevision":0,"agentModeSupported":true,"autoWake":true,"compactions":1,"concurrency":32,"cwd":"@FIXTURE_ROOT@","epoch":1,"id":"agent","inFlight":true,"isLead":true,"maxAgentsExplicit":false,"name":"Transcript fixture","nativeResponseTurn":"turn","provider":"codex","rootId":"agent","status":"approval","subagentConcurrencyVersion":2,"threadId":"thread","turnId":"turn"}','clear',NULL);
INSERT INTO runtime_agents VALUES('other','{"accountKey":"default","activeExecution":{"attemptId":null,"runId":"run:48f065a1-8be0-543a-9b6f-69bc8581573d"},"agentMode":"multi","agentModeRevision":0,"agentModeSupported":true,"autoWake":true,"compactions":0,"concurrency":32,"cwd":"@FIXTURE_ROOT@","epoch":1,"id":"other","inFlight":true,"isLead":true,"maxAgentsExplicit":false,"name":"Transcript fixture","provider":"codex","rootId":"other","status":"running","subagentConcurrencyVersion":2,"threadId":"other-thread","turnId":"turn"}','clear',NULL);
INSERT INTO runtime_agents VALUES('child','{"accountKey":"default","activeExecution":{"attemptId":null,"runId":"run:24b2429b-e38b-5348-a024-3759e25043e1"},"agentMode":"multi","agentModeRevision":0,"agentModeSupported":true,"autoWake":true,"compactions":0,"concurrency":32,"cwd":"@FIXTURE_ROOT@","epoch":1,"id":"child","inFlight":true,"isLead":false,"maxAgentsExplicit":false,"name":"Transcript fixture","parentId":"agent","provider":"codex","rootId":"agent","status":"running","subagentConcurrencyVersion":2,"threadId":"child-thread","turnId":"turn"}','clear',NULL);
CREATE TABLE durable_rows (
        tbl TEXT NOT NULL, id TEXT NOT NULL, seq INTEGER NOT NULL,
        deleted INTEGER NOT NULL DEFAULT 0, at INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY(tbl, id));
INSERT INTO durable_rows VALUES('runtime_agents','agent',24,0,0);
INSERT INTO durable_rows VALUES('runtime_agents','other',25,0,0);
INSERT INTO durable_rows VALUES('runtime_agents','child',26,0,0);
CREATE TABLE durable_floor (
        id INTEGER PRIMARY KEY, floor INTEGER NOT NULL);
INSERT INTO durable_floor VALUES(1,0);
CREATE TABLE runtime_agent_volatile(id TEXT PRIMARY KEY,"events" TEXT,"lastEvent" TEXT,"activity" TEXT,"activeTools" TEXT,"tail" TEXT,"lastAnswer" TEXT,"contextUsage" TEXT,"tokensUsed" TEXT,"tokenUsageAccounting" TEXT);
INSERT INTO runtime_agent_volatile VALUES('agent','7','"2026-10-22T17:20:34Z"','{"phase":"writing","at":300.0}','[]','"Needle streamed answer"',NULL,NULL,NULL,NULL);
INSERT INTO runtime_agent_volatile VALUES('other','0',NULL,NULL,'[]',NULL,NULL,NULL,NULL,NULL);
INSERT INTO runtime_agent_volatile VALUES('child','0',NULL,NULL,'[]',NULL,NULL,NULL,NULL,NULL);
CREATE TABLE IF NOT EXISTS "runtime_monitors" (
        id TEXT PRIMARY KEY, record TEXT NOT NULL,
        stream_pending INTEGER GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN COALESCE(json_array_length(record,'$.streamQueue')>0,0) ELSE 0 END) STORED NOT NULL CHECK(stream_pending IN (0,1)),
        monitor_status TEXT GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN CASE WHEN json_type(record,'$.status')='text' AND json_extract(record,'$.status') IN ('approval','starting','running','completed','failed','cancelled','lost') THEN json_extract(record,'$.status') ELSE 'lost' END ELSE 'lost' END) STORED NOT NULL CHECK(monitor_status IN ('approval','starting','running','completed','failed','cancelled','lost')),
        monitor_error_candidate INTEGER GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN json_type(record,'$.status') IS NOT 'text' OR json_extract(record,'$.status') NOT IN ('approval','starting','running','completed','failed','cancelled','lost') OR json_extract(record,'$.status')='lost' OR (json_extract(record,'$.status')='failed' AND json_type(record,'$.exitCode') IS NOT 'integer') ELSE 1 END) STORED NOT NULL CHECK(monitor_error_candidate IN (0,1)),
        monitor_created REAL GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN CASE WHEN json_type(record,'$.created') IN ('integer','real') AND json_extract(record,'$.created')>=0 AND json_extract(record,'$.created')<=1.7976931348623157e308 THEN json_extract(record,'$.created') ELSE 0 END ELSE 0 END) STORED NOT NULL CHECK(monitor_created >= 0 AND monitor_created <= 1.7976931348623157e308)
    );
CREATE TABLE runtime_analytics_projection (seq INTEGER PRIMARY KEY AUTOINCREMENT, record TEXT NOT NULL);
CREATE TABLE runtime_item_blobs (id TEXT PRIMARY KEY, hash TEXT NOT NULL, size INTEGER NOT NULL);
CREATE TABLE runtime_orchestration_settings (id TEXT PRIMARY KEY, record TEXT NOT NULL);
INSERT INTO runtime_orchestration_settings VALUES('global','{"expired_before_ms":1793451775292}');
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
CREATE TABLE runtime_access_clients (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_access_invites (
          id TEXT PRIMARY KEY, request_id TEXT NOT NULL UNIQUE, hash TEXT NOT NULL, record TEXT NOT NULL);
CREATE TABLE runtime_access_nonces (
          client TEXT NOT NULL, nonce TEXT NOT NULL, created INTEGER NOT NULL, PRIMARY KEY(client,nonce));
CREATE TABLE runtime_access_outbound (
          server TEXT NOT NULL, request_id TEXT NOT NULL, hash TEXT NOT NULL,
          record TEXT NOT NULL, receipt_state TEXT GENERATED ALWAYS AS (json_extract(record,'$.outcome')) VIRTUAL NOT NULL CHECK("receipt_state" IN ('unknown','complete','redacted')), PRIMARY KEY(server,request_id));
CREATE TABLE runtime_access_audit (
          seq INTEGER PRIMARY KEY AUTOINCREMENT, client TEXT NOT NULL, actor TEXT NOT NULL,
          action TEXT NOT NULL, created REAL NOT NULL);
CREATE TABLE runtime_access_local_alias (
          id INTEGER PRIMARY KEY CHECK(id=1), alias TEXT NOT NULL);
CREATE TABLE runtime_access_name_targets (
          id TEXT PRIMARY KEY, server TEXT NOT NULL, label TEXT NOT NULL);
CREATE TABLE runtime_access_name_requests (
          id TEXT PRIMARY KEY, label TEXT NOT NULL, actor TEXT NOT NULL, applied INTEGER NOT NULL);
CREATE TABLE runtime_access_alias_requests (
          id TEXT PRIMARY KEY, server TEXT NOT NULL, alias TEXT NOT NULL);
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
INSERT INTO runtime_payload_migrations VALUES('receipts:cutover',0,1,'complete',1793451775.292,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:37:2026-10-28-receipts-execution-prepare:',0,1,'complete',1793451775.292,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:29:2026-10-28-receipts-execution:runtime_execution_runs',3,1,'complete',1793451775.292,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:29:2026-10-28-receipts-execution:runtime_execution_attempts',0,1,'complete',1793451775.292,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:29:2026-10-28-receipts-execution:runtime_execution_effects',1,1,'complete',1793451775.292,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:29:2026-10-28-receipts-execution:runtime_execution_nodes',1,1,'complete',1793451775.292,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:29:2026-10-28-receipts-execution:runtime_agents',3,1,'complete',1793451775.292,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:36:2026-10-28-receipts-execution-finish:',0,1,'complete',1793451775.292,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:35:2026-10-28-receipts-actions-prepare:',0,1,'complete',1793451775.356,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:27:2026-10-28-receipts-actions:runtime_operation_receipts',0,1,'complete',1793451775.356,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:27:2026-10-28-receipts-actions:runtime_native_action_receipts',0,1,'complete',1793451775.356,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:27:2026-10-28-receipts-actions:user_terminal_receipts',0,1,'complete',1793451775.356,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:27:2026-10-28-receipts-actions:runtime_agents',3,1,'complete',1793451775.356,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:27:2026-10-28-receipts-actions:runtime_safety_retries',0,1,'complete',1793451775.356,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:27:2026-10-28-receipts-actions:runtime_limit_reset_attempts',0,1,'complete',1793451775.356,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:27:2026-10-28-receipts-actions:runtime_process_actions',0,1,'complete',1793451775.356,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:27:2026-10-28-receipts-actions:runtime_monitors',0,1,'complete',1793451775.356,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:34:2026-10-28-receipts-actions-finish:',0,1,'complete',1793451775.356,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:42:2026-10-28-receipts-context-repair-prepare:',0,1,'complete',1793451775.475,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:34:2026-10-28-receipts-context-repair:runtime_context_terminal_receipts',0,1,'complete',1793451775.475,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:34:2026-10-28-receipts-context-repair:runtime_agents',3,1,'complete',1793451775.475,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:41:2026-10-28-receipts-context-repair-finish:',0,1,'complete',1793451775.475,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:33:2026-10-28-receipts-voice-prepare:',0,1,'complete',1793451775.535,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:25:2026-10-28-receipts-voice:voice_sessions',0,1,'complete',1793451775.535,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:25:2026-10-28-receipts-voice:runtime_operation_receipts',0,1,'complete',1793451775.535,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:32:2026-10-28-receipts-voice-finish:',0,1,'complete',1793451775.535,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:32:2026-10-28-receipts-work-prepare:',0,1,'complete',1793451775.591,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:24:2026-10-28-receipts-work:work-current-counters',0,1,'complete',1793451775.591,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:24:2026-10-28-receipts-work:runtime_operation_receipts',0,1,'complete',1793451775.591,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:31:2026-10-28-receipts-work-finish:',0,1,'complete',1793451775.591,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:33:2026-10-28-receipts-title-prepare:',0,1,'complete',1793451775.632,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:25:2026-10-28-receipts-title:title-effects',0,1,'complete',1793451775.632,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:25:2026-10-28-receipts-title:title-public',0,1,'complete',1793451775.632,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:32:2026-10-28-receipts-title-finish:',0,1,'complete',1793451775.632,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:39:2026-10-28-receipts-owner-state-prepare:',0,1,'complete',1793451775.7,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:31:2026-10-28-receipts-owner-state:runtime_operation_receipts',0,1,'complete',1793451775.7,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:38:2026-10-28-receipts-owner-state-finish:',0,1,'complete',1793451775.7,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:33:2026-10-28-receipts-tools-prepare:',0,1,'complete',1793451775.725,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:25:2026-10-28-receipts-tools:deleted-completed-c:managed-project-location',0,1,'complete',1793451775.292,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:25:2026-10-28-receipts-tools:normalized-signature:managed-project-location',0,1,'complete',1793451775.292,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:25:2026-10-28-receipts-tools:runtime_tool_request_aliases',0,1,'complete',1793451775.725,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:25:2026-10-28-receipts-tools:runtime_tool_results',0,1,'complete',1793451775.725,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:25:2026-10-28-receipts-tools:runtime_tool_requests',0,1,'complete',1793451775.725,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:32:2026-10-28-receipts-tools-finish:',0,1,'complete',1793451775.725,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:36:2026-10-28-receipts-requests-prepare:',0,1,'complete',1793451775.776,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:28:2026-10-28-receipts-requests:deleted-completed-c:peer-team',0,1,'complete',1793451775.292,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:28:2026-10-28-receipts-requests:deleted-completed-c:sidebar-order',0,1,'complete',1793451775.292,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:28:2026-10-28-receipts-requests:runtime_operation_receipts',0,1,'complete',1793451775.776,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:28:2026-10-28-receipts-requests:messages',0,1,'complete',1793451775.776,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:28:2026-10-28-receipts-requests:runtime_chat_messages',1,1,'complete',1793451775.776,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:28:2026-10-28-receipts-requests:runtime_workspace_operations',0,1,'complete',1793451775.776,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:28:2026-10-28-receipts-requests:runtime_event_meta',1,1,'complete',1793451775.776,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:35:2026-10-28-receipts-requests-finish:',0,1,'complete',1793451775.776,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:34:2026-10-28-receipts-remote-prepare:',0,1,'complete',1793451775.87,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:26:2026-10-28-receipts-remote:normalized-wire:server-outbox-operation',0,1,'complete',1793451775.292,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:26:2026-10-28-receipts-remote:runtime_access_receipts',0,1,'complete',1793451775.87,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:26:2026-10-28-receipts-remote:runtime_access_outbound',0,1,'complete',1793451775.87,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:26:2026-10-28-receipts-remote:runtime_server_inbox',0,1,'complete',1793451775.87,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:26:2026-10-28-receipts-remote:runtime_server_outbox',0,1,'complete',1793451775.87,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:26:2026-10-28-receipts-remote:runtime_server_fetch',0,1,'complete',1793451775.87,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:26:2026-10-28-receipts-remote:runtime_federation_inbox',0,1,'complete',1793451775.87,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:26:2026-10-28-receipts-remote:runtime_federation_outbox',0,1,'complete',1793451775.87,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:33:2026-10-28-receipts-remote-finish:',0,1,'complete',1793451775.87,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:45:2026-10-28-receipts-provider-delivery-prepare:',0,1,'complete',1793451776.029,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:37:2026-10-28-receipts-provider-delivery:runtime_operation_receipts',0,1,'complete',1793451776.029,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:44:2026-10-28-receipts-provider-delivery-finish:',0,1,'complete',1793451776.029,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:46:2026-10-28-receipts-provider-callbacks-prepare:',0,1,'complete',1793451776.065,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:38:2026-10-28-receipts-provider-callbacks:callbacks',0,1,'complete',1793451776.065,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:38:2026-10-28-receipts-provider-callbacks:frames',0,1,'complete',1793451776.065,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:38:2026-10-28-receipts-provider-callbacks:frame-outcomes',0,1,'complete',1793451776.065,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:38:2026-10-28-receipts-provider-callbacks:unanswered',0,1,'complete',1793451776.065,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:45:2026-10-28-receipts-provider-callbacks-finish:',0,1,'complete',1793451776.065,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:39:2026-10-28-receipts-vm-provider-prepare:',0,1,'complete',1793451776.145,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:31:2026-10-28-receipts-vm-provider:runtime_operation_receipts',0,1,'complete',1793451776.145,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:38:2026-10-28-receipts-vm-provider-finish:',0,1,'complete',1793451776.145,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:42:2026-10-28-receipts-account-worker-prepare:',0,1,'complete',1793451776.162,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:34:2026-10-28-receipts-account-worker:journal',0,1,'complete',1793451776.162,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:34:2026-10-28-receipts-account-worker:state',0,1,'complete',1793451776.162,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:34:2026-10-28-receipts-account-worker:calls',0,1,'complete',1793451776.162,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:34:2026-10-28-receipts-account-worker:effects',0,1,'complete',1793451776.162,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:34:2026-10-28-receipts-account-worker:streams',0,1,'complete',1793451776.162,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:34:2026-10-28-receipts-account-worker:stream-pending',0,1,'complete',1793451776.162,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:34:2026-10-28-receipts-account-worker:stream-bases',0,1,'complete',1793451776.162,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:34:2026-10-28-receipts-account-worker:submission-inputs',10,1,'complete',1793451776.162,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:34:2026-10-28-receipts-account-worker:unmatched',0,1,'complete',1793451776.162,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:41:2026-10-28-receipts-account-worker-finish:',0,1,'complete',1793451776.162,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:37:2026-10-28-receipts-incidents-prepare:',0,1,'complete',1793451776.307,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:29:2026-10-28-receipts-incidents:runtime_operation_receipts',0,1,'complete',1793451776.307,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:36:2026-10-28-receipts-incidents-finish:',0,1,'complete',1793451776.307,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:34:2026-10-28-receipts-checks-prepare:',0,1,'complete',1793451776.358,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:26:2026-10-28-receipts-checks:runtime_events',1,1,'complete',1793451776.358,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:26:2026-10-28-receipts-checks:runtime_execution_effects',1,1,'complete',1793451776.358,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:26:2026-10-28-receipts-checks:runtime_payload_migrations',94,1,'complete',1793451776.358,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:33:2026-10-28-receipts-checks-finish:',0,1,'complete',1793451776.358,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:43:2026-10-28-receipts-drop-old-tables-prepare:',0,1,'complete',1793451776.515,NULL,1793451775292);
INSERT INTO runtime_payload_migrations VALUES('receipts:42:2026-10-28-receipts-drop-old-tables-finish:',0,1,'complete',1793451776.515,NULL,1793451775292);
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
CREATE TABLE client_command_watermarks (
    client  TEXT PRIMARY KEY,
    applied INTEGER NOT NULL CHECK (applied >= 0)
) WITHOUT ROWID;
CREATE TABLE runtime_event_revisions(id TEXT PRIMARY KEY, rev INTEGER NOT NULL) WITHOUT ROWID;
CREATE TABLE durable_view_log(
             collection TEXT NOT NULL, id TEXT NOT NULL, seq INTEGER NOT NULL,
             deleted INTEGER NOT NULL, PRIMARY KEY(collection,id));
INSERT INTO durable_view_log VALUES('agent','agent',19,0);
INSERT INTO durable_view_log VALUES('agent','other',3,0);
INSERT INTO durable_view_log VALUES('agent','child',4,0);
CREATE TABLE chats(
            id TEXT PRIMARY KEY, state TEXT NOT NULL, status TEXT NOT NULL,
            auto_wake INTEGER, usage_resume_enabled INTEGER, execution_mode TEXT, turn_epoch INTEGER) STRICT;
INSERT INTO chats VALUES('agent','active','approval',1,NULL,NULL,NULL);
INSERT INTO chats VALUES('other','active','running',1,NULL,NULL,NULL);
INSERT INTO chats VALUES('child','active','running',1,NULL,NULL,NULL);
CREATE TABLE sessions(
            id TEXT PRIMARY KEY, chat TEXT NOT NULL, thread_id TEXT NOT NULL,
            account_key TEXT NOT NULL, state TEXT NOT NULL) STRICT;
INSERT INTO sessions VALUES('agent:thread','agent','thread','default','ready');
INSERT INTO sessions VALUES('other:other-thread','other','other-thread','default','ready');
INSERT INTO sessions VALUES('child:child-thread','child','child-thread','default','ready');
CREATE TABLE turns(
            id TEXT PRIMARY KEY, chat TEXT NOT NULL, state TEXT NOT NULL, session TEXT,
            provider_turn TEXT, attempt TEXT, interrupt_ticket TEXT,
            attempt_submitted INTEGER GENERATED ALWAYS AS (attempt->>'$.submitted') STORED,
            attempt_connection_generation INTEGER GENERATED ALWAYS AS (attempt->>'$.connectionGeneration') STORED,
            attempt_action TEXT GENERATED ALWAYS AS (attempt->>'$.action') STORED,
            attempt_active_at_reservation INTEGER GENERATED ALWAYS AS (attempt->>'$.activeAtReservation') STORED) STRICT;
INSERT INTO turns VALUES('agent:turn','agent','completed',NULL,'turn',NULL,NULL);
INSERT INTO turns VALUES('4826615f-e13e-4194-893d-4cf6880932de','agent','running','agent:thread','turn',NULL,NULL);
INSERT INTO turns VALUES('14ce5cd9-0c64-4ee5-b4d9-27c8920756bb','other','running','other:other-thread','turn',NULL,NULL);
INSERT INTO turns VALUES('9a5214b4-598f-44ed-a896-fbae8ff6bd7b','child','running','child:child-thread','turn',NULL,NULL);
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
CREATE INDEX runtime_access_nonce_age ON runtime_access_nonces(created);
CREATE INDEX runtime_access_outbound_age
          ON runtime_access_outbound(json_extract(record,'$.completed'))
          WHERE json_extract(record,'$.outcome')='complete';
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
