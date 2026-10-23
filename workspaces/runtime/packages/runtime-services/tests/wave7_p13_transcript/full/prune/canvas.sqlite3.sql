BEGIN TRANSACTION;
CREATE TABLE runtime_account_transfers(id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_agent_moves(id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_agent_record_generation (
                    id INTEGER PRIMARY KEY CHECK(id=1), value INTEGER NOT NULL);
INSERT INTO "runtime_agent_record_generation" VALUES(1,12);
CREATE TABLE runtime_agents (id TEXT PRIMARY KEY, record TEXT NOT NULL);
INSERT INTO "runtime_agents" VALUES('agent','{"id": "agent", "rootId": "agent", "epoch": 1, "status": "approval", "cwd": "@FIXTURE_ROOT@", "threadId": "thread", "accountKey": "default", "turnId": "turn", "inFlight": true, "events": 7, "activeTools": [], "autoWake": true, "isLead": true, "name": "Transcript fixture", "agentMode": "multi", "agentModeRevision": 0, "agentModeSupported": true, "concurrency": 32, "maxAgentsExplicit": false, "subagentConcurrencyVersion": 2, "nativeResponseTurn": "turn", "lastEvent": "2026-10-22T17:20:38Z", "activity": {"phase": "writing", "at": 300.0}, "compactions": 1, "contextUsage": null, "tail": "Needle streamed answer"}');
INSERT INTO "runtime_agents" VALUES('other','{"id": "other", "rootId": "other", "epoch": 1, "status": "running", "cwd": "@FIXTURE_ROOT@", "threadId": "other-thread", "accountKey": "default", "turnId": "turn", "inFlight": true, "events": 0, "activeTools": [], "autoWake": true, "isLead": true, "name": "Transcript fixture", "agentMode": "multi", "agentModeRevision": 0, "agentModeSupported": true, "concurrency": 32, "maxAgentsExplicit": false, "subagentConcurrencyVersion": 2}');
INSERT INTO "runtime_agents" VALUES('child','{"id": "child", "rootId": "agent", "epoch": 1, "status": "running", "cwd": "@FIXTURE_ROOT@", "threadId": "child-thread", "accountKey": "default", "turnId": "turn", "inFlight": true, "events": 0, "activeTools": [], "autoWake": true, "isLead": false, "name": "Transcript fixture", "agentMode": "multi", "agentModeRevision": 0, "agentModeSupported": true, "concurrency": 32, "maxAgentsExplicit": false, "subagentConcurrencyVersion": 2, "parentId": "agent"}');
CREATE TABLE runtime_annotations (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_assets (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_budget (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_budget_usage (id TEXT PRIMARY KEY, agent TEXT NOT NULL, kind TEXT NOT NULL, tokens INTEGER NOT NULL, at REAL NOT NULL);
CREATE TABLE runtime_capacity_retries (id TEXT PRIMARY KEY, agent TEXT NOT NULL, record TEXT NOT NULL);
CREATE TABLE runtime_chat_messages (
                  seq INTEGER PRIMARY KEY AUTOINCREMENT, id TEXT UNIQUE NOT NULL,
                  room TEXT NOT NULL, sender TEXT NOT NULL, text TEXT NOT NULL,
                  created REAL NOT NULL, deliveries TEXT NOT NULL);
INSERT INTO "runtime_chat_messages" VALUES(1,'chat','private','child','needle room',210.0,'{}');
CREATE TABLE runtime_checkpoints (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_compactions (id TEXT PRIMARY KEY, agent TEXT NOT NULL);
INSERT INTO "runtime_compactions" VALUES('agent:compact','agent');
CREATE TABLE runtime_complaints (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_completed_turns (id TEXT PRIMARY KEY);
INSERT INTO "runtime_completed_turns" VALUES('agent:turn');
CREATE TABLE runtime_context_manifests (
                  agent TEXT NOT NULL, thread_id TEXT NOT NULL, compactions INTEGER NOT NULL,
                  sequence INTEGER NOT NULL, event_id TEXT NOT NULL, record TEXT NOT NULL,
                  PRIMARY KEY(agent,thread_id,compactions));
CREATE TABLE runtime_context_reminders (
                  agent TEXT NOT NULL, version TEXT NOT NULL, event_id TEXT NOT NULL,
                  PRIMARY KEY(agent,version));
CREATE TABLE runtime_event_meta (id TEXT PRIMARY KEY, record TEXT NOT NULL);
INSERT INTO "runtime_event_meta" VALUES('input','{"requestedDelivery": "steer", "transcriptItemId": "agent:input"}');
INSERT INTO "runtime_event_meta" VALUES('pending','{"delivery":"current_turn","timing":{"enqueuedAt":228874422279577,"fastSkipReason":"scheduleOverride"}}');
CREATE TABLE runtime_events (
                  id TEXT PRIMARY KEY, agent TEXT NOT NULL, kind TEXT NOT NULL,
                  text TEXT NOT NULL, status TEXT NOT NULL, created REAL NOT NULL,
                  epoch INTEGER NOT NULL, turn_id TEXT, error TEXT);
INSERT INTO "runtime_events" VALUES('input','agent','user','Needle submitted input','delivered',200.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('pending','agent','user','pending receipt needle','pending',500.0,1,NULL,NULL);
CREATE TABLE runtime_execution_attempts (id TEXT PRIMARY KEY, run TEXT NOT NULL, record TEXT NOT NULL);
CREATE TABLE runtime_execution_effects (id TEXT PRIMARY KEY, run TEXT NOT NULL, kind TEXT NOT NULL, reference TEXT NOT NULL, record TEXT NOT NULL);
INSERT INTO "runtime_execution_effects" VALUES('spawn:child','run:543b6d63-ba17-5996-aa5b-7bc7c6764369','spawn','child','{"id": "spawn:child", "runId": "run:543b6d63-ba17-5996-aa5b-7bc7c6764369", "kind": "spawn", "referenceId": "child", "attemptId": null, "status": "running"}');
CREATE TABLE runtime_execution_inputs (run TEXT NOT NULL, event TEXT NOT NULL, PRIMARY KEY(run,event));
CREATE TABLE runtime_execution_nodes (id TEXT PRIMARY KEY, run TEXT NOT NULL, record TEXT NOT NULL);
INSERT INTO "runtime_execution_nodes" VALUES('worker:child','run:543b6d63-ba17-5996-aa5b-7bc7c6764369','{"id": "worker:child", "runId": "run:543b6d63-ba17-5996-aa5b-7bc7c6764369", "kind": "managed_worker", "agentId": "child", "status": "running"}');
CREATE TABLE runtime_execution_runs (id TEXT PRIMARY KEY, agent TEXT NOT NULL, account TEXT NOT NULL, epoch INTEGER NOT NULL, thread TEXT, turn TEXT, created REAL NOT NULL, record TEXT NOT NULL);
INSERT INTO "runtime_execution_runs" VALUES('run:543b6d63-ba17-5996-aa5b-7bc7c6764369','agent','default',1,'thread','turn',1792689637.9987645,'{"id": "run:543b6d63-ba17-5996-aa5b-7bc7c6764369", "agent": "agent", "accountKey": "default", "epoch": 1, "threadId": "thread", "turnId": "turn", "created": 1792689637.9987645, "status": "running", "firstAttemptId": null, "rootAttemptId": null, "latestAttemptId": null, "reconciled": true}');
INSERT INTO "runtime_execution_runs" VALUES('run:48f065a1-8be0-543a-9b6f-69bc8581573d','other','default',1,'other-thread','turn',1792689637.9999597,'{"id": "run:48f065a1-8be0-543a-9b6f-69bc8581573d", "agent": "other", "accountKey": "default", "epoch": 1, "threadId": "other-thread", "turnId": "turn", "created": 1792689637.9999597, "status": "running", "firstAttemptId": null, "rootAttemptId": null, "latestAttemptId": null, "reconciled": true}');
INSERT INTO "runtime_execution_runs" VALUES('run:24b2429b-e38b-5348-a024-3759e25043e1','child','default',1,'child-thread','turn',1792689638.0005055,'{"id": "run:24b2429b-e38b-5348-a024-3759e25043e1", "agent": "child", "accountKey": "default", "epoch": 1, "threadId": "child-thread", "turnId": "turn", "created": 1792689638.0005054, "status": "running", "firstAttemptId": null, "rootAttemptId": null, "latestAttemptId": null, "reconciled": true}');
CREATE TABLE runtime_federation_identity (
          id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_federation_inbox (
          peer_id TEXT NOT NULL, message_id TEXT NOT NULL, record TEXT NOT NULL,
          PRIMARY KEY(peer_id,message_id));
CREATE TABLE runtime_federation_invites (
          id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_federation_nonces (
          peer_id TEXT NOT NULL, nonce TEXT NOT NULL, created INTEGER NOT NULL,
          PRIMARY KEY(peer_id,nonce));
CREATE TABLE runtime_federation_outbox (
          id TEXT PRIMARY KEY, peer_id TEXT NOT NULL, record TEXT NOT NULL);
CREATE TABLE runtime_federation_peers (
          id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_federation_rooms (
          id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_federation_settings (
          id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_item_fulltext (id TEXT PRIMARY KEY, body TEXT NOT NULL);
CREATE TABLE runtime_items (
                  id TEXT PRIMARY KEY, agent TEXT NOT NULL, record TEXT NOT NULL, created REAL NOT NULL);
INSERT INTO "runtime_items" VALUES('agent:old-1','agent','{"id": "agent:old-1", "role": "assistant", "title": "Agent", "text": "needle history 1", "truncated": false, "at": 101.0, "turnId": "old-turn"}',101.0);
INSERT INTO "runtime_items" VALUES('agent:old-2','agent','{"id": "agent:old-2", "role": "assistant", "title": "Agent", "text": "needle history 2", "truncated": false, "at": 102.0, "turnId": "old-turn"}',102.0);
INSERT INTO "runtime_items" VALUES('agent:old-3','agent','{"id": "agent:old-3", "role": "assistant", "title": "Agent", "text": "needle history 3", "truncated": false, "at": 103.0, "turnId": "old-turn"}',103.0);
INSERT INTO "runtime_items" VALUES('agent:old-4','agent','{"id": "agent:old-4", "role": "assistant", "title": "Agent", "text": "needle history 4", "truncated": false, "at": 104.0, "turnId": "old-turn"}',104.0);
INSERT INTO "runtime_items" VALUES('agent:old-5','agent','{"id": "agent:old-5", "role": "assistant", "title": "Agent", "text": "needle history 5", "truncated": false, "at": 105.0, "turnId": "old-turn"}',105.0);
INSERT INTO "runtime_items" VALUES('agent:old-6','agent','{"id": "agent:old-6", "role": "assistant", "title": "Agent", "text": "needle history 6", "truncated": false, "at": 106.0, "turnId": "old-turn"}',106.0);
INSERT INTO "runtime_items" VALUES('agent:old-7','agent','{"id": "agent:old-7", "role": "assistant", "title": "Agent", "text": "needle history 7", "truncated": false, "at": 107.0, "turnId": "old-turn"}',107.0);
INSERT INTO "runtime_items" VALUES('other:other','other','{"id": "other:other", "role": "assistant", "title": "Agent", "text": "needle foreign", "truncated": false, "at": 1792689638.0096738}',1792689638.0096784);
INSERT INTO "runtime_items" VALUES('agent:input','agent','{"id": "agent:input", "role": "user", "title": "You", "text": "Needle submitted input", "truncated": false, "at": 200.0, "turnId": "turn", "inputs": [{"id": "input", "at": 200.0, "kind": "user", "text": "Needle submitted input", "truncated": false, "assets": []}]}',200.0);
INSERT INTO "runtime_items" VALUES('agent:answer','agent','{"id": "agent:answer", "role": "assistant", "title": "Assistant", "text": "Needle updated answer", "truncated": false, "at": 300.0, "streaming": true, "turnId": "turn", "phase": null}',300.0);
INSERT INTO "runtime_items" VALUES('agent:command','agent','{"id": "agent:command", "role": "output", "title": "commandExecution", "text": "{\"id\": \"command\", \"type\": \"commandExecution\", \"exitCode\": 0, \"aggregatedOutput\": \"needle tool output\", \"outputTruncated\": false, \"durationMs\": 0}", "truncated": false, "at": 300.0, "toolStatus": "completed", "turnId": "turn"}',300.0);
INSERT INTO "runtime_items" VALUES('agent:compact','agent','{"id": "agent:compact", "role": "output", "title": "contextCompaction", "text": "{\"id\": \"compact\", \"type\": \"contextCompaction\", \"durationMs\": 0}", "truncated": false, "at": 300.0, "toolStatus": "completed", "turnId": "turn"}',300.0);
CREATE TABLE runtime_lead_requests (id TEXT PRIMARY KEY, agent TEXT NOT NULL, signature TEXT NOT NULL);
CREATE TABLE runtime_monitors (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_native_sweeps (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_operation_receipts (id TEXT PRIMARY KEY, signature TEXT NOT NULL, result TEXT NOT NULL);
CREATE TABLE runtime_payload_refs (
            table_name TEXT NOT NULL, record_id TEXT NOT NULL, field TEXT NOT NULL,
            sha256 TEXT NOT NULL, PRIMARY KEY(table_name,record_id,field));
CREATE TABLE runtime_plans (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_profiles (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_projects (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_requests (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_rooms (id TEXT PRIMARY KEY, record TEXT NOT NULL);
INSERT INTO "runtime_rooms" VALUES('private','{"id": "private", "kind": "private", "rootId": "agent", "members": ["agent", "child"], "updated": 1.0}');
CREATE TABLE runtime_rules (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_scheduler_agent_change_state (id INTEGER PRIMARY KEY CHECK(id=1),floor INTEGER NOT NULL,generation INTEGER NOT NULL,entries INTEGER NOT NULL,total INTEGER NOT NULL);
INSERT INTO "runtime_scheduler_agent_change_state" VALUES(1,0,12,3,17);
CREATE TABLE runtime_scheduler_agent_changes (id TEXT PRIMARY KEY,generation INTEGER NOT NULL CHECK(typeof(generation)='integer' AND generation>=0));
INSERT INTO "runtime_scheduler_agent_changes" VALUES('agent',12);
INSERT INTO "runtime_scheduler_agent_changes" VALUES('other',2);
INSERT INTO "runtime_scheduler_agent_changes" VALUES('child',3);
PRAGMA writable_schema=ON;
INSERT INTO sqlite_master(type,name,tbl_name,rootpage,sql)VALUES('table','runtime_search_next','runtime_search_next',0,'CREATE VIRTUAL TABLE runtime_search_next USING fts5(body,content='''',contentless_delete=1,tokenize=''unicode61'')');
CREATE TABLE 'runtime_search_next_config'(k PRIMARY KEY, v) WITHOUT ROWID;
INSERT INTO "runtime_search_next_config" VALUES('version',4);
CREATE TABLE 'runtime_search_next_data'(id INTEGER PRIMARY KEY, block BLOB);
INSERT INTO "runtime_search_next_data" VALUES(1,X'0C31');
INSERT INTO "runtime_search_next_data" VALUES(10,X'00000000FF000001010D0F000D0101010101000001020101020200000103010103030000010401010404000001050101050500000106010106060000010701010707000001080101080800000109010109090000010A01010A0A0000010B01010B0B0102010C01010C0C0000010D01010D0D000001');
INSERT INTO "runtime_search_next_data" VALUES(137438953473,X'000000210230300102040107686973746F727901020301066E6565646C6501020204060C');
INSERT INTO "runtime_search_next_data" VALUES(274877906945,X'000000210230310202040107686973746F727902020301066E6565646C6502020204060C');
INSERT INTO "runtime_search_next_data" VALUES(412316860417,X'000000210230320302040107686973746F727903020301066E6565646C6503020204060C');
INSERT INTO "runtime_search_next_data" VALUES(549755813889,X'000000210230330402040107686973746F727904020301066E6565646C6504020204060C');
INSERT INTO "runtime_search_next_data" VALUES(687194767361,X'000000210230340502040107686973746F727905020301066E6565646C6505020204060C');
INSERT INTO "runtime_search_next_data" VALUES(824633720833,X'000000210230350602040107686973746F727906020301066E6565646C6506020204060C');
INSERT INTO "runtime_search_next_data" VALUES(962072674305,X'000000210230360702040107686973746F727907020301066E6565646C6507020204060C');
INSERT INTO "runtime_search_next_data" VALUES(1099511627777,X'000000210230370802040107686973746F727908020301066E6565646C6508020204060C');
INSERT INTO "runtime_search_next_data" VALUES(1236950581249,X'0000001B0830666F726569676E09020301066E6565646C65090202040C');
INSERT INTO "runtime_search_next_data" VALUES(1374389534721,X'000000270630696E7075740A020401066E6565646C650A020201097375626D69747465640A0203040A0B');
INSERT INTO "runtime_search_next_data" VALUES(1511828488193,X'00000058033034320C020A0107636F6D6D616E640C0403050809657865637574696F6E0C0205010269640C020201066E6565646C650C020801067072696E74660C020703076F6365737369640C02090104747970650C020404070D0E070B0B0C');
INSERT INTO "runtime_search_next_data" VALUES(1649267441665,X'0000009C0230300C04070A0110616767726567617465646F75747075740C02080107636F6D6D616E640C02030809657865637574696F6E0C0205010A6475726174696F6E6D730C020E010865786974636F64650C0206010566616C73650C020D010269640C020201066E6565646C650C020901066F75747075740C020B07097472756E63617465640C020C0104746F6F6C0C020A02037970650C02040407150C0E0F0D0A070B0B0E09');
INSERT INTO "runtime_search_next_data" VALUES(1786706395137,X'000000490230300D02070107636F6D706163740D0203030F6E74657874636F6D70616374696F6E0D0205010A6475726174696F6E6D730D0206010269640D02020104747970650D020404060C140F07');
INSERT INTO "runtime_search_next_data" VALUES(9008711083229184,X'040000000000000200000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000B0000000C00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000');
CREATE TABLE 'runtime_search_next_docsize'(id INTEGER PRIMARY KEY, sz BLOB, origin INTEGER);
INSERT INTO "runtime_search_next_docsize" VALUES(1,X'03',1);
INSERT INTO "runtime_search_next_docsize" VALUES(2,X'03',2);
INSERT INTO "runtime_search_next_docsize" VALUES(3,X'03',3);
INSERT INTO "runtime_search_next_docsize" VALUES(4,X'03',4);
INSERT INTO "runtime_search_next_docsize" VALUES(5,X'03',5);
INSERT INTO "runtime_search_next_docsize" VALUES(6,X'03',6);
INSERT INTO "runtime_search_next_docsize" VALUES(7,X'03',7);
INSERT INTO "runtime_search_next_docsize" VALUES(8,X'03',8);
INSERT INTO "runtime_search_next_docsize" VALUES(9,X'02',9);
INSERT INTO "runtime_search_next_docsize" VALUES(10,X'03',10);
INSERT INTO "runtime_search_next_docsize" VALUES(12,X'0E',12);
INSERT INTO "runtime_search_next_docsize" VALUES(13,X'06',13);
CREATE TABLE 'runtime_search_next_idx'(segid, term, pgno, PRIMARY KEY(segid, term)) WITHOUT ROWID;
INSERT INTO "runtime_search_next_idx" VALUES(1,X'',2);
INSERT INTO "runtime_search_next_idx" VALUES(2,X'',2);
INSERT INTO "runtime_search_next_idx" VALUES(3,X'',2);
INSERT INTO "runtime_search_next_idx" VALUES(4,X'',2);
INSERT INTO "runtime_search_next_idx" VALUES(5,X'',2);
INSERT INTO "runtime_search_next_idx" VALUES(6,X'',2);
INSERT INTO "runtime_search_next_idx" VALUES(7,X'',2);
INSERT INTO "runtime_search_next_idx" VALUES(8,X'',2);
INSERT INTO "runtime_search_next_idx" VALUES(9,X'',2);
INSERT INTO "runtime_search_next_idx" VALUES(10,X'',2);
INSERT INTO "runtime_search_next_idx" VALUES(11,X'',2);
INSERT INTO "runtime_search_next_idx" VALUES(12,X'',2);
INSERT INTO "runtime_search_next_idx" VALUES(13,X'',2);
CREATE TABLE runtime_search_next_meta (
                id TEXT PRIMARY KEY, agent TEXT NOT NULL, kind TEXT NOT NULL,
                search_rowid INTEGER NOT NULL UNIQUE);
INSERT INTO "runtime_search_next_meta" VALUES('agent:old-0','agent','Agent',1);
INSERT INTO "runtime_search_next_meta" VALUES('agent:old-1','agent','Agent',2);
INSERT INTO "runtime_search_next_meta" VALUES('agent:old-2','agent','Agent',3);
INSERT INTO "runtime_search_next_meta" VALUES('agent:old-3','agent','Agent',4);
INSERT INTO "runtime_search_next_meta" VALUES('agent:old-4','agent','Agent',5);
INSERT INTO "runtime_search_next_meta" VALUES('agent:old-5','agent','Agent',6);
INSERT INTO "runtime_search_next_meta" VALUES('agent:old-6','agent','Agent',7);
INSERT INTO "runtime_search_next_meta" VALUES('agent:old-7','agent','Agent',8);
INSERT INTO "runtime_search_next_meta" VALUES('other:other','other','Agent',9);
INSERT INTO "runtime_search_next_meta" VALUES('agent:input','agent','You',10);
INSERT INTO "runtime_search_next_meta" VALUES('agent:command','agent','commandExecution',12);
INSERT INTO "runtime_search_next_meta" VALUES('agent:compact','agent','contextCompaction',13);
CREATE TABLE runtime_search_rollout (
                id INTEGER PRIMARY KEY CHECK(id=1), phase TEXT NOT NULL,
                cursor INTEGER NOT NULL DEFAULT 0, updated REAL NOT NULL);
INSERT INTO "runtime_search_rollout" VALUES(1,'complete',0,1792689637.8706896);
CREATE TABLE runtime_server_fetch (
                    id TEXT PRIMARY KEY, server TEXT NOT NULL, signature TEXT NOT NULL,
                    state TEXT NOT NULL, result TEXT);
CREATE TABLE runtime_server_inbox (
                    id TEXT PRIMARY KEY, signature TEXT NOT NULL, state TEXT NOT NULL,
                    result TEXT, created_at REAL NOT NULL DEFAULT 0, completed_at REAL NOT NULL DEFAULT 0, principal TEXT, actor TEXT);
CREATE TABLE runtime_server_links (
                    id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_server_outbox (
                    id TEXT PRIMARY KEY, server TEXT NOT NULL, body TEXT NOT NULL,
                    state TEXT NOT NULL, result TEXT, attempts INTEGER NOT NULL DEFAULT 0,
                    next_at REAL NOT NULL DEFAULT 0, error TEXT, created_at REAL NOT NULL DEFAULT 0, completed_at REAL NOT NULL DEFAULT 0, fingerprint TEXT);
CREATE TABLE runtime_server_output_tools (
                    id TEXT PRIMARY KEY, expires REAL NOT NULL);
CREATE TABLE runtime_server_retired (
                    id TEXT NOT NULL, direction TEXT NOT NULL, signature TEXT NOT NULL,
                    server TEXT, actor TEXT, PRIMARY KEY(id,direction));
CREATE TABLE runtime_server_sequence (
                    id INTEGER PRIMARY KEY CHECK(id=1), value INTEGER NOT NULL);
INSERT INTO "runtime_server_sequence" VALUES(1,0);
CREATE TABLE runtime_sidebar_order (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_supervisor_cursor (handle TEXT PRIMARY KEY, sequence INTEGER NOT NULL, epoch TEXT);
CREATE TABLE runtime_tasks (id TEXT PRIMARY KEY, record TEXT NOT NULL);
INSERT INTO "runtime_tasks" VALUES('agent:command','{"id": "agent:command", "agent": "agent", "itemId": "command", "turnId": "turn", "created": 300.0, "kind": "command", "ownerEpoch": 1, "sourceAccountKey": "default", "sourceConnectionId": "connection-7", "sourceThreadId": "thread", "sourceProvider": "codex", "type": "commandExecution", "name": "commandExecution", "status": "completed", "command": "printf needle", "processId": "42", "tail": "needle tool output", "outputTruncated": false, "exitCode": 0, "durationMs": 0, "finished": 300.0}');
INSERT INTO "runtime_tasks" VALUES('agent:compact','{"id": "agent:compact", "agent": "agent", "itemId": "compact", "turnId": "turn", "created": 300.0, "kind": "tool", "ownerEpoch": 1, "sourceAccountKey": "default", "sourceConnectionId": "connection-7", "sourceThreadId": "thread", "sourceProvider": "codex", "type": "contextCompaction", "name": "contextCompaction", "status": "completed", "durationMs": 0, "finished": 300.0}');
CREATE TABLE runtime_tool_request_aliases
                (agent TEXT NOT NULL, alias TEXT NOT NULL, request TEXT NOT NULL, PRIMARY KEY(agent,alias,request));
CREATE TABLE runtime_tool_requests (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_tool_results (id TEXT PRIMARY KEY, result TEXT NOT NULL);
CREATE TABLE runtime_transcript_revisions(
                        agent TEXT PRIMARY KEY, revision INTEGER NOT NULL DEFAULT 0);
INSERT INTO "runtime_transcript_revisions" VALUES('agent',37);
INSERT INTO "runtime_transcript_revisions" VALUES('other',2);
INSERT INTO "runtime_transcript_revisions" VALUES('child',1);
CREATE TABLE runtime_turn_item_link_state_v1 (id INTEGER PRIMARY KEY CHECK(id=1),epoch TEXT NOT NULL,source_rootpage INTEGER NOT NULL,cursor INTEGER,target INTEGER,complete INTEGER NOT NULL CHECK(complete IN (0,1)),scanned_rows INTEGER NOT NULL,source_bytes INTEGER NOT NULL,updated REAL NOT NULL);
INSERT INTO "runtime_turn_item_link_state_v1" VALUES(1,'814eff3c807b4a799cc3f63d64d7d0ce',73,NULL,NULL,1,0,0,1792689637.776191);
CREATE TABLE runtime_turn_item_links_v1 (id TEXT PRIMARY KEY,agent TEXT NOT NULL,turn_id,created REAL NOT NULL,epoch TEXT NOT NULL);
INSERT INTO "runtime_turn_item_links_v1" VALUES('agent:old-1','agent','old-turn',101.0,'814eff3c807b4a799cc3f63d64d7d0ce');
INSERT INTO "runtime_turn_item_links_v1" VALUES('agent:old-2','agent','old-turn',102.0,'814eff3c807b4a799cc3f63d64d7d0ce');
INSERT INTO "runtime_turn_item_links_v1" VALUES('agent:old-3','agent','old-turn',103.0,'814eff3c807b4a799cc3f63d64d7d0ce');
INSERT INTO "runtime_turn_item_links_v1" VALUES('agent:old-4','agent','old-turn',104.0,'814eff3c807b4a799cc3f63d64d7d0ce');
INSERT INTO "runtime_turn_item_links_v1" VALUES('agent:old-5','agent','old-turn',105.0,'814eff3c807b4a799cc3f63d64d7d0ce');
INSERT INTO "runtime_turn_item_links_v1" VALUES('agent:old-6','agent','old-turn',106.0,'814eff3c807b4a799cc3f63d64d7d0ce');
INSERT INTO "runtime_turn_item_links_v1" VALUES('agent:old-7','agent','old-turn',107.0,'814eff3c807b4a799cc3f63d64d7d0ce');
INSERT INTO "runtime_turn_item_links_v1" VALUES('other:other','other',NULL,1792689638.0096784,'814eff3c807b4a799cc3f63d64d7d0ce');
INSERT INTO "runtime_turn_item_links_v1" VALUES('agent:input','agent','turn',200.0,'814eff3c807b4a799cc3f63d64d7d0ce');
INSERT INTO "runtime_turn_item_links_v1" VALUES('agent:answer','agent','turn',300.0,'814eff3c807b4a799cc3f63d64d7d0ce');
INSERT INTO "runtime_turn_item_links_v1" VALUES('agent:command','agent','turn',300.0,'814eff3c807b4a799cc3f63d64d7d0ce');
INSERT INTO "runtime_turn_item_links_v1" VALUES('agent:compact','agent','turn',300.0,'814eff3c807b4a799cc3f63d64d7d0ce');
CREATE TABLE runtime_usage_resumes (id TEXT PRIMARY KEY, agent TEXT NOT NULL, record TEXT NOT NULL);
CREATE TABLE runtime_work (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_workspace_operations (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE sync_documents (
        seq INTEGER PRIMARY KEY AUTOINCREMENT, scope TEXT NOT NULL,
        id TEXT NOT NULL, payload TEXT NOT NULL, deleted INTEGER NOT NULL DEFAULT 0,
        UNIQUE(scope,id));
CREATE TABLE sync_entities (
        collection TEXT NOT NULL, id TEXT NOT NULL, seq INTEGER NOT NULL,
        hash TEXT NOT NULL, payload TEXT, deleted INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY(collection,id));
INSERT INTO "sync_entities" VALUES('workspace','current',1,'5f0bc1ead2a9c2e010e53e0399e775e92eecc591bba396f2a69721886a312c65','{"collection":"workspace","id":"current","value":{"connected":false,"nativeNotices":[],"peerTeamsVersion":1,"projectOrganizationVersion":1,"rateLimits":{"accountKey":"default","at":null,"data":null,"error":null},"rateLimitsByAccount":{},"sidebarOrder":{"groups":null,"revision":0},"stateDir":"@FIXTURE_ROOT@/python","tasksHistoryLimit":100}}',0);
INSERT INTO "sync_entities" VALUES('agent','agent',19,'5676ce0ad0bcf8185bfc90206ec93ac5670f6aeacd5fa05dff1bf8f797325a4b','{"collection":"agent","id":"agent","value":{"accountKey":"default","activity":{"at":300.0,"phase":"writing"},"agentMode":"multi","agentModeRevision":0,"agentModeSupported":true,"autoWake":true,"canSend":true,"compactions":1,"concurrency":32,"contextUsage":null,"cwd":"@FIXTURE_ROOT@","empty":false,"epoch":1,"id":"agent","inFlight":true,"isLead":true,"kind":"agent","lastEvent":"2026-10-22T17:20:38Z","launcherAlive":true,"name":"Transcript fixture","nextTurnSettingsSupported":true,"readStateSupported":true,"rootId":"agent","serverId":"fixture-server","source":"managed","status":"approval","subagentConcurrencyVersion":2,"tail":"Needle streamed answer","threadId":"thread","turnId":"turn"}}',0);
INSERT INTO "sync_entities" VALUES('agent','other',3,'b96aaa3e57c6560b3dc97745e2a439b5028a3a1911b0a5b51a0201aee563c350','{"collection":"agent","id":"other","value":{"accountKey":"default","agentMode":"multi","agentModeRevision":0,"agentModeSupported":true,"autoWake":true,"canSend":true,"concurrency":32,"cwd":"@FIXTURE_ROOT@","empty":false,"epoch":1,"id":"other","inFlight":true,"isLead":true,"kind":"agent","launcherAlive":true,"name":"Transcript fixture","nextTurnSettingsSupported":true,"readStateSupported":true,"rootId":"other","serverId":"fixture-server","source":"managed","status":"running","subagentConcurrencyVersion":2,"threadId":"other-thread","turnId":"turn"}}',0);
INSERT INTO "sync_entities" VALUES('agent','child',4,'9e9c192a7b84f0298b5ba5436c9928cec6da36e65a008ebe3c94543ce9f2f1fd','{"collection":"agent","id":"child","value":{"accountKey":"default","agentMode":"multi","agentModeRevision":0,"agentModeSupported":true,"autoWake":true,"canSend":true,"concurrency":32,"cwd":"@FIXTURE_ROOT@","epoch":1,"id":"child","inFlight":true,"isLead":false,"kind":"agent","launcherAlive":true,"name":"Transcript fixture","nextTurnSettingsSupported":true,"overview":{"result":"","resultFile":null,"resultTruncated":false,"resultTurnId":null,"task":"","taskTruncated":false},"parentId":"agent","readStateSupported":true,"rootId":"agent","serverId":"fixture-server","source":"managed","status":"running","subagentConcurrencyVersion":2,"threadId":"child-thread","turnId":"turn"}}',0);
INSERT INTO "sync_entities" VALUES('event','input',25,'bff206d03823a5b769e63060cb0763453b2b719399478130fd16096b8e289f95','{"collection":"event","id":"input","value":{"agent":"agent","created":200.0,"error":null,"id":"input","kind":"user","status":"delivered"}}',0);
INSERT INTO "sync_entities" VALUES('room','private',7,'53df58b94e00bc7d1706ac8d79cae6fecdbce6e7f494160687490829200aa899','{"collection":"room","id":"private","value":{"id":"private","kind":"private","lastMessage":null,"members":["agent","child"],"name":"Transcript fixture ↔ Transcript fixture","rootId":"agent","updated":1.0}}',0);
INSERT INTO "sync_entities" VALUES('task','agent:command',12,'2af7a8c98cc5344143fa75ebea6446595058e8a29e9cd224fd9f23aada6ff6a3','{"collection":"task","id":"agent:command","value":{"agent":"agent","command":"printf needle","created":300.0,"durationMs":0,"exitCode":0,"finished":300.0,"id":"agent:command","kind":"command","name":"commandExecution","outputTruncated":false,"processId":"42","status":"completed","turnId":"turn"}}',0);
INSERT INTO "sync_entities" VALUES('task','agent:compact',14,'aafd76c2984f447747e28ce0d00ca6c04a0704bb86cfcddcc757ec57ec637f30','{"collection":"task","id":"agent:compact","value":{"agent":"agent","created":300.0,"durationMs":0,"finished":300.0,"id":"agent:compact","kind":"tool","name":"contextCompaction","status":"completed","turnId":"turn"}}',0);
INSERT INTO "sync_entities" VALUES('event','pending',27,'07a0ed3c4c70bbfacb3e1953bf1364f9b60cdfd73f772a69e0eb77ebe3c50869','{"collection":"event","id":"pending","value":{"agent":"agent","created":500.0,"error":null,"id":"pending","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:agent:old-0',45,'2fb9ed07d959ca4ff60d9e1841376ff2ddbcc0321fd3ae377b15b86dc43cbd72',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:agent:old-1',29,'d7691a12cf5a07d416f04bad6e765e8a9726d49000c79aab7c483d81256f46d6',NULL,0);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:agent:old-2',30,'c6f47674d75b4a9be1c2234aeb869c7300bf7ea8d24af5679a2d47a50feadef2',NULL,0);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:agent:old-3',31,'13a8de652b968460e7ee53d1bd87a3c9b6e8e8d17a99feeb3c84c4c92675dc6f',NULL,0);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:agent:old-4',32,'c0c268158ed5bbc9652588fb95795c56bc476faa0e54e8e1a4c890058bd156b1',NULL,0);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:agent:old-5',33,'6a03dc6fa2c5f8b554893687e15bb68f459854ec47db888dcb304fbbaaac0e7f',NULL,0);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:agent:old-6',34,'a0ab283be23e339f7b118c7b9834b23e6ccbec768576a1fae535c24d05ccc9e1',NULL,0);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:agent:old-7',35,'78ab4a25d68ea456e7802cb6d8f6fa7e9add2647da7b6a8402359d9c1b4984e0',NULL,0);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:agent:input',36,'a017bf6c56f7d24d197ca72e5217e9f657223699d313f6fdf13b2ef2ef6bdfce',NULL,0);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:agent:answer',44,'e9e5be0bdc89f23fd49fc80687728696f07559b94e992cb11fbd6416925606f0',NULL,0);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:agent:command',38,'0104d41b94f4c9e0fcdcb71623b0413d7158539c887a4e328a5a069de44c838f',NULL,0);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:agent:compact',39,'383fa2e16eb15f5801958fb85e6e9bd57a9e463806c601d818b834f9253c2541',NULL,0);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:agent:pending',40,'55465de0e6f5ee108b1173ea590f1b600647b1f0c932b55a1f9ab16ec8ecc634',NULL,0);
INSERT INTO "sync_entities" VALUES('transcript:agent','@order',46,'ffa8eda309acc4023f59146efb5a0e02da71bbae602d1cd17f7b26f888c802d5',NULL,0);
INSERT INTO "sync_entities" VALUES('transcript:agent','@meta',42,'2dff93b54c1092bc22009f56070df43e2c74db861e6097b42f8d15bd35907cb0',NULL,0);
INSERT INTO "sync_entities" VALUES('transcript:agent','@revision',47,'7a61b53701befdae0eeeffaecc73f14e20b537bb0f8b91ad7c2936dc63562b25',NULL,0);
INSERT INTO "sync_entities" VALUES('transcript:agent','@floor',43,'44cb730c420480a0477b505ae68af508fb90f96cf0ec54c6ad16949dd427f13a',NULL,0);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-0',48,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-1',49,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-2',50,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-3',51,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-4',52,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-5',53,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-6',54,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-7',55,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-8',56,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-9',57,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-10',58,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-11',59,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-12',60,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-13',61,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-14',62,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-15',63,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-16',64,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-17',65,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-18',66,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-19',67,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-20',68,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-21',69,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-22',70,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-23',71,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-24',72,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-25',73,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-26',74,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-27',75,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-28',76,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-29',77,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-30',78,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-31',79,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-32',80,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-33',81,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-34',82,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-35',83,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-36',84,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-37',85,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-38',86,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-39',87,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-40',88,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-41',89,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-42',90,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-43',91,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-44',92,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-45',93,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-46',94,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-47',95,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-48',96,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-49',97,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-50',98,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-51',99,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-52',100,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-53',101,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-54',102,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-55',103,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-56',104,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-57',105,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-58',106,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-59',107,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-60',108,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-61',109,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-62',110,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-63',111,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-64',112,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-65',113,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-66',114,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-67',115,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-68',116,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-69',117,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-70',118,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-71',119,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-72',120,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-73',121,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-74',122,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-75',123,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-76',124,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-77',125,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-78',126,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-79',127,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-80',128,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-81',129,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-82',130,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-83',131,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-84',132,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-85',133,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-86',134,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-87',135,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-88',136,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-89',137,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-90',138,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-91',139,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-92',140,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-93',141,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-94',142,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-95',143,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-96',144,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-97',145,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-98',146,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-99',147,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-100',148,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-101',149,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-102',150,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-103',151,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-104',152,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-105',153,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-106',154,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-107',155,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-108',156,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-109',157,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-110',158,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-111',159,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-112',160,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-113',161,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-114',162,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-115',163,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-116',164,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-117',165,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-118',166,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-119',167,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-120',168,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-121',169,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-122',170,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-123',171,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-124',172,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-125',173,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-126',174,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-127',175,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-128',176,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-129',177,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-130',178,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-131',179,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-132',180,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-133',181,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-134',182,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-135',183,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-136',184,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-137',185,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-138',186,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-139',187,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-140',188,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-141',189,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-142',190,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-143',191,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-144',192,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-145',193,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-146',194,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-147',195,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-148',196,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-149',197,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-150',198,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-151',199,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-152',200,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-153',201,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-154',202,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-155',203,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-156',204,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-157',205,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-158',206,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-159',207,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-160',208,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-161',209,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-162',210,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-163',211,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-164',212,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-165',213,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-166',214,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-167',215,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-168',216,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-169',217,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-170',218,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-171',219,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-172',220,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-173',221,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-174',222,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-175',223,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-176',224,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-177',225,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-178',226,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-179',227,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-180',228,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-181',229,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-182',230,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-183',231,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-184',232,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-185',233,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-186',234,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-187',235,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-188',236,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-189',237,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-190',238,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-191',239,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-192',240,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-193',241,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-194',242,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-195',243,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-196',244,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-197',245,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-198',246,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-199',247,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-200',248,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-201',249,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-202',250,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-203',251,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-204',252,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-205',253,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-206',254,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-207',255,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-208',256,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-209',257,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-210',258,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-211',259,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-212',260,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-213',261,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-214',262,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-215',263,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-216',264,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-217',265,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-218',266,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-219',267,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-220',268,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-221',269,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-222',270,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-223',271,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-224',272,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-225',273,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-226',274,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-227',275,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-228',276,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-229',277,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-230',278,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-231',279,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-232',280,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-233',281,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-234',282,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-235',283,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-236',284,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-237',285,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-238',286,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-239',287,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-240',288,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-241',289,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-242',290,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-243',291,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-244',292,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-245',293,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-246',294,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-247',295,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-248',296,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-249',297,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-250',298,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-251',299,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-252',300,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-253',301,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-254',302,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-255',303,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-256',304,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-257',305,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-258',306,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-259',307,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-260',308,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-261',309,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-262',310,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-263',311,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-264',312,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-265',313,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-266',314,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-267',315,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-268',316,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-269',317,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-270',318,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-271',319,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-272',320,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-273',321,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-274',322,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-275',323,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-276',324,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-277',325,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-278',326,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-279',327,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-280',328,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-281',329,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-282',330,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-283',331,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-284',332,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-285',333,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-286',334,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-287',335,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-288',336,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-289',337,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-290',338,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-291',339,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-292',340,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-293',341,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-294',342,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-295',343,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-296',344,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-297',345,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-298',346,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-299',347,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-300',348,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-301',349,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-302',350,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-303',351,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-304',352,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-305',353,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-306',354,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-307',355,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-308',356,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-309',357,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-310',358,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-311',359,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-312',360,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-313',361,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-314',362,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-315',363,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-316',364,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-317',365,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-318',366,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-319',367,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-320',368,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-321',369,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-322',370,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-323',371,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-324',372,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-325',373,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-326',374,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-327',375,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-328',376,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-329',377,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-330',378,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-331',379,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-332',380,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-333',381,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-334',382,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-335',383,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-336',384,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-337',385,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-338',386,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-339',387,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-340',388,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-341',389,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-342',390,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-343',391,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-344',392,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-345',393,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-346',394,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-347',395,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-348',396,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-349',397,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-350',398,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-351',399,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-352',400,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-353',401,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-354',402,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-355',403,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-356',404,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-357',405,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-358',406,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-359',407,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-360',408,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-361',409,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-362',410,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-363',411,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-364',412,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-365',413,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-366',414,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-367',415,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-368',416,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-369',417,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-370',418,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-371',419,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-372',420,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-373',421,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-374',422,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-375',423,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-376',424,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-377',425,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-378',426,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-379',427,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-380',428,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-381',429,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-382',430,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-383',431,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-384',432,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-385',433,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-386',434,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-387',435,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-388',436,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-389',437,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-390',438,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-391',439,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-392',440,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-393',441,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-394',442,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-395',443,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-396',444,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-397',445,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-398',446,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-399',447,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-400',448,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-401',449,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-402',450,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-403',451,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-404',452,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-405',453,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-406',454,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-407',455,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-408',456,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-409',457,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-410',458,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-411',459,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-412',460,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-413',461,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-414',462,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-415',463,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-416',464,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-417',465,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-418',466,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-419',467,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-420',468,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-421',469,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-422',470,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-423',471,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-424',472,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-425',473,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-426',474,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-427',475,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-428',476,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-429',477,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-430',478,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-431',479,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-432',480,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-433',481,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-434',482,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-435',483,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-436',484,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-437',485,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-438',486,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-439',487,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-440',488,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-441',489,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-442',490,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-443',491,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-444',492,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-445',493,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-446',494,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-447',495,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-448',496,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-449',497,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-450',498,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-451',499,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-452',500,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-453',501,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-454',502,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-455',503,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-456',504,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-457',505,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-458',506,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-459',507,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-460',508,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-461',509,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-462',510,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-463',511,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-464',512,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-465',513,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-466',514,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-467',515,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-468',516,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-469',517,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-470',518,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-471',519,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-472',520,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-473',521,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-474',522,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-475',523,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-476',524,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-477',525,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-478',526,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-479',527,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-480',528,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-481',529,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-482',530,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-483',531,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-484',532,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-485',533,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-486',534,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-487',535,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-488',536,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-489',537,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-490',538,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-491',539,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-492',540,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-493',541,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-494',542,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-495',543,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-496',544,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-497',545,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-498',546,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-499',547,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-500',548,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-501',549,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-502',550,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-503',551,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-504',552,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-505',553,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-506',554,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-507',555,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-508',556,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-509',557,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-510',558,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-511',559,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-512',560,'stale',NULL,1);
CREATE TABLE sync_entity_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
INSERT INTO "sync_entity_meta" VALUES('entity_tombstone_count','0');
INSERT INTO "sync_entity_meta" VALUES('task_window_migrated','1');
INSERT INTO "sync_entity_meta" VALUES('event_window_seq','27');
INSERT INTO "sync_entity_meta" VALUES('seeded','1');
INSERT INTO "sync_entity_meta" VALUES('agent_organization_fields','2');
CREATE TABLE sync_generation (id INTEGER PRIMARY KEY, value INTEGER NOT NULL);
INSERT INTO "sync_generation" VALUES(1,0);
CREATE TABLE sync_identity (id TEXT PRIMARY KEY);
INSERT INTO "sync_identity" VALUES('6f99fae733fe413c97db00d16663fa9d');
CREATE TABLE sync_versions (
        seq INTEGER PRIMARY KEY, scope TEXT NOT NULL UNIQUE,
        hash TEXT NOT NULL, deleted INTEGER NOT NULL, updated REAL NOT NULL);
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
CREATE INDEX runtime_server_outbox_due
                    ON runtime_server_outbox(state,next_at);
CREATE INDEX runtime_server_links_home_parent
                    ON runtime_server_links(json_extract(record,'$.parent'))
                    WHERE json_extract(record,'$.side')='home';
CREATE INDEX runtime_server_output_tools_expiry
                    ON runtime_server_output_tools(expires);
CREATE INDEX runtime_agent_native_scope ON runtime_agents(
                    json_extract(record,'$.threadId'),
                    CASE WHEN json_type(record,'$.accountKey') IS NULL THEN 'default'
                         ELSE json_extract(record,'$.accountKey') END);
CREATE INDEX runtime_agent_native_reset_pending ON runtime_agents(id)
                    WHERE json_extract(record,'$.nativeRelease.resetPending')=1;
CREATE INDEX runtime_agent_account_scope ON runtime_agents(
                    CASE WHEN json_type(record,'$.accountKey') IS NULL THEN 'default'
                         ELSE json_extract(record,'$.accountKey') END);
CREATE INDEX runtime_agent_root ON runtime_agents(
                    json_extract(record,'$.rootId'));
CREATE INDEX runtime_agent_live_root ON runtime_agents(
                    json_extract(record,'$.rootId'),id)
                    WHERE json_extract(record,'$.deletedAt') IS NULL;
CREATE INDEX runtime_agent_parent ON runtime_agents(
                    json_extract(record,'$.parentId'));
CREATE INDEX runtime_agent_global_active ON runtime_agents(
                    json_extract(record,'$.status')) WHERE json_extract(record,'$.status') IN
                    ('running','starting','approval');
CREATE INDEX runtime_agent_inflight ON runtime_agents(
                    json_extract(record,'$.inFlight')) WHERE json_extract(record,'$.inFlight')=1;
CREATE INDEX runtime_agent_reservation_cwd ON runtime_agents(
                    json_extract(record,'$.cwd'))
                    WHERE json_type(record,'$.workspaceOperation')='text'
                      AND json_extract(record,'$.workspaceOperation')!='';
CREATE INDEX runtime_agent_capacity_retry_state_due_v2 ON runtime_agents(
                    json_extract(record,'$.capacityRetry.status'),
                    json_extract(record,'$.capacityRetry.dueAt'));
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
CREATE INDEX runtime_monitor_status ON runtime_monitors(json_extract(record,'$.status'),json_extract(record,'$.created'));
CREATE INDEX runtime_monitor_agent_status ON runtime_monitors(
                    json_extract(record,'$.agent'),json_extract(record,'$.status'));
CREATE INDEX runtime_monitor_stall_due ON runtime_monitors(
                    COALESCE(json_extract(record,'$.activityAt'),json_extract(record,'$.created'),1e99)+
                    COALESCE(json_extract(record,'$.stallTimeoutSeconds'),1800))
                    WHERE json_extract(record,'$.status')='running';
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
CREATE TRIGGER runtime_agent_record_generation_insert AFTER INSERT ON runtime_agents BEGIN UPDATE runtime_agent_record_generation SET value=value+1 WHERE id=1; UPDATE runtime_scheduler_agent_change_state SET entries=entries+CASE WHEN EXISTS(SELECT 1 FROM runtime_scheduler_agent_changes WHERE id=NEW.id) THEN 0 ELSE 1 END,total=total-COALESCE((SELECT generation FROM runtime_scheduler_agent_changes WHERE id=NEW.id),0)+(SELECT value FROM runtime_agent_record_generation WHERE id=1) WHERE id=1; INSERT INTO runtime_scheduler_agent_changes(id,generation) SELECT NEW.id,value FROM runtime_agent_record_generation WHERE id=1 ON CONFLICT(id) DO UPDATE SET generation=excluded.generation; UPDATE runtime_scheduler_agent_change_state SET generation=(SELECT value FROM runtime_agent_record_generation WHERE id=1) WHERE id=1; END;
CREATE TRIGGER runtime_agent_record_generation_update AFTER UPDATE ON runtime_agents WHEN OLD.record IS NOT NEW.record OR OLD.id IS NOT NEW.id OR OLD.rowid IS NOT NEW.rowid BEGIN UPDATE runtime_agent_record_generation SET value=value+1 WHERE id=1; UPDATE runtime_scheduler_agent_change_state SET entries=entries+CASE WHEN EXISTS(SELECT 1 FROM runtime_scheduler_agent_changes WHERE id=OLD.id) THEN 0 ELSE 1 END,total=total-COALESCE((SELECT generation FROM runtime_scheduler_agent_changes WHERE id=OLD.id),0)+(SELECT value FROM runtime_agent_record_generation WHERE id=1) WHERE id=1 AND OLD.id IS NOT NEW.id; INSERT INTO runtime_scheduler_agent_changes(id,generation) SELECT OLD.id,value FROM runtime_agent_record_generation WHERE id=1 AND OLD.id IS NOT NEW.id ON CONFLICT(id) DO UPDATE SET generation=excluded.generation; UPDATE runtime_scheduler_agent_change_state SET entries=entries+CASE WHEN EXISTS(SELECT 1 FROM runtime_scheduler_agent_changes WHERE id=NEW.id) THEN 0 ELSE 1 END,total=total-COALESCE((SELECT generation FROM runtime_scheduler_agent_changes WHERE id=NEW.id),0)+(SELECT value FROM runtime_agent_record_generation WHERE id=1) WHERE id=1; INSERT INTO runtime_scheduler_agent_changes(id,generation) SELECT NEW.id,value FROM runtime_agent_record_generation WHERE id=1 ON CONFLICT(id) DO UPDATE SET generation=excluded.generation; UPDATE runtime_scheduler_agent_change_state SET generation=(SELECT value FROM runtime_agent_record_generation WHERE id=1) WHERE id=1; END;
CREATE TRIGGER runtime_agent_record_generation_delete AFTER DELETE ON runtime_agents BEGIN UPDATE runtime_agent_record_generation SET value=value+1 WHERE id=1; UPDATE runtime_scheduler_agent_change_state SET entries=entries+CASE WHEN EXISTS(SELECT 1 FROM runtime_scheduler_agent_changes WHERE id=OLD.id) THEN 0 ELSE 1 END,total=total-COALESCE((SELECT generation FROM runtime_scheduler_agent_changes WHERE id=OLD.id),0)+(SELECT value FROM runtime_agent_record_generation WHERE id=1) WHERE id=1; INSERT INTO runtime_scheduler_agent_changes(id,generation) SELECT OLD.id,value FROM runtime_agent_record_generation WHERE id=1 ON CONFLICT(id) DO UPDATE SET generation=excluded.generation; UPDATE runtime_scheduler_agent_change_state SET generation=(SELECT value FROM runtime_agent_record_generation WHERE id=1) WHERE id=1; END;
CREATE INDEX runtime_turn_item_link_scope_v1 ON runtime_turn_item_links_v1(agent,turn_id,epoch,created,id);
CREATE TRIGGER runtime_turn_item_link_insert_v1 AFTER INSERT ON runtime_items BEGIN INSERT INTO runtime_turn_item_links_v1(id,agent,turn_id,created,epoch) VALUES (NEW.id,NEW.agent,json_extract(NEW.record,'$.turnId'),NEW.created,COALESCE((SELECT epoch FROM runtime_turn_item_link_state_v1 WHERE id=1),'')) ON CONFLICT(id) DO UPDATE SET agent=excluded.agent,turn_id=excluded.turn_id,created=excluded.created,epoch=excluded.epoch WHERE runtime_turn_item_links_v1.agent IS NOT excluded.agent OR runtime_turn_item_links_v1.turn_id IS NOT excluded.turn_id OR runtime_turn_item_links_v1.created IS NOT excluded.created OR runtime_turn_item_links_v1.epoch IS NOT excluded.epoch; END;
CREATE TRIGGER runtime_turn_item_link_update_v1 AFTER UPDATE ON runtime_items WHEN OLD.id IS NOT NEW.id OR OLD.agent IS NOT NEW.agent OR OLD.record IS NOT NEW.record OR OLD.created IS NOT NEW.created OR OLD.rowid IS NOT NEW.rowid BEGIN DELETE FROM runtime_turn_item_links_v1 WHERE id=OLD.id AND OLD.id IS NOT NEW.id; INSERT INTO runtime_turn_item_links_v1(id,agent,turn_id,created,epoch) VALUES (NEW.id,NEW.agent,json_extract(NEW.record,'$.turnId'),NEW.created,COALESCE((SELECT epoch FROM runtime_turn_item_link_state_v1 WHERE id=1),'')) ON CONFLICT(id) DO UPDATE SET agent=excluded.agent,turn_id=excluded.turn_id,created=excluded.created,epoch=excluded.epoch WHERE runtime_turn_item_links_v1.agent IS NOT excluded.agent OR runtime_turn_item_links_v1.turn_id IS NOT excluded.turn_id OR runtime_turn_item_links_v1.created IS NOT excluded.created OR runtime_turn_item_links_v1.epoch IS NOT excluded.epoch; END;
CREATE TRIGGER runtime_turn_item_link_delete_v1 AFTER DELETE ON runtime_items BEGIN DELETE FROM runtime_turn_item_links_v1 WHERE id=OLD.id;  END;
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
CREATE INDEX runtime_federation_nonce_age
          ON runtime_federation_nonces(created);
CREATE INDEX runtime_event_created_id ON runtime_events(created DESC,id);
CREATE TRIGGER sync_entity_event_INSERT
          AFTER INSERT ON runtime_events  BEGIN
          INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted)
          VALUES ('event',NEW.id,(SELECT max(seq)+1 FROM (
                    SELECT COALESCE(MAX(seq),0) seq FROM sync_entities UNION ALL
                    SELECT COALESCE(MAX(seq),0) FROM sync_documents UNION ALL
                    SELECT COALESCE(MAX(seq),0) FROM sync_versions)),
                  sync_entity_hash(sync_entity_payload('event',NEW.id,json_object('id',NEW.id,'agent',NEW.agent,'kind',NEW.kind,'status',NEW.status,'created',NEW.created,'error',NEW.error),0)),sync_entity_payload('event',NEW.id,json_object('id',NEW.id,'agent',NEW.agent,'kind',NEW.kind,'status',NEW.status,'created',NEW.created,'error',NEW.error),0),0)
          ON CONFLICT(collection,id) DO UPDATE SET
            seq=excluded.seq,hash=excluded.hash,payload=excluded.payload,deleted=excluded.deleted
          WHERE sync_entities.hash!=excluded.hash OR sync_entities.deleted!=excluded.deleted;
          INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted)
        SELECT 'event',e.id,(SELECT max(seq) FROM (
        SELECT COALESCE(MAX(seq),0) seq FROM sync_entities UNION ALL
        SELECT COALESCE(MAX(seq),0) FROM sync_documents UNION ALL
        SELECT COALESCE(MAX(seq),0) FROM sync_versions))+ROW_NUMBER() OVER (ORDER BY e.created DESC,e.id),
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
        SELECT COALESCE(MAX(seq),0) FROM sync_documents UNION ALL
        SELECT COALESCE(MAX(seq),0) FROM sync_versions))+ROW_NUMBER() OVER (ORDER BY s.id),
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
                    SELECT COALESCE(MAX(seq),0) FROM sync_documents UNION ALL
                    SELECT COALESCE(MAX(seq),0) FROM sync_versions)),
                  sync_entity_hash(sync_entity_payload('event',NEW.id,json_object('id',NEW.id,'agent',NEW.agent,'kind',NEW.kind,'status',NEW.status,'created',NEW.created,'error',NEW.error),0)),sync_entity_payload('event',NEW.id,json_object('id',NEW.id,'agent',NEW.agent,'kind',NEW.kind,'status',NEW.status,'created',NEW.created,'error',NEW.error),0),0)
          ON CONFLICT(collection,id) DO UPDATE SET
            seq=excluded.seq,hash=excluded.hash,payload=excluded.payload,deleted=excluded.deleted
          WHERE sync_entities.hash!=excluded.hash OR sync_entities.deleted!=excluded.deleted;
          INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted)
        SELECT 'event',NEW.id,(SELECT max(seq)+1 FROM (
                    SELECT COALESCE(MAX(seq),0) seq FROM sync_entities UNION ALL
                    SELECT COALESCE(MAX(seq),0) FROM sync_documents UNION ALL
                    SELECT COALESCE(MAX(seq),0) FROM sync_versions)),
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
                    SELECT COALESCE(MAX(seq),0) FROM sync_documents UNION ALL
                    SELECT COALESCE(MAX(seq),0) FROM sync_versions)),
                  sync_entity_hash(sync_entity_payload('event',NEW.id,json_object('id',NEW.id,'agent',NEW.agent,'kind',NEW.kind,'status',NEW.status,'created',NEW.created,'error',NEW.error),0)),sync_entity_payload('event',NEW.id,json_object('id',NEW.id,'agent',NEW.agent,'kind',NEW.kind,'status',NEW.status,'created',NEW.created,'error',NEW.error),0),0)
          ON CONFLICT(collection,id) DO UPDATE SET
            seq=excluded.seq,hash=excluded.hash,payload=excluded.payload,deleted=excluded.deleted
          WHERE sync_entities.hash!=excluded.hash OR sync_entities.deleted!=excluded.deleted;
          INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted)
        SELECT 'event',e.id,(SELECT max(seq) FROM (
        SELECT COALESCE(MAX(seq),0) seq FROM sync_entities UNION ALL
        SELECT COALESCE(MAX(seq),0) FROM sync_documents UNION ALL
        SELECT COALESCE(MAX(seq),0) FROM sync_versions))+ROW_NUMBER() OVER (ORDER BY e.created DESC,e.id),
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
        SELECT COALESCE(MAX(seq),0) FROM sync_documents UNION ALL
        SELECT COALESCE(MAX(seq),0) FROM sync_versions))+ROW_NUMBER() OVER (ORDER BY s.id),
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
                    SELECT COALESCE(MAX(seq),0) FROM sync_documents UNION ALL
                    SELECT COALESCE(MAX(seq),0) FROM sync_versions)),
                  sync_entity_hash(sync_entity_payload('event',OLD.id,'{}',1)),sync_entity_payload('event',OLD.id,'{}',1),1)
          ON CONFLICT(collection,id) DO UPDATE SET
            seq=excluded.seq,hash=excluded.hash,payload=excluded.payload,deleted=excluded.deleted
          WHERE sync_entities.hash!=excluded.hash OR sync_entities.deleted!=excluded.deleted;
          INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted)
        SELECT 'event',e.id,(SELECT max(seq) FROM (
        SELECT COALESCE(MAX(seq),0) seq FROM sync_entities UNION ALL
        SELECT COALESCE(MAX(seq),0) FROM sync_documents UNION ALL
        SELECT COALESCE(MAX(seq),0) FROM sync_versions))+ROW_NUMBER() OVER (ORDER BY e.created DESC,e.id),
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
        SELECT COALESCE(MAX(seq),0) FROM sync_documents UNION ALL
        SELECT COALESCE(MAX(seq),0) FROM sync_versions))+ROW_NUMBER() OVER (ORDER BY s.id),
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
CREATE INDEX runtime_budget_usage_agent ON runtime_budget_usage(agent,kind);
CREATE INDEX runtime_work_root_status
                ON runtime_work(json_extract(record,'$.rootId'),json_extract(record,'$.status'));
CREATE INDEX runtime_work_owner_status
                ON runtime_work(json_extract(record,'$.owner'),json_extract(record,'$.status'));
CREATE INDEX runtime_work_owner_root_status_id ON runtime_work(
                json_extract(record,'$.owner'),json_extract(record,'$.rootId'),
                json_extract(record,'$.status'),id);
CREATE INDEX runtime_work_archive_due ON runtime_work(
                COALESCE(json_extract(record,'$.archiveIntent.nextAttemptAt'),0),id)
                WHERE json_extract(record,'$.archiveIntent.status')='pending';
CREATE INDEX runtime_payload_refs_hash ON runtime_payload_refs(sha256);
CREATE TRIGGER runtime_payload_checkpoints_insert AFTER INSERT ON runtime_checkpoints BEGIN
            INSERT OR REPLACE INTO runtime_payload_refs(table_name,record_id,field,sha256)
            SELECT 'runtime_checkpoints',NEW.id,j.key,json_extract(j.value,'$.sha256')
            FROM json_each(json_extract(NEW.record,'$._payloadBlobs')) AS j
            WHERE json_type(j.value,'$.sha256')='text';
        END;
CREATE TRIGGER runtime_payload_checkpoints_update AFTER UPDATE OF record ON runtime_checkpoints BEGIN
            DELETE FROM runtime_payload_refs WHERE table_name='runtime_checkpoints' AND record_id=OLD.id;
            INSERT OR REPLACE INTO runtime_payload_refs(table_name,record_id,field,sha256)
            SELECT 'runtime_checkpoints',NEW.id,j.key,json_extract(j.value,'$.sha256')
            FROM json_each(json_extract(NEW.record,'$._payloadBlobs')) AS j
            WHERE json_type(j.value,'$.sha256')='text';
        END;
CREATE TRIGGER runtime_payload_checkpoints_delete AFTER DELETE ON runtime_checkpoints BEGIN
            DELETE FROM runtime_payload_refs WHERE table_name='runtime_checkpoints' AND record_id=OLD.id;
        END;
CREATE TRIGGER runtime_payload_requests_insert AFTER INSERT ON runtime_tool_requests BEGIN
            INSERT OR REPLACE INTO runtime_payload_refs(table_name,record_id,field,sha256)
            SELECT 'runtime_tool_requests',NEW.id,j.key,json_extract(j.value,'$.sha256')
            FROM json_each(json_extract(NEW.record,'$._payloadBlobs')) AS j
            WHERE json_type(j.value,'$.sha256')='text';
        END;
CREATE TRIGGER runtime_payload_requests_update AFTER UPDATE OF record ON runtime_tool_requests BEGIN
            DELETE FROM runtime_payload_refs WHERE table_name='runtime_tool_requests' AND record_id=OLD.id;
            INSERT OR REPLACE INTO runtime_payload_refs(table_name,record_id,field,sha256)
            SELECT 'runtime_tool_requests',NEW.id,j.key,json_extract(j.value,'$.sha256')
            FROM json_each(json_extract(NEW.record,'$._payloadBlobs')) AS j
            WHERE json_type(j.value,'$.sha256')='text';
        END;
CREATE TRIGGER runtime_payload_requests_delete AFTER DELETE ON runtime_tool_requests BEGIN
            DELETE FROM runtime_payload_refs WHERE table_name='runtime_tool_requests' AND record_id=OLD.id;
        END;
CREATE TRIGGER runtime_payload_results_insert AFTER INSERT ON runtime_tool_results BEGIN
            INSERT OR REPLACE INTO runtime_payload_refs(table_name,record_id,field,sha256)
            SELECT 'runtime_tool_results',NEW.id,j.key,json_extract(j.value,'$.sha256')
            FROM json_each(json_extract(NEW.result,'$._payloadBlobs')) AS j
            WHERE json_type(j.value,'$.sha256')='text';
        END;
CREATE TRIGGER runtime_payload_results_update AFTER UPDATE OF result ON runtime_tool_results BEGIN
            DELETE FROM runtime_payload_refs WHERE table_name='runtime_tool_results' AND record_id=OLD.id;
            INSERT OR REPLACE INTO runtime_payload_refs(table_name,record_id,field,sha256)
            SELECT 'runtime_tool_results',NEW.id,j.key,json_extract(j.value,'$.sha256')
            FROM json_each(json_extract(NEW.result,'$._payloadBlobs')) AS j
            WHERE json_type(j.value,'$.sha256')='text';
        END;
CREATE TRIGGER runtime_payload_results_delete AFTER DELETE ON runtime_tool_results BEGIN
            DELETE FROM runtime_payload_refs WHERE table_name='runtime_tool_results' AND record_id=OLD.id;
        END;
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
CREATE INDEX runtime_agent_dispatch_active ON runtime_agents(json_extract(record,'$.rootId')) WHERE json_extract(record,'$.inFlight')=1 OR json_extract(record,'$.status') IN ('running','starting','approval');
CREATE INDEX runtime_agent_dispatch_busy_input ON runtime_agents(json_extract(record,'$.rootId')) WHERE json_extract(record,'$.startAttempt.activeAtReservation')=1;
CREATE INDEX runtime_agent_dispatch_workspace ON runtime_agents(json_extract(record,'$.cwd')) WHERE json_type(record,'$.workspaceOperation')='text' AND json_extract(record,'$.workspaceOperation')!='';
CREATE INDEX runtime_account_transfer_status ON runtime_account_transfers(json_extract(record,'$.status'));
CREATE TRIGGER sync_transcript_revision_runtime_agents_insert AFTER INSERT ON runtime_agents BEGIN INSERT INTO runtime_transcript_revisions(agent,revision)
                                    VALUES (NEW.id,1) ON CONFLICT(agent) DO UPDATE SET revision=revision+1; END;
CREATE TRIGGER sync_transcript_revision_runtime_agents_update AFTER UPDATE ON runtime_agents BEGIN INSERT INTO runtime_transcript_revisions(agent,revision)
                                    VALUES (NEW.id,1) ON CONFLICT(agent) DO UPDATE SET revision=revision+1;
                                    INSERT INTO runtime_transcript_revisions(agent,revision)
                                    SELECT OLD.id,1 WHERE OLD.id!=NEW.id
                                    ON CONFLICT(agent) DO UPDATE SET revision=revision+1; END;
CREATE TRIGGER sync_transcript_revision_runtime_agents_delete AFTER DELETE ON runtime_agents BEGIN INSERT INTO runtime_transcript_revisions(agent,revision)
                                    VALUES (OLD.id,1) ON CONFLICT(agent) DO UPDATE SET revision=revision+1; END;
CREATE TRIGGER sync_transcript_revision_runtime_items_insert AFTER INSERT ON runtime_items BEGIN INSERT INTO runtime_transcript_revisions(agent,revision)
                                    VALUES (NEW.agent,1) ON CONFLICT(agent) DO UPDATE SET revision=revision+1; END;
CREATE TRIGGER sync_transcript_revision_runtime_items_update AFTER UPDATE ON runtime_items BEGIN INSERT INTO runtime_transcript_revisions(agent,revision)
                                    VALUES (NEW.agent,1) ON CONFLICT(agent) DO UPDATE SET revision=revision+1;
                                    INSERT INTO runtime_transcript_revisions(agent,revision)
                                    SELECT OLD.agent,1 WHERE OLD.agent!=NEW.agent
                                    ON CONFLICT(agent) DO UPDATE SET revision=revision+1; END;
CREATE TRIGGER sync_transcript_revision_runtime_items_delete AFTER DELETE ON runtime_items BEGIN INSERT INTO runtime_transcript_revisions(agent,revision)
                                    VALUES (OLD.agent,1) ON CONFLICT(agent) DO UPDATE SET revision=revision+1; END;
PRAGMA writable_schema=OFF;
DELETE FROM "sqlite_sequence";
INSERT INTO "sqlite_sequence" VALUES('runtime_chat_messages',1);
COMMIT;

-- Current Rust full-text records for the current search index.
INSERT OR IGNORE INTO runtime_item_fulltext(id,body) VALUES('agent:old-0','needle history 0');
INSERT OR IGNORE INTO runtime_item_fulltext(id,body) VALUES('agent:old-1','needle history 1');
INSERT OR IGNORE INTO runtime_item_fulltext(id,body) VALUES('agent:old-2','needle history 2');
INSERT OR IGNORE INTO runtime_item_fulltext(id,body) VALUES('agent:old-3','needle history 3');
INSERT OR IGNORE INTO runtime_item_fulltext(id,body) VALUES('agent:old-4','needle history 4');
INSERT OR IGNORE INTO runtime_item_fulltext(id,body) VALUES('agent:old-5','needle history 5');
INSERT OR IGNORE INTO runtime_item_fulltext(id,body) VALUES('agent:old-6','needle history 6');
INSERT OR IGNORE INTO runtime_item_fulltext(id,body) VALUES('agent:old-7','needle history 7');
INSERT OR IGNORE INTO runtime_item_fulltext(id,body) VALUES('other:other','needle foreign');
INSERT OR IGNORE INTO runtime_item_fulltext(id,body) VALUES('agent:input','Needle submitted input');
INSERT OR IGNORE INTO runtime_item_fulltext(id,body) VALUES('agent:command','{"id": "command", "type": "commandExecution", "exitCode": 0, "aggregatedOutput": "needle tool output", "outputTruncated": false, "durationMs": 0}');
INSERT OR IGNORE INTO runtime_item_fulltext(id,body) VALUES('agent:compact','{"id": "compact", "type": "contextCompaction", "durationMs": 0}');
