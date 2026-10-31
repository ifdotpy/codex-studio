/* WARNING: Script requires that SQLITE_DBCONFIG_DEFENSIVE be disabled */
PRAGMA foreign_keys=OFF;
BEGIN TRANSACTION;
CREATE TABLE runtime_account_transfers(id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_agent_moves(id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_agent_record_generation (
                    id INTEGER PRIMARY KEY CHECK(id=1), value INTEGER NOT NULL);
INSERT INTO runtime_agent_record_generation VALUES(1,20);
CREATE TABLE runtime_annotations (id TEXT PRIMARY KEY, record TEXT NOT NULL, rev INTEGER NOT NULL DEFAULT 1);
CREATE TABLE runtime_assets (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_budget (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_capacity_retries (id TEXT PRIMARY KEY, agent TEXT NOT NULL, record TEXT NOT NULL);
CREATE TABLE runtime_chat_messages (
                  seq INTEGER PRIMARY KEY AUTOINCREMENT, id TEXT UNIQUE NOT NULL,
                  room TEXT NOT NULL, sender TEXT NOT NULL, text TEXT NOT NULL,
                  created REAL NOT NULL, deliveries TEXT NOT NULL, rev INTEGER NOT NULL DEFAULT 1);
INSERT INTO runtime_chat_messages VALUES(1,'chat','private','child','needle room',210,'{}',1);
INSERT INTO runtime_chat_messages VALUES(2,'chat-peer','peer','agent','needle peer',800,'{}',1);
INSERT INTO runtime_chat_messages VALUES(3,'chat-unavailable','unavailable','agent','needle unavailable',800,'{}',1);
INSERT INTO runtime_chat_messages VALUES(4,'chat-approved','approved','agent','needle approved',800,'{}',1);
INSERT INTO runtime_chat_messages VALUES(5,'chat-unapproved','unapproved','agent','needle unapproved',800,'{}',1);
CREATE TABLE runtime_checkpoints (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_compactions (id TEXT PRIMARY KEY, agent TEXT NOT NULL);
INSERT INTO runtime_compactions VALUES('agent:compact','agent');
CREATE TABLE runtime_complaints (id TEXT PRIMARY KEY, record TEXT NOT NULL);
INSERT INTO runtime_complaints VALUES('complaint','{"id":"complaint","leadId":"agent","title":"needle complaint","text":"needle detail","status":"open","author":null,"recipient":"lead","version":null,"created":null,"updated":null,"readAt":null,"responses":[]}');
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
INSERT INTO runtime_event_meta VALUES('input','{"remoteRequestId":null,"requestedDelivery":"steer","transcriptItemId":"agent:input"}');
INSERT INTO runtime_event_meta VALUES('pending','{"delivery":"current_turn","remoteRequestId":null,"timing":{"enqueuedAt":228874422279577,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO runtime_event_meta VALUES('legacy-event','{"delivery":"current_turn","remoteRequestId":null,"timing":{"enqueuedAt":228874508067265,"fastSkipReason":"scheduleOverride"},"transcriptItemId":"agent:legacy"}');
CREATE TABLE runtime_events (
                  id TEXT PRIMARY KEY, agent TEXT NOT NULL, kind TEXT NOT NULL,
                  text TEXT NOT NULL, status TEXT NOT NULL, created REAL NOT NULL,
                  epoch INTEGER NOT NULL, turn_id TEXT, error TEXT, receipt_enum_guard INTEGER GENERATED ALWAYS AS (NULL) VIRTUAL CHECK("kind" IN ('user','followup','agent_message','child_result','child_stopped','complaint','complaint_response','chat_review','work_review','work_decision','work_ready','monitor_exit','monitor_stall','rule','rule_missed','rule_stall','radio_turn','browser_declined','browser_joined','browser_outcome','browser_unanswered','event_wake','monitor_cancelled','monitor_output','plan_update','shell_policy','work_released','work_result_invalid','context_repair_wait','monitor_correction','rule_hold')) CHECK("status" IN ('stored_only','pending','reserved','dispatching','delivered','uncertain','cancelled','failed')));
INSERT INTO runtime_events VALUES('input','agent','user','Needle submitted input','delivered',200,1,NULL,NULL);
INSERT INTO runtime_events VALUES('pending','agent','user','pending receipt needle','pending',500,1,NULL,NULL);
INSERT INTO runtime_events VALUES('legacy-event','agent','user','needle legacy durable body','delivered',1792689640.537699,1,NULL,NULL);
CREATE TABLE runtime_execution_attempts (id TEXT PRIMARY KEY, run TEXT NOT NULL, record TEXT NOT NULL, submission TEXT GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN record->>'$.submission' END) VIRTUAL CHECK("submission" IN ('unsent','pending','accepted','observed','rejected','unknown')), turn_status TEXT GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN record->>'$.turnStatus' END) VIRTUAL CHECK("turn_status" IN ('unsent','pending','accepted','observed','running','unknown','rejected','completed','failed','interrupted','cancelled')));
CREATE TABLE runtime_execution_effects (id TEXT PRIMARY KEY, run TEXT NOT NULL, kind TEXT NOT NULL, reference TEXT NOT NULL, record TEXT NOT NULL, status TEXT GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN record->>'$.status' END) VIRTUAL CHECK("status" IN ('unknown','idle','queued','starting','running','waiting','waiting_on_monitor','approval','completed','interrupted','failed','blocked','paused','parked','abandoned','capacity-retry','pending','cancelled','answering','answered','uncertain','resolved','lost','expired','deleted')), receipt_enum_guard INTEGER GENERATED ALWAYS AS (NULL) VIRTUAL CHECK("kind" IN ('spawn','tool_requests','requests','monitors','task_submit','unrecognized')));
INSERT INTO runtime_execution_effects VALUES('spawn:child','run:543b6d63-ba17-5996-aa5b-7bc7c6764369','spawn','child','{"id": "spawn:child", "runId": "run:543b6d63-ba17-5996-aa5b-7bc7c6764369", "kind": "spawn", "referenceId": "child", "attemptId": null, "status": "running"}');
CREATE TABLE runtime_execution_inputs (run TEXT NOT NULL, event TEXT NOT NULL, PRIMARY KEY(run,event));
CREATE TABLE runtime_execution_nodes (id TEXT PRIMARY KEY, run TEXT NOT NULL, record TEXT NOT NULL, status TEXT GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN record->>'$.status' END) VIRTUAL CHECK("status" IN ('unknown','idle','queued','starting','running','waiting','waiting_on_monitor','approval','completed','interrupted','failed','blocked','paused','parked','abandoned','capacity-retry')));
INSERT INTO runtime_execution_nodes VALUES('worker:child','run:543b6d63-ba17-5996-aa5b-7bc7c6764369','{"id": "worker:child", "runId": "run:543b6d63-ba17-5996-aa5b-7bc7c6764369", "kind": "managed_worker", "agentId": "child", "status": "running"}');
CREATE TABLE runtime_execution_runs (id TEXT PRIMARY KEY, agent TEXT NOT NULL, account TEXT NOT NULL, epoch INTEGER NOT NULL, thread TEXT, turn TEXT, created REAL NOT NULL, record TEXT NOT NULL, status TEXT GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN record->>'$.status' END) VIRTUAL CHECK("status" IN ('unsent','pending','accepted','observed','running','unknown','rejected','completed','failed','interrupted','cancelled')));
INSERT INTO runtime_execution_runs VALUES('run:543b6d63-ba17-5996-aa5b-7bc7c6764369','agent','default',1,'thread','turn',1792689637.998765,'{"id": "run:543b6d63-ba17-5996-aa5b-7bc7c6764369", "agent": "agent", "accountKey": "default", "epoch": 1, "threadId": "thread", "turnId": "turn", "created": 1792689637.9987645, "status": "running", "firstAttemptId": null, "rootAttemptId": null, "latestAttemptId": null, "reconciled": true}');
INSERT INTO runtime_execution_runs VALUES('run:48f065a1-8be0-543a-9b6f-69bc8581573d','other','default',1,'other-thread','turn',1792689637.9999597,'{"id": "run:48f065a1-8be0-543a-9b6f-69bc8581573d", "agent": "other", "accountKey": "default", "epoch": 1, "threadId": "other-thread", "turnId": "turn", "created": 1792689637.9999597, "status": "running", "firstAttemptId": null, "rootAttemptId": null, "latestAttemptId": null, "reconciled": true}');
INSERT INTO runtime_execution_runs VALUES('run:24b2429b-e38b-5348-a024-3759e25043e1','child','default',1,'child-thread','turn',1792689638.000505,'{"id": "run:24b2429b-e38b-5348-a024-3759e25043e1", "agent": "child", "accountKey": "default", "epoch": 1, "threadId": "child-thread", "turnId": "turn", "created": 1792689638.0005054, "status": "running", "firstAttemptId": null, "rootAttemptId": null, "latestAttemptId": null, "reconciled": true}');
INSERT INTO runtime_execution_runs VALUES('run:f87743b1-c05c-578e-abca-80c319c6f7f4','gone','default',1,'thread','turn',1792689640.534606,'{"id": "run:f87743b1-c05c-578e-abca-80c319c6f7f4", "agent": "gone", "accountKey": "default", "epoch": 1, "threadId": "thread", "turnId": "turn", "created": 1792689640.534606, "status": "running", "firstAttemptId": null, "rootAttemptId": null, "latestAttemptId": null, "reconciled": true}');
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
INSERT INTO runtime_federation_rooms VALUES('approved','{"id": "approved", "kind": "federated", "status": "approved", "localApproved": true, "remoteApproved": true, "localMembers": ["agent"], "remoteMembers": [], "peerId": "remote", "peerLabel": "Remote", "name": "Federated", "shareNames": false, "shareStatus": false, "created": 800.0, "updated": 800.0}');
INSERT INTO runtime_federation_rooms VALUES('unapproved','{"id": "unapproved", "kind": "federated", "status": "pending", "localApproved": true, "remoteApproved": false, "localMembers": ["agent"], "remoteMembers": [], "peerId": "remote", "peerLabel": "Remote", "name": "Federated", "shareNames": false, "shareStatus": false, "created": 800.0, "updated": 800.0}');
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
INSERT INTO runtime_item_fulltext VALUES('agent:dynamic','{"id": "call", "type": "dynamicToolCall", "status": "inProgress", "contentItems": null}');
INSERT INTO runtime_item_fulltext VALUES('agent:legacy','needle legacy durable body');
CREATE TABLE runtime_items (
                  id TEXT PRIMARY KEY, agent TEXT NOT NULL, record TEXT NOT NULL, created REAL NOT NULL);
INSERT INTO runtime_items VALUES('agent:old-1','agent','{"id": "agent:old-1", "role": "assistant", "title": "Agent", "text": "needle history 1", "truncated": false, "at": 101.0, "turnId": "old-turn"}',101);
INSERT INTO runtime_items VALUES('agent:old-2','agent','{"id": "agent:old-2", "role": "assistant", "title": "Agent", "text": "needle history 2", "truncated": false, "at": 102.0, "turnId": "old-turn"}',102);
INSERT INTO runtime_items VALUES('agent:old-3','agent','{"id": "agent:old-3", "role": "assistant", "title": "Agent", "text": "needle history 3", "truncated": false, "at": 103.0, "turnId": "old-turn"}',103);
INSERT INTO runtime_items VALUES('agent:old-4','agent','{"id": "agent:old-4", "role": "assistant", "title": "Agent", "text": "needle history 4", "truncated": false, "at": 104.0, "turnId": "old-turn"}',104);
INSERT INTO runtime_items VALUES('agent:old-5','agent','{"id": "agent:old-5", "role": "assistant", "title": "Agent", "text": "needle history 5", "truncated": false, "at": 105.0, "turnId": "old-turn"}',105);
INSERT INTO runtime_items VALUES('agent:old-6','agent','{"id": "agent:old-6", "role": "assistant", "title": "Agent", "text": "needle history 6", "truncated": false, "at": 106.0, "turnId": "old-turn"}',106);
INSERT INTO runtime_items VALUES('agent:old-7','agent','{"id": "agent:old-7", "role": "assistant", "title": "Agent", "text": "needle history 7", "truncated": false, "at": 107.0, "turnId": "old-turn"}',107);
INSERT INTO runtime_items VALUES('other:other','other','{"id": "other:other", "role": "assistant", "title": "Agent", "text": "needle foreign", "truncated": false, "at": 1792689638.0096738}',1792689638.009678);
INSERT INTO runtime_items VALUES('agent:input','agent','{"id": "agent:input", "role": "user", "title": "You", "text": "Needle submitted input", "truncated": false, "at": 200.0, "turnId": "turn", "inputs": [{"id": "input", "at": 200.0, "kind": "user", "text": "Needle submitted input", "truncated": false, "assets": []}]}',200);
INSERT INTO runtime_items VALUES('agent:answer','agent','{"id": "agent:answer", "role": "assistant", "title": "Assistant", "text": "Needle updated answer", "truncated": false, "at": 300.0, "streaming": true, "turnId": "turn", "phase": null}',300);
INSERT INTO runtime_items VALUES('agent:command','agent','{"id": "agent:command", "role": "output", "title": "commandExecution", "text": "{\"id\": \"command\", \"type\": \"commandExecution\", \"exitCode\": 0, \"aggregatedOutput\": \"needle tool output\", \"outputTruncated\": false, \"durationMs\": 0}", "truncated": false, "at": 300.0, "toolStatus": "completed", "turnId": "turn"}',300);
INSERT INTO runtime_items VALUES('agent:compact','agent','{"id": "agent:compact", "role": "output", "title": "contextCompaction", "text": "{\"id\": \"compact\", \"type\": \"contextCompaction\", \"durationMs\": 0}", "truncated": false, "at": 300.0, "toolStatus": "completed", "turnId": "turn"}',300);
INSERT INTO runtime_items VALUES('agent:dynamic','agent','{"id": "agent:dynamic", "role": "output", "title": "dynamicToolCall", "text": "{\"id\": \"call\", \"type\": \"dynamicToolCall\", \"status\": \"inProgress\", \"contentItems\": null}", "truncated": false, "at": 600.0, "turnId": "turn", "toolStatus": "running"}',600);
INSERT INTO runtime_items VALUES('agent:legacy','agent','{"id":"agent:legacy","role":"user","title":"You","truncated":false,"at":850.0,"inputs":[{"id":"legacy-event","at":null,"kind":"user","text":"short","truncated":false,"assets":[]}]}',1792689640.538707);
CREATE TABLE runtime_lead_requests (id TEXT PRIMARY KEY, agent TEXT NOT NULL, signature TEXT NOT NULL);
CREATE TABLE runtime_plans (id TEXT PRIMARY KEY, record TEXT NOT NULL);
INSERT INTO runtime_plans VALUES('agent','{"id": "agent", "agent": "agent", "rootId": "agent", "text": "needle plan", "version": 1, "updated": 1.0}');
CREATE TABLE runtime_profiles (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_projects (id TEXT PRIMARY KEY, record TEXT NOT NULL, removal_reason TEXT GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN record->>'$.removal.reason' END) VIRTUAL CHECK("removal_reason" IN ('delete')));
INSERT INTO runtime_projects VALUES('@FIXTURE_ROOT@','{"id": "@FIXTURE_ROOT@", "path": "@FIXTURE_ROOT@", "name": "fixture", "peerTeams": [{"id": "peers", "name": "Peers", "members": ["agent", "other"]}], "homeServerId": "fixture-server", "locations": [{"serverId": "fixture-server", "path": "@FIXTURE_ROOT@", "projectId": "@FIXTURE_ROOT@"}], "locationsRevision": 0}');
CREATE TABLE runtime_requests (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_rooms (id TEXT PRIMARY KEY, record TEXT NOT NULL);
INSERT INTO runtime_rooms VALUES('private','{"id": "private", "kind": "private", "rootId": "agent", "members": ["agent", "child"], "updated": 1.0}');
INSERT INTO runtime_rooms VALUES('peer','{"id": "peer", "kind": "private", "rootId": "agent", "members": ["agent", "other"], "updated": 1.0}');
INSERT INTO runtime_rooms VALUES('unavailable','{"id": "unavailable", "kind": "private", "rootId": "agent", "members": ["agent", "gone"], "updated": 1.0}');
INSERT INTO runtime_rooms VALUES('approved','{"id": "approved", "kind": "federated", "rootId": "agent", "members": ["agent"], "updated": 1.0}');
INSERT INTO runtime_rooms VALUES('unapproved','{"id": "unapproved", "kind": "federated", "rootId": "agent", "members": ["agent"], "updated": 1.0}');
CREATE TABLE runtime_rules (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_scheduler_agent_change_state (id INTEGER PRIMARY KEY CHECK(id=1),floor INTEGER NOT NULL,generation INTEGER NOT NULL,entries INTEGER NOT NULL,total INTEGER NOT NULL);
INSERT INTO runtime_scheduler_agent_change_state VALUES(1,0,20,4,74);
CREATE TABLE runtime_scheduler_agent_changes (id TEXT PRIMARY KEY,generation INTEGER NOT NULL CHECK(typeof(generation)='integer' AND generation>=0));
INSERT INTO runtime_scheduler_agent_changes VALUES('agent',17);
INSERT INTO runtime_scheduler_agent_changes VALUES('other',18);
INSERT INTO runtime_scheduler_agent_changes VALUES('child',19);
INSERT INTO runtime_scheduler_agent_changes VALUES('gone',20);
PRAGMA writable_schema=ON;
INSERT INTO sqlite_schema(type,name,tbl_name,rootpage,sql)VALUES('table','runtime_search_next','runtime_search_next',0,'CREATE VIRTUAL TABLE runtime_search_next USING fts5(body,content='''',contentless_delete=1,tokenize=''unicode61'')');
CREATE TABLE IF NOT EXISTS 'runtime_search_next_config'(k PRIMARY KEY, v) WITHOUT ROWID;
INSERT INTO runtime_search_next_config VALUES('version',4);
CREATE TABLE IF NOT EXISTS 'runtime_search_next_data'(id INTEGER PRIMARY KEY, block BLOB);
INSERT INTO runtime_search_next_data VALUES(1,x'0e3d');
INSERT INTO runtime_search_next_data VALUES(10,x'00000000ff000001010f11000f0101010101000001020101020200000103010103030000010401010404000001050101050500000106010106060000010701010707000001080101080800000109010109090000010a01010a0a0000010b01010b0b0102010c01010c0c0000010d01010d0d0000010e01010e0e0000010f01010f0f000001');
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
INSERT INTO runtime_search_next_data VALUES(1924145348609,x'00000063053063616c6c0e0203020b6f6e74656e746974656d730e0208010f64796e616d6963746f6f6c63616c6c0e0205010269640e020202096e70726f67726573730e020701046e756c6c0e020901067374617475730e02060104747970650e020404091014070e090b');
INSERT INTO runtime_search_next_data VALUES(2061584302081,x'0000002f0530626f64790f0205010764757261626c650f020401066c65676163790f020301066e6565646c650f020204090c0b');
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
INSERT INTO runtime_search_next_docsize VALUES(14,x'08',14);
INSERT INTO runtime_search_next_docsize VALUES(15,x'04',15);
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
INSERT INTO runtime_search_next_idx VALUES(14,x'',2);
INSERT INTO runtime_search_next_idx VALUES(15,x'',2);
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
INSERT INTO runtime_search_next_meta VALUES('agent:dynamic','agent','dynamicToolCall',14);
INSERT INTO runtime_search_next_meta VALUES('agent:legacy','agent','You',15);
CREATE TABLE runtime_search_rollout (
                id INTEGER PRIMARY KEY CHECK(id=1), phase TEXT NOT NULL,
                cursor INTEGER NOT NULL DEFAULT 0, updated REAL NOT NULL);
INSERT INTO runtime_search_rollout VALUES(1,'complete',0,1792689637.8706896);
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
INSERT INTO runtime_transcript_revisions VALUES('agent',43);
INSERT INTO runtime_transcript_revisions VALUES('other',4);
INSERT INTO runtime_transcript_revisions VALUES('child',3);
INSERT INTO runtime_transcript_revisions VALUES('gone',2);
CREATE TABLE runtime_usage_resumes (id TEXT PRIMARY KEY, agent TEXT NOT NULL, record TEXT NOT NULL);
CREATE TABLE runtime_work (id TEXT PRIMARY KEY, record TEXT NOT NULL, status TEXT GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN record->>'$.status' END) VIRTUAL CHECK("status" IN ('ready','blocked','running','review','accepted','cancelled')), archive_status TEXT GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN record->>'$.archiveIntent.status' END) VIRTUAL CHECK("archive_status" IN ('completed','terminal','pending','retryable','submitted','unknown','not_sent','complete','applied','failed','cancelled')), archive_next_at REAL GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN COALESCE(record->>'$.archiveIntent.nextAttemptAt',0) END) VIRTUAL, vm_submit_status TEXT GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN record->>'$.vmSubmit.status' END) VIRTUAL CHECK("vm_submit_status" IN ('completed','terminal','pending','retryable','submitted','unknown','not_sent','complete','applied','failed','cancelled')), vm_accept_status TEXT GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN record->>'$.vmAccept.status' END) VIRTUAL CHECK("vm_accept_status" IN ('completed','terminal','pending','retryable','submitted','unknown','not_sent','complete','applied','failed','cancelled')));
INSERT INTO runtime_work VALUES('work','{"id": "work", "rootId": "agent", "title": "needle work", "status": "ready"}');
INSERT INTO runtime_work VALUES('child-work','{"id": "child-work", "rootId": "child", "title": "needle child root", "status": "ready"}');
CREATE TABLE runtime_workspace_operations (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE sync_documents (
        seq INTEGER PRIMARY KEY AUTOINCREMENT, scope TEXT NOT NULL,
        id TEXT NOT NULL, payload TEXT NOT NULL, deleted INTEGER NOT NULL DEFAULT 0,
        UNIQUE(scope,id));
CREATE TABLE sync_entities (
        collection TEXT NOT NULL, id TEXT NOT NULL, seq INTEGER NOT NULL,
        hash TEXT NOT NULL, payload TEXT, deleted INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY(collection,id));
INSERT INTO sync_entities VALUES('workspace','current',570,'2a4db84923fdffae730da9dac1740d104f1aea8eea0c9b8afbee67b2489b66a8','{"collection":"workspace","id":"current","value":{"connected":false,"nativeNotices":[],"peerTeamsVersion":1,"projectOrganizationVersion":1,"sidebarOrder":{"groups":null,"revision":0},"stateDir":"@FIXTURE_ROOT@/python","tasksHistoryLimit":100}}',0);
INSERT INTO sync_entities VALUES('agent','agent',561,'3293609b66441d3efda248080a266969b05b3aa0f7437f67f18a2b9f12836cc5','{"collection":"agent","id":"agent","value":{"accountKey":"default","activity":{"at":300.0,"phase":"writing"},"agentMode":"multi","agentModeRevision":0,"agentModeSupported":true,"autoWake":true,"canSend":true,"compactions":1,"concurrency":32,"contextUsage":null,"cwd":"@FIXTURE_ROOT@","empty":false,"epoch":1,"id":"agent","inFlight":true,"isLead":true,"kind":"agent","lastEvent":"2026-10-22T17:20:38Z","launcherAlive":true,"name":"Transcript fixture","nextTurnSettingsSupported":true,"readStateSupported":true,"rootId":"agent","serverId":"fixture-server","source":"managed","status":"approval","subagentConcurrencyVersion":2,"tail":"Needle streamed answer","threadId":"thread","turnId":"turn","workspaceMode":"image"}}',0);
INSERT INTO sync_entities VALUES('agent','other',3,'b96aaa3e57c6560b3dc97745e2a439b5028a3a1911b0a5b51a0201aee563c350','{"collection":"agent","id":"other","value":{"accountKey":"default","agentMode":"multi","agentModeRevision":0,"agentModeSupported":true,"autoWake":true,"canSend":true,"concurrency":32,"cwd":"@FIXTURE_ROOT@","empty":false,"epoch":1,"id":"other","inFlight":true,"isLead":true,"kind":"agent","launcherAlive":true,"name":"Transcript fixture","nextTurnSettingsSupported":true,"readStateSupported":true,"rootId":"other","serverId":"fixture-server","source":"managed","status":"running","subagentConcurrencyVersion":2,"threadId":"other-thread","turnId":"turn"}}',0);
INSERT INTO sync_entities VALUES('agent','child',4,'9e9c192a7b84f0298b5ba5436c9928cec6da36e65a008ebe3c94543ce9f2f1fd','{"collection":"agent","id":"child","value":{"accountKey":"default","agentMode":"multi","agentModeRevision":0,"agentModeSupported":true,"autoWake":true,"canSend":true,"concurrency":32,"cwd":"@FIXTURE_ROOT@","epoch":1,"id":"child","inFlight":true,"isLead":false,"kind":"agent","launcherAlive":true,"name":"Transcript fixture","nextTurnSettingsSupported":true,"overview":{"result":"","resultFile":null,"resultTruncated":false,"resultTurnId":null,"task":"","taskTruncated":false},"parentId":"agent","readStateSupported":true,"rootId":"agent","serverId":"fixture-server","source":"managed","status":"running","subagentConcurrencyVersion":2,"threadId":"child-thread","turnId":"turn"}}',0);
INSERT INTO sync_entities VALUES('event','input',25,'bff206d03823a5b769e63060cb0763453b2b719399478130fd16096b8e289f95','{"collection":"event","id":"input","value":{"agent":"agent","created":200.0,"error":null,"id":"input","kind":"user","status":"delivered"}}',0);
INSERT INTO sync_entities VALUES('room','private',7,'53df58b94e00bc7d1706ac8d79cae6fecdbce6e7f494160687490829200aa899','{"collection":"room","id":"private","value":{"id":"private","kind":"private","lastMessage":null,"members":["agent","child"],"name":"Transcript fixture ↔ Transcript fixture","rootId":"agent","updated":1.0}}',0);
INSERT INTO sync_entities VALUES('task','agent:command',12,'2af7a8c98cc5344143fa75ebea6446595058e8a29e9cd224fd9f23aada6ff6a3','{"collection":"task","id":"agent:command","value":{"agent":"agent","command":"printf needle","created":300.0,"durationMs":0,"exitCode":0,"finished":300.0,"id":"agent:command","kind":"command","name":"commandExecution","outputTruncated":false,"processId":"42","status":"completed","turnId":"turn"}}',0);
INSERT INTO sync_entities VALUES('task','agent:compact',14,'aafd76c2984f447747e28ce0d00ca6c04a0704bb86cfcddcc757ec57ec637f30','{"collection":"task","id":"agent:compact","value":{"agent":"agent","created":300.0,"durationMs":0,"finished":300.0,"id":"agent:compact","kind":"tool","name":"contextCompaction","status":"completed","turnId":"turn"}}',0);
INSERT INTO sync_entities VALUES('event','pending',27,'07a0ed3c4c70bbfacb3e1953bf1364f9b60cdfd73f772a69e0eb77ebe3c50869','{"collection":"event","id":"pending","value":{"agent":"agent","created":500.0,"error":null,"id":"pending","kind":"user","status":"pending"}}',0);
INSERT INTO sync_entities VALUES('transcript:agent','item:agent:old-1',29,'d7691a12cf5a07d416f04bad6e765e8a9726d49000c79aab7c483d81256f46d6',NULL,0);
INSERT INTO sync_entities VALUES('transcript:agent','item:agent:old-2',30,'c6f47674d75b4a9be1c2234aeb869c7300bf7ea8d24af5679a2d47a50feadef2',NULL,0);
INSERT INTO sync_entities VALUES('transcript:agent','item:agent:old-3',31,'13a8de652b968460e7ee53d1bd87a3c9b6e8e8d17a99feeb3c84c4c92675dc6f',NULL,0);
INSERT INTO sync_entities VALUES('transcript:agent','item:agent:old-4',32,'c0c268158ed5bbc9652588fb95795c56bc476faa0e54e8e1a4c890058bd156b1',NULL,0);
INSERT INTO sync_entities VALUES('transcript:agent','item:agent:old-5',33,'6a03dc6fa2c5f8b554893687e15bb68f459854ec47db888dcb304fbbaaac0e7f',NULL,0);
INSERT INTO sync_entities VALUES('transcript:agent','item:agent:old-6',34,'a0ab283be23e339f7b118c7b9834b23e6ccbec768576a1fae535c24d05ccc9e1',NULL,0);
INSERT INTO sync_entities VALUES('transcript:agent','item:agent:old-7',35,'78ab4a25d68ea456e7802cb6d8f6fa7e9add2647da7b6a8402359d9c1b4984e0',NULL,0);
INSERT INTO sync_entities VALUES('transcript:agent','item:agent:input',36,'a017bf6c56f7d24d197ca72e5217e9f657223699d313f6fdf13b2ef2ef6bdfce',NULL,0);
INSERT INTO sync_entities VALUES('transcript:agent','item:agent:answer',44,'e9e5be0bdc89f23fd49fc80687728696f07559b94e992cb11fbd6416925606f0',NULL,0);
INSERT INTO sync_entities VALUES('transcript:agent','item:agent:command',38,'0104d41b94f4c9e0fcdcb71623b0413d7158539c887a4e328a5a069de44c838f',NULL,0);
INSERT INTO sync_entities VALUES('transcript:agent','item:agent:compact',39,'383fa2e16eb15f5801958fb85e6e9bd57a9e463806c601d818b834f9253c2541',NULL,0);
INSERT INTO sync_entities VALUES('transcript:agent','item:agent:pending',40,'55465de0e6f5ee108b1173ea590f1b600647b1f0c932b55a1f9ab16ec8ecc634',NULL,0);
INSERT INTO sync_entities VALUES('transcript:agent','@order',46,'ffa8eda309acc4023f59146efb5a0e02da71bbae602d1cd17f7b26f888c802d5',NULL,0);
INSERT INTO sync_entities VALUES('transcript:agent','@meta',42,'2dff93b54c1092bc22009f56070df43e2c74db861e6097b42f8d15bd35907cb0',NULL,0);
INSERT INTO sync_entities VALUES('transcript:agent','@revision',47,'7a61b53701befdae0eeeffaecc73f14e20b537bb0f8b91ad7c2936dc63562b25',NULL,0);
INSERT INTO sync_entities VALUES('transcript:agent','@floor',48,'98010bd9270f9b100b6214a21754fd33bdc8d41b2bc9f9dd16ff54d3c34ffd71',NULL,0);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-1',49,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-2',50,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-3',51,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-4',52,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-5',53,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-6',54,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-7',55,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-8',56,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-9',57,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-10',58,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-11',59,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-12',60,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-13',61,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-14',62,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-15',63,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-16',64,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-17',65,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-18',66,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-19',67,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-20',68,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-21',69,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-22',70,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-23',71,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-24',72,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-25',73,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-26',74,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-27',75,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-28',76,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-29',77,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-30',78,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-31',79,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-32',80,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-33',81,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-34',82,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-35',83,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-36',84,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-37',85,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-38',86,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-39',87,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-40',88,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-41',89,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-42',90,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-43',91,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-44',92,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-45',93,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-46',94,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-47',95,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-48',96,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-49',97,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-50',98,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-51',99,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-52',100,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-53',101,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-54',102,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-55',103,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-56',104,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-57',105,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-58',106,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-59',107,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-60',108,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-61',109,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-62',110,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-63',111,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-64',112,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-65',113,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-66',114,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-67',115,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-68',116,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-69',117,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-70',118,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-71',119,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-72',120,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-73',121,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-74',122,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-75',123,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-76',124,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-77',125,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-78',126,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-79',127,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-80',128,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-81',129,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-82',130,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-83',131,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-84',132,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-85',133,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-86',134,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-87',135,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-88',136,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-89',137,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-90',138,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-91',139,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-92',140,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-93',141,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-94',142,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-95',143,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-96',144,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-97',145,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-98',146,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-99',147,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-100',148,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-101',149,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-102',150,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-103',151,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-104',152,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-105',153,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-106',154,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-107',155,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-108',156,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-109',157,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-110',158,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-111',159,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-112',160,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-113',161,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-114',162,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-115',163,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-116',164,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-117',165,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-118',166,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-119',167,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-120',168,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-121',169,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-122',170,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-123',171,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-124',172,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-125',173,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-126',174,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-127',175,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-128',176,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-129',177,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-130',178,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-131',179,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-132',180,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-133',181,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-134',182,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-135',183,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-136',184,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-137',185,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-138',186,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-139',187,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-140',188,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-141',189,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-142',190,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-143',191,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-144',192,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-145',193,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-146',194,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-147',195,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-148',196,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-149',197,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-150',198,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-151',199,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-152',200,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-153',201,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-154',202,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-155',203,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-156',204,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-157',205,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-158',206,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-159',207,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-160',208,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-161',209,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-162',210,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-163',211,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-164',212,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-165',213,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-166',214,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-167',215,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-168',216,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-169',217,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-170',218,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-171',219,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-172',220,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-173',221,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-174',222,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-175',223,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-176',224,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-177',225,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-178',226,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-179',227,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-180',228,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-181',229,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-182',230,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-183',231,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-184',232,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-185',233,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-186',234,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-187',235,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-188',236,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-189',237,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-190',238,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-191',239,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-192',240,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-193',241,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-194',242,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-195',243,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-196',244,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-197',245,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-198',246,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-199',247,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-200',248,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-201',249,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-202',250,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-203',251,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-204',252,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-205',253,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-206',254,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-207',255,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-208',256,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-209',257,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-210',258,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-211',259,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-212',260,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-213',261,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-214',262,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-215',263,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-216',264,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-217',265,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-218',266,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-219',267,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-220',268,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-221',269,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-222',270,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-223',271,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-224',272,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-225',273,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-226',274,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-227',275,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-228',276,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-229',277,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-230',278,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-231',279,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-232',280,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-233',281,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-234',282,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-235',283,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-236',284,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-237',285,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-238',286,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-239',287,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-240',288,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-241',289,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-242',290,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-243',291,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-244',292,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-245',293,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-246',294,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-247',295,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-248',296,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-249',297,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-250',298,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-251',299,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-252',300,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-253',301,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-254',302,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-255',303,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-256',304,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-257',305,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-258',306,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-259',307,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-260',308,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-261',309,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-262',310,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-263',311,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-264',312,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-265',313,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-266',314,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-267',315,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-268',316,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-269',317,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-270',318,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-271',319,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-272',320,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-273',321,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-274',322,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-275',323,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-276',324,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-277',325,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-278',326,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-279',327,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-280',328,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-281',329,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-282',330,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-283',331,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-284',332,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-285',333,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-286',334,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-287',335,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-288',336,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-289',337,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-290',338,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-291',339,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-292',340,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-293',341,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-294',342,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-295',343,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-296',344,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-297',345,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-298',346,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-299',347,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-300',348,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-301',349,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-302',350,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-303',351,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-304',352,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-305',353,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-306',354,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-307',355,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-308',356,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-309',357,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-310',358,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-311',359,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-312',360,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-313',361,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-314',362,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-315',363,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-316',364,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-317',365,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-318',366,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-319',367,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-320',368,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-321',369,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-322',370,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-323',371,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-324',372,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-325',373,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-326',374,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-327',375,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-328',376,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-329',377,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-330',378,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-331',379,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-332',380,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-333',381,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-334',382,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-335',383,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-336',384,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-337',385,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-338',386,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-339',387,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-340',388,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-341',389,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-342',390,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-343',391,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-344',392,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-345',393,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-346',394,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-347',395,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-348',396,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-349',397,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-350',398,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-351',399,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-352',400,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-353',401,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-354',402,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-355',403,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-356',404,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-357',405,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-358',406,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-359',407,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-360',408,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-361',409,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-362',410,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-363',411,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-364',412,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-365',413,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-366',414,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-367',415,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-368',416,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-369',417,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-370',418,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-371',419,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-372',420,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-373',421,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-374',422,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-375',423,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-376',424,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-377',425,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-378',426,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-379',427,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-380',428,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-381',429,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-382',430,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-383',431,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-384',432,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-385',433,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-386',434,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-387',435,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-388',436,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-389',437,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-390',438,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-391',439,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-392',440,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-393',441,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-394',442,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-395',443,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-396',444,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-397',445,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-398',446,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-399',447,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-400',448,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-401',449,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-402',450,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-403',451,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-404',452,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-405',453,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-406',454,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-407',455,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-408',456,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-409',457,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-410',458,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-411',459,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-412',460,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-413',461,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-414',462,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-415',463,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-416',464,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-417',465,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-418',466,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-419',467,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-420',468,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-421',469,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-422',470,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-423',471,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-424',472,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-425',473,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-426',474,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-427',475,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-428',476,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-429',477,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-430',478,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-431',479,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-432',480,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-433',481,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-434',482,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-435',483,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-436',484,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-437',485,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-438',486,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-439',487,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-440',488,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-441',489,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-442',490,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-443',491,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-444',492,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-445',493,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-446',494,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-447',495,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-448',496,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-449',497,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-450',498,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-451',499,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-452',500,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-453',501,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-454',502,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-455',503,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-456',504,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-457',505,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-458',506,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-459',507,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-460',508,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-461',509,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-462',510,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-463',511,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-464',512,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-465',513,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-466',514,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-467',515,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-468',516,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-469',517,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-470',518,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-471',519,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-472',520,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-473',521,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-474',522,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-475',523,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-476',524,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-477',525,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-478',526,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-479',527,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-480',528,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-481',529,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-482',530,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-483',531,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-484',532,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-485',533,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-486',534,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-487',535,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-488',536,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-489',537,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-490',538,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-491',539,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-492',540,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-493',541,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-494',542,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-495',543,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-496',544,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-497',545,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-498',546,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-499',547,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-500',548,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-501',549,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-502',550,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-503',551,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-504',552,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-505',553,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-506',554,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-507',555,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-508',556,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-509',557,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-510',558,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-511',559,'stale',NULL,1);
INSERT INTO sync_entities VALUES('transcript:agent','item:stale-512',560,'stale',NULL,1);
INSERT INTO sync_entities VALUES('agent','gone',562,'e80d513d643858e5c9bff7b17b40fb4b2af298215bc26d3ee603dfc2c233f542','{"collection":"agent","id":"gone","value":{}}',1);
INSERT INTO sync_entities VALUES('project','@FIXTURE_ROOT@',563,'d423b005768e13394f79c51017f624f241fbf2d5339d4cd1bd643ec572645b40','{"collection":"project","id":"@FIXTURE_ROOT@","value":{"homeServerId":"fixture-server","id":"@FIXTURE_ROOT@","locations":[{"path":"@FIXTURE_ROOT@","projectId":"@FIXTURE_ROOT@","serverId":"fixture-server"}],"locationsRevision":0,"name":"fixture","path":"@FIXTURE_ROOT@","peerTeams":[{"id":"peers","members":["agent","other"],"name":"Peers"}]}}',0);
INSERT INTO sync_entities VALUES('work','work',564,'aabb57526846e96b3e42676b854f6fac3c0716471236c07bab860fc85b9ccb77','{"collection":"work","id":"work","value":{"id":"work","rootId":"agent","status":"ready","title":"needle work"}}',0);
INSERT INTO sync_entities VALUES('work','child-work',565,'f75bed3c0f9f416f778fb76bbc6760fcd710c3b2d1983001b8fa0b9d0544d17f','{"collection":"work","id":"child-work","value":{"id":"child-work","rootId":"child","status":"ready","title":"needle child root"}}',0);
INSERT INTO sync_entities VALUES('complaint','complaint',566,'f63dc2706cd91550aa1138e5cd9575e94c9723182dad637765d4b16acb137d0c','{"collection":"complaint","id":"complaint","value":{"author":null,"authorName":null,"created":null,"id":"complaint","leadId":"agent","leadName":"Transcript fixture","needsUserResponse":false,"readAt":null,"recipient":"lead","status":"open","title":"needle detail","updated":null,"version":null}}',0);
INSERT INTO sync_entities VALUES('room','peer',567,'6f341bf2e6ba4816552f70872aca418ae7303548a3a99d5b2bdf2c7c3335c333','{"collection":"room","id":"peer","value":{"id":"peer","kind":"private","lastMessage":null,"members":["agent","other"],"name":"Transcript fixture ↔ Transcript fixture","peerTeamId":"peers","peerTeamName":"Peers","rootId":"agent","updated":1.0}}',0);
INSERT INTO sync_entities VALUES('event','legacy-event',569,'05f1b967815f0155b188ec680dc49e06a0639a847d49ce9aaf15f8a1acaa4ea2','{"collection":"event","id":"legacy-event","value":{"agent":"agent","created":1792689640.5376985,"error":null,"id":"legacy-event","kind":"user","status":"delivered"}}',0);
CREATE TABLE sync_entity_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
INSERT INTO sync_entity_meta VALUES('entity_tombstone_count','1');
INSERT INTO sync_entity_meta VALUES('task_window_migrated','1');
INSERT INTO sync_entity_meta VALUES('event_window_seq','569');
INSERT INTO sync_entity_meta VALUES('seeded','1');
INSERT INTO sync_entity_meta VALUES('agent_organization_fields','2');
CREATE TABLE sync_generation (id INTEGER PRIMARY KEY, value INTEGER NOT NULL);
INSERT INTO sync_generation VALUES(1,0);
CREATE TABLE sync_identity (id TEXT PRIMARY KEY);
INSERT INTO sync_identity VALUES('6f99fae733fe413c97db00d16663fa9d');
CREATE TABLE IF NOT EXISTS "runtime_agents"(id TEXT PRIMARY KEY,"root_id" TEXT GENERATED ALWAYS AS (record->>'$.rootId') STORED ,"parent_id" TEXT GENERATED ALWAYS AS (record->>'$.parentId') STORED ,"status" TEXT GENERATED ALWAYS AS (record->>'$.status') STORED ,"account_key" TEXT GENERATED ALWAYS AS (COALESCE(record->>'$.accountKey','default')) STORED NOT NULL,"provider" TEXT GENERATED ALWAYS AS (record->>'$.provider') STORED NOT NULL,"role" TEXT GENERATED ALWAYS AS (record->>'$.role') STORED ,"deleted_at"  GENERATED ALWAYS AS (record->>'$.deletedAt') STORED ,"thread_id" TEXT GENERATED ALWAYS AS (record->>'$.threadId') STORED ,"turn_id" TEXT GENERATED ALWAYS AS (record->>'$.turnId') STORED ,"cwd" TEXT GENERATED ALWAYS AS (record->>'$.cwd') STORED ,"workspace_operation"  GENERATED ALWAYS AS (record->>'$.workspaceOperation') STORED ,"in_flight" INTEGER GENERATED ALWAYS AS (record->>'$.inFlight') STORED ,"auto_wake" INTEGER GENERATED ALWAYS AS (record->>'$.autoWake') STORED ,"remote_worker"  GENERATED ALWAYS AS (record->>'$.remoteWorker') STORED ,"remote_anchor"  GENERATED ALWAYS AS (record->>'$.remoteAnchor') STORED ,"native_failure_hold"  GENERATED ALWAYS AS (record->>'$.nativeFailureHold') STORED ,"steer_rejected_turn_id" TEXT GENERATED ALWAYS AS (record->>'$.steerRejectedTurnId') STORED ,"execution_mode" TEXT GENERATED ALWAYS AS (record->>'$.executionMode') STORED ,"native_tool_refresh_id" TEXT GENERATED ALWAYS AS (record->>'$.nativeToolRefreshId') STORED ,"context_repair_phase" TEXT GENERATED ALWAYS AS (record->>'$.contextRepair.phase') STORED ,"source_cleanup_phase" TEXT GENERATED ALWAYS AS (record->>'$.contextRepair.sourceCleanup.phase') STORED ,"source_cleanup_account_key" TEXT GENERATED ALWAYS AS (record->>'$.contextRepair.sourceCleanup.accountKey') STORED ,"source_cleanup_thread_id" TEXT GENERATED ALWAYS AS (record->>'$.contextRepair.sourceCleanup.threadId') STORED ,"has_disconnect_recovery" INTEGER GENERATED ALWAYS AS (COALESCE(json_type(record,'$.disconnectRecovery')='object',0)) STORED NOT NULL,"disconnect_recovery_stage" TEXT GENERATED ALWAYS AS (record->>'$.disconnectRecovery.stage') STORED ,"has_restart_recovery" INTEGER GENERATED ALWAYS AS (COALESCE(json_type(record,'$.restartRecovery')='object',0)) STORED NOT NULL,"restart_recovery_stage" TEXT GENERATED ALWAYS AS (record->>'$.restartRecovery.stage') STORED ,"has_workspace_cleanup_intent" INTEGER GENERATED ALWAYS AS (COALESCE(json_type(record,'$.workspaceCleanupIntent')='object',0)) STORED NOT NULL,"agent_archived" INTEGER GENERATED ALWAYS AS (NOT (json_type(record,'$.agentArchive') IS NULL OR json_type(record,'$.agentArchive') IN ('null','false') OR (json_type(record,'$.agentArchive') IN ('integer','real') AND record->>'$.agentArchive'=0) OR (json_type(record,'$.agentArchive')='text' AND record->>'$.agentArchive'='') OR (json_type(record,'$.agentArchive')='array' AND json_array_length(record,'$.agentArchive')=0) OR (json_type(record,'$.agentArchive')='object' AND record->>'$.agentArchive'='{}'))) STORED NOT NULL,"review_archive_failed" INTEGER GENERATED ALWAYS AS (NOT (json_type(record,'$.reviewArchiveError') IS NULL OR json_type(record,'$.reviewArchiveError') IN ('null','false') OR (json_type(record,'$.reviewArchiveError') IN ('integer','real') AND record->>'$.reviewArchiveError'=0) OR (json_type(record,'$.reviewArchiveError')='text' AND record->>'$.reviewArchiveError'='') OR (json_type(record,'$.reviewArchiveError')='array' AND json_array_length(record,'$.reviewArchiveError')=0) OR (json_type(record,'$.reviewArchiveError')='object' AND record->>'$.reviewArchiveError'='{}'))) STORED NOT NULL,"review_archive_marked" INTEGER GENERATED ALWAYS AS (record->>'$.reviewArchiveScheduled' IS NOT NULL) STORED NOT NULL,"review_archive_scheduled" INTEGER GENERATED ALWAYS AS (COALESCE(json_type(record,'$.reviewArchiveScheduled')='text' AND record->>'$.reviewArchiveScheduled'<>'',0)) STORED NOT NULL,"review_archive_next_at"  GENERATED ALWAYS AS (record->>'$.reviewArchiveNextAt') STORED ,"idle_episode_type" TEXT GENERATED ALWAYS AS (json_type(record,'$.idleCompactionEpisode')) STORED ,"idle_episode_fire_at"  GENERATED ALWAYS AS (record->>'$.idleCompactionEpisode.fireAt') STORED ,"idle_episode_id"  GENERATED ALWAYS AS (record->>'$.idleCompactionEpisode.id') STORED ,"legacy_unsettled" INTEGER GENERATED ALWAYS AS (COALESCE(record->>'$.workspaceMode'='image' OR record->>'$.imageWorkspace'=1,0) AND COALESCE(record->>'$.legacyWorkspaceSettled',0)=0) STORED NOT NULL,record TEXT NOT NULL,hold_status TEXT NOT NULL DEFAULT 'clear' CHECK("hold_status" IN ('clear','invalid','unrecognized')),hold_incident TEXT,title_status TEXT GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN record->>'$.titleJob.status' END) VIRTUAL CHECK("title_status" IN ('scheduled','written','invalid','discarded','pending','submitted','unknown','not_sent','applied','failed')),vm_launch_status TEXT GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN record->>'$.vmLaunch.status' END) VIRTUAL CHECK("vm_launch_status" IN ('completed','terminal','pending','retryable','submitted','unknown','not_sent','complete','applied','failed','cancelled')),idle_episode_outcome TEXT GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN record->>'$.idleCompactionEpisode.outcome' END) VIRTUAL CHECK("idle_episode_outcome" IN ('compacted','disabled','compaction_off','idle_compaction_off','prefix_changed','started','dispatched','failed','ttl_changed','lapsed','small','newer_request','near_limit','active','queued_input','stopped','no_host','would_compact')),provider_stream_generation INTEGER GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN record->>'$.providerStream.generation' END) VIRTUAL,provider_stream_ack INTEGER GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN record->>'$.providerStream.ack' END) VIRTUAL);
INSERT INTO runtime_agents VALUES('agent','{"accountKey":"default","activeExecution":{"attemptId":null,"runId":"run:543b6d63-ba17-5996-aa5b-7bc7c6764369"},"agentMode":"multi","agentModeRevision":0,"agentModeSupported":true,"autoWake":true,"compactions":1,"concurrency":32,"cwd":"@FIXTURE_ROOT@","epoch":1,"id":"agent","inFlight":true,"isLead":true,"maxAgentsExplicit":false,"name":"Transcript fixture","nativeResponseTurn":"turn","provider":"codex","rootId":"agent","status":"approval","subagentConcurrencyVersion":2,"threadId":"thread","turnId":"turn","workspaceMode":"image"}','clear',NULL);
INSERT INTO runtime_agents VALUES('other','{"accountKey":"default","activeExecution":{"attemptId":null,"runId":"run:48f065a1-8be0-543a-9b6f-69bc8581573d"},"agentMode":"multi","agentModeRevision":0,"agentModeSupported":true,"autoWake":true,"compactions":0,"concurrency":32,"cwd":"@FIXTURE_ROOT@","epoch":1,"id":"other","inFlight":true,"isLead":true,"maxAgentsExplicit":false,"name":"Transcript fixture","provider":"codex","rootId":"other","status":"running","subagentConcurrencyVersion":2,"threadId":"other-thread","turnId":"turn"}','clear',NULL);
INSERT INTO runtime_agents VALUES('child','{"accountKey":"default","activeExecution":{"attemptId":null,"runId":"run:24b2429b-e38b-5348-a024-3759e25043e1"},"agentMode":"multi","agentModeRevision":0,"agentModeSupported":true,"autoWake":true,"compactions":0,"concurrency":32,"cwd":"@FIXTURE_ROOT@","epoch":1,"id":"child","inFlight":true,"isLead":false,"maxAgentsExplicit":false,"name":"Transcript fixture","parentId":"agent","provider":"codex","rootId":"agent","status":"running","subagentConcurrencyVersion":2,"threadId":"child-thread","turnId":"turn"}','clear',NULL);
INSERT INTO runtime_agents VALUES('gone','{"accountKey":"default","activeExecution":{"attemptId":null,"runId":"run:f87743b1-c05c-578e-abca-80c319c6f7f4"},"agentMode":"multi","agentModeRevision":0,"agentModeSupported":true,"autoWake":true,"compactions":1,"concurrency":32,"cwd":"@FIXTURE_ROOT@","deletedAt":1.0,"epoch":1,"id":"gone","inFlight":true,"isLead":true,"maxAgentsExplicit":false,"name":"Transcript fixture","nativeResponseTurn":"turn","provider":"codex","rootId":"agent","status":"approval","subagentConcurrencyVersion":2,"threadId":"thread","turnId":"turn","workspaceMode":"image"}','clear',NULL);
CREATE TABLE durable_rows (
        tbl TEXT NOT NULL, id TEXT NOT NULL, seq INTEGER NOT NULL,
        deleted INTEGER NOT NULL DEFAULT 0, at INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY(tbl, id));
INSERT INTO durable_rows VALUES('runtime_agents','agent',577,0,0);
INSERT INTO durable_rows VALUES('runtime_agents','other',578,0,0);
INSERT INTO durable_rows VALUES('runtime_agents','child',579,0,0);
INSERT INTO durable_rows VALUES('runtime_agents','gone',580,0,0);
INSERT INTO durable_rows VALUES('runtime_work','work',575,0,0);
INSERT INTO durable_rows VALUES('runtime_work','child-work',576,0,0);
CREATE TABLE durable_floor (
        id INTEGER PRIMARY KEY, floor INTEGER NOT NULL);
INSERT INTO durable_floor VALUES(1,0);
CREATE TABLE runtime_agent_volatile(id TEXT PRIMARY KEY,"events" TEXT,"lastEvent" TEXT,"activity" TEXT,"activeTools" TEXT,"tail" TEXT,"lastAnswer" TEXT,"contextUsage" TEXT,"tokensUsed" TEXT,"tokenUsageAccounting" TEXT);
INSERT INTO runtime_agent_volatile VALUES('agent','7','"2026-10-22T17:20:38Z"','{"phase":"writing","at":300.0}','[]','"Needle streamed answer"',NULL,NULL,NULL,NULL);
INSERT INTO runtime_agent_volatile VALUES('other','0',NULL,NULL,'[]',NULL,NULL,NULL,NULL,NULL);
INSERT INTO runtime_agent_volatile VALUES('child','0',NULL,NULL,'[]',NULL,NULL,NULL,NULL,NULL);
INSERT INTO runtime_agent_volatile VALUES('gone','7','"2026-10-22T17:20:38Z"','{"phase":"writing","at":300.0}','[]','"Needle streamed answer"',NULL,NULL,NULL,NULL);
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
INSERT INTO runtime_orchestration_settings VALUES('global','{"expired_before_ms":1793451803162}');
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
INSERT INTO runtime_payload_migrations VALUES('receipts:cutover',0,1,'complete',1793451803.162,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:37:2026-10-28-receipts-execution-prepare:',0,1,'complete',1793451803.162,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:29:2026-10-28-receipts-execution:runtime_execution_runs',4,1,'complete',1793451803.162,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:29:2026-10-28-receipts-execution:runtime_execution_attempts',0,1,'complete',1793451803.162,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:29:2026-10-28-receipts-execution:runtime_execution_effects',1,1,'complete',1793451803.162,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:29:2026-10-28-receipts-execution:runtime_execution_nodes',1,1,'complete',1793451803.162,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:29:2026-10-28-receipts-execution:runtime_agents',4,1,'complete',1793451803.162,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:36:2026-10-28-receipts-execution-finish:',0,1,'complete',1793451803.162,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:35:2026-10-28-receipts-actions-prepare:',0,1,'complete',1793451803.255,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:27:2026-10-28-receipts-actions:runtime_operation_receipts',0,1,'complete',1793451803.255,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:27:2026-10-28-receipts-actions:runtime_native_action_receipts',0,1,'complete',1793451803.255,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:27:2026-10-28-receipts-actions:user_terminal_receipts',0,1,'complete',1793451803.255,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:27:2026-10-28-receipts-actions:runtime_agents',4,1,'complete',1793451803.255,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:27:2026-10-28-receipts-actions:runtime_safety_retries',0,1,'complete',1793451803.255,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:27:2026-10-28-receipts-actions:runtime_limit_reset_attempts',0,1,'complete',1793451803.255,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:27:2026-10-28-receipts-actions:runtime_process_actions',0,1,'complete',1793451803.255,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:27:2026-10-28-receipts-actions:runtime_monitors',0,1,'complete',1793451803.255,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:34:2026-10-28-receipts-actions-finish:',0,1,'complete',1793451803.255,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:42:2026-10-28-receipts-context-repair-prepare:',0,1,'complete',1793451803.383,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:34:2026-10-28-receipts-context-repair:runtime_context_terminal_receipts',0,1,'complete',1793451803.383,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:34:2026-10-28-receipts-context-repair:runtime_agents',4,1,'complete',1793451803.383,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:41:2026-10-28-receipts-context-repair-finish:',0,1,'complete',1793451803.383,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:33:2026-10-28-receipts-voice-prepare:',0,1,'complete',1793451803.437,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:25:2026-10-28-receipts-voice:voice_sessions',0,1,'complete',1793451803.437,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:25:2026-10-28-receipts-voice:runtime_operation_receipts',0,1,'complete',1793451803.437,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:32:2026-10-28-receipts-voice-finish:',0,1,'complete',1793451803.437,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:32:2026-10-28-receipts-work-prepare:',0,1,'complete',1793451803.48,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:24:2026-10-28-receipts-work:work-current-counters',2,1,'complete',1793451803.48,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:24:2026-10-28-receipts-work:runtime_operation_receipts',0,1,'complete',1793451803.48,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:31:2026-10-28-receipts-work-finish:',0,1,'complete',1793451803.48,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:33:2026-10-28-receipts-title-prepare:',0,1,'complete',1793451803.552,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:25:2026-10-28-receipts-title:title-effects',0,1,'complete',1793451803.552,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:25:2026-10-28-receipts-title:title-public',0,1,'complete',1793451803.552,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:32:2026-10-28-receipts-title-finish:',0,1,'complete',1793451803.552,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:39:2026-10-28-receipts-owner-state-prepare:',0,1,'complete',1793451804.443,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:31:2026-10-28-receipts-owner-state:runtime_operation_receipts',0,1,'complete',1793451804.443,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:38:2026-10-28-receipts-owner-state-finish:',0,1,'complete',1793451804.443,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:33:2026-10-28-receipts-tools-prepare:',0,1,'complete',1793451804.502,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:25:2026-10-28-receipts-tools:deleted-completed-c:managed-project-location',0,1,'complete',1793451803.162,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:25:2026-10-28-receipts-tools:normalized-signature:managed-project-location',0,1,'complete',1793451803.162,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:25:2026-10-28-receipts-tools:runtime_tool_request_aliases',0,1,'complete',1793452080.023,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:25:2026-10-28-receipts-tools:runtime_tool_results',0,1,'complete',1793452080.023,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:25:2026-10-28-receipts-tools:runtime_tool_requests',0,1,'complete',1793452080.023,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:32:2026-10-28-receipts-tools-finish:',0,1,'complete',1793452080.023,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:36:2026-10-28-receipts-requests-prepare:',0,1,'complete',1793452080.07,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:28:2026-10-28-receipts-requests:deleted-completed-c:peer-team',0,1,'complete',1793451803.162,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:28:2026-10-28-receipts-requests:deleted-completed-c:sidebar-order',0,1,'complete',1793451803.162,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:28:2026-10-28-receipts-requests:runtime_operation_receipts',0,1,'complete',1793452080.07,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:28:2026-10-28-receipts-requests:messages',0,1,'complete',1793452080.07,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:28:2026-10-28-receipts-requests:runtime_chat_messages',5,1,'complete',1793452080.07,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:28:2026-10-28-receipts-requests:runtime_workspace_operations',0,1,'complete',1793452080.07,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:28:2026-10-28-receipts-requests:runtime_event_meta',3,1,'complete',1793452080.07,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:35:2026-10-28-receipts-requests-finish:',0,1,'complete',1793452080.07,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:34:2026-10-28-receipts-remote-prepare:',0,1,'complete',1793452080.184,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:26:2026-10-28-receipts-remote:normalized-wire:server-outbox-operation',0,1,'complete',1793451803.162,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:26:2026-10-28-receipts-remote:runtime_access_receipts',0,1,'complete',1793452080.184,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:26:2026-10-28-receipts-remote:runtime_access_outbound',0,1,'complete',1793452080.184,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:26:2026-10-28-receipts-remote:runtime_server_inbox',0,1,'complete',1793452080.184,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:26:2026-10-28-receipts-remote:runtime_server_outbox',0,1,'complete',1793452080.184,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:26:2026-10-28-receipts-remote:runtime_server_fetch',0,1,'complete',1793452080.184,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:26:2026-10-28-receipts-remote:runtime_federation_inbox',0,1,'complete',1793452080.184,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:26:2026-10-28-receipts-remote:runtime_federation_outbox',0,1,'complete',1793452080.184,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:33:2026-10-28-receipts-remote-finish:',0,1,'complete',1793452080.184,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:45:2026-10-28-receipts-provider-delivery-prepare:',0,1,'complete',1793452080.354,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:37:2026-10-28-receipts-provider-delivery:runtime_operation_receipts',0,1,'complete',1793452080.354,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:44:2026-10-28-receipts-provider-delivery-finish:',0,1,'complete',1793452080.354,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:46:2026-10-28-receipts-provider-callbacks-prepare:',0,1,'complete',1793452080.385,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:38:2026-10-28-receipts-provider-callbacks:callbacks',0,1,'complete',1793452080.385,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:38:2026-10-28-receipts-provider-callbacks:frames',0,1,'complete',1793452080.385,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:38:2026-10-28-receipts-provider-callbacks:frame-outcomes',0,1,'complete',1793452080.385,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:38:2026-10-28-receipts-provider-callbacks:unanswered',0,1,'complete',1793452080.385,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:45:2026-10-28-receipts-provider-callbacks-finish:',0,1,'complete',1793452080.385,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:39:2026-10-28-receipts-vm-provider-prepare:',0,1,'complete',1793452080.46,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:31:2026-10-28-receipts-vm-provider:runtime_operation_receipts',0,1,'complete',1793452080.46,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:38:2026-10-28-receipts-vm-provider-finish:',0,1,'complete',1793452080.46,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:42:2026-10-28-receipts-account-worker-prepare:',0,1,'complete',1793452080.501,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:34:2026-10-28-receipts-account-worker:journal',0,1,'complete',1793452080.501,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:34:2026-10-28-receipts-account-worker:state',0,1,'complete',1793452080.501,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:34:2026-10-28-receipts-account-worker:calls',0,1,'complete',1793452080.501,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:34:2026-10-28-receipts-account-worker:effects',0,1,'complete',1793452080.501,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:34:2026-10-28-receipts-account-worker:streams',0,1,'complete',1793452080.501,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:34:2026-10-28-receipts-account-worker:stream-pending',0,1,'complete',1793452080.501,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:34:2026-10-28-receipts-account-worker:stream-bases',0,1,'complete',1793452080.501,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:34:2026-10-28-receipts-account-worker:submission-inputs',14,1,'complete',1793452080.501,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:34:2026-10-28-receipts-account-worker:unmatched',0,1,'complete',1793452080.501,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:41:2026-10-28-receipts-account-worker-finish:',0,1,'complete',1793452080.501,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:37:2026-10-28-receipts-incidents-prepare:',0,1,'complete',1793452080.643,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:29:2026-10-28-receipts-incidents:runtime_operation_receipts',0,1,'complete',1793452080.643,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:36:2026-10-28-receipts-incidents-finish:',0,1,'complete',1793452080.643,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:34:2026-10-28-receipts-checks-prepare:',0,1,'complete',1793452080.68,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:26:2026-10-28-receipts-checks:runtime_events',3,1,'complete',1793452080.68,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:26:2026-10-28-receipts-checks:runtime_execution_effects',1,1,'complete',1793452080.68,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:26:2026-10-28-receipts-checks:runtime_payload_migrations',94,1,'complete',1793452080.68,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:33:2026-10-28-receipts-checks-finish:',0,1,'complete',1793452080.68,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:43:2026-10-28-receipts-drop-old-tables-prepare:',0,1,'complete',1793452080.873,NULL,1793451803162);
INSERT INTO runtime_payload_migrations VALUES('receipts:42:2026-10-28-receipts-drop-old-tables-finish:',0,1,'complete',1793452080.873,NULL,1793451803162);
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
INSERT INTO durable_view_log VALUES('agent','agent',561,0);
INSERT INTO durable_view_log VALUES('agent','other',3,0);
INSERT INTO durable_view_log VALUES('agent','child',4,0);
INSERT INTO durable_view_log VALUES('agent','gone',562,1);
INSERT INTO durable_view_log VALUES('work','work',564,0);
INSERT INTO durable_view_log VALUES('work','child-work',565,0);
CREATE TABLE chats(
            id TEXT PRIMARY KEY, state TEXT NOT NULL, status TEXT NOT NULL,
            auto_wake INTEGER, usage_resume_enabled INTEGER, execution_mode TEXT, turn_epoch INTEGER) STRICT;
INSERT INTO chats VALUES('agent','active','approval',1,NULL,NULL,NULL);
INSERT INTO chats VALUES('other','active','running',1,NULL,NULL,NULL);
INSERT INTO chats VALUES('child','active','running',1,NULL,NULL,NULL);
INSERT INTO chats VALUES('gone','active','approval',1,NULL,NULL,NULL);
CREATE TABLE sessions(
            id TEXT PRIMARY KEY, chat TEXT NOT NULL, thread_id TEXT NOT NULL,
            account_key TEXT NOT NULL, state TEXT NOT NULL) STRICT;
INSERT INTO sessions VALUES('agent:thread','agent','thread','default','ready');
INSERT INTO sessions VALUES('other:other-thread','other','other-thread','default','ready');
INSERT INTO sessions VALUES('child:child-thread','child','child-thread','default','ready');
INSERT INTO sessions VALUES('gone:thread','gone','thread','default','ready');
CREATE TABLE turns(
            id TEXT PRIMARY KEY, chat TEXT NOT NULL, state TEXT NOT NULL, session TEXT,
            provider_turn TEXT, attempt TEXT, interrupt_ticket TEXT,
            attempt_submitted INTEGER GENERATED ALWAYS AS (attempt->>'$.submitted') STORED,
            attempt_connection_generation INTEGER GENERATED ALWAYS AS (attempt->>'$.connectionGeneration') STORED,
            attempt_action TEXT GENERATED ALWAYS AS (attempt->>'$.action') STORED,
            attempt_active_at_reservation INTEGER GENERATED ALWAYS AS (attempt->>'$.activeAtReservation') STORED) STRICT;
INSERT INTO turns VALUES('agent:turn','agent','completed',NULL,'turn',NULL,NULL);
INSERT INTO turns VALUES('2ac526b7-9c81-42aa-982e-c53e661f4d7f','agent','running','agent:thread','turn',NULL,NULL);
INSERT INTO turns VALUES('c7de26ad-fcd5-4214-aea3-67ced3f5f04d','other','running','other:other-thread','turn',NULL,NULL);
INSERT INTO turns VALUES('3a6f0feb-4460-467d-9bd4-47500f14c72c','child','running','child:child-thread','turn',NULL,NULL);
INSERT INTO turns VALUES('c8f3d16a-703a-43ef-8423-ce4bdf7fe928','gone','running','gone:thread','turn',NULL,NULL);
CREATE TABLE IF NOT EXISTS sqlite_sequence(name,seq);
DELETE FROM sqlite_sequence;
INSERT INTO sqlite_sequence VALUES('runtime_chat_messages',5);
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
