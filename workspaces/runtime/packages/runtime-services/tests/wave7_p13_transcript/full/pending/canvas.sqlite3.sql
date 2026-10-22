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
INSERT INTO "runtime_items" VALUES('agent:old-0','agent','{"id": "agent:old-0", "role": "assistant", "title": "Agent", "text": "needle history 0", "truncated": false, "at": 100.0, "turnId": "old-turn"}',100.0);
INSERT INTO "runtime_items" VALUES('agent:old-1','agent','{"id": "agent:old-1", "role": "assistant", "title": "Agent", "text": "needle history 1", "truncated": false, "at": 101.0, "turnId": "old-turn"}',101.0);
INSERT INTO "runtime_items" VALUES('agent:old-2','agent','{"id": "agent:old-2", "role": "assistant", "title": "Agent", "text": "needle history 2", "truncated": false, "at": 102.0, "turnId": "old-turn"}',102.0);
INSERT INTO "runtime_items" VALUES('agent:old-3','agent','{"id": "agent:old-3", "role": "assistant", "title": "Agent", "text": "needle history 3", "truncated": false, "at": 103.0, "turnId": "old-turn"}',103.0);
INSERT INTO "runtime_items" VALUES('agent:old-4','agent','{"id": "agent:old-4", "role": "assistant", "title": "Agent", "text": "needle history 4", "truncated": false, "at": 104.0, "turnId": "old-turn"}',104.0);
INSERT INTO "runtime_items" VALUES('agent:old-5','agent','{"id": "agent:old-5", "role": "assistant", "title": "Agent", "text": "needle history 5", "truncated": false, "at": 105.0, "turnId": "old-turn"}',105.0);
INSERT INTO "runtime_items" VALUES('agent:old-6','agent','{"id": "agent:old-6", "role": "assistant", "title": "Agent", "text": "needle history 6", "truncated": false, "at": 106.0, "turnId": "old-turn"}',106.0);
INSERT INTO "runtime_items" VALUES('agent:old-7','agent','{"id": "agent:old-7", "role": "assistant", "title": "Agent", "text": "needle history 7", "truncated": false, "at": 107.0, "turnId": "old-turn"}',107.0);
INSERT INTO "runtime_items" VALUES('other:other','other','{"id": "other:other", "role": "assistant", "title": "Agent", "text": "needle foreign", "truncated": false, "at": 1792689638.0096738}',1792689638.0096784);
INSERT INTO "runtime_items" VALUES('agent:input','agent','{"id": "agent:input", "role": "user", "title": "You", "text": "Needle submitted input", "truncated": false, "at": 200.0, "turnId": "turn", "inputs": [{"id": "input", "at": 200.0, "kind": "user", "text": "Needle submitted input", "truncated": false, "assets": []}]}',200.0);
INSERT INTO "runtime_items" VALUES('agent:answer','agent','{"id": "agent:answer", "role": "assistant", "title": "Assistant", "text": "Needle streamed answer", "truncated": false, "at": 300.0, "streaming": true, "turnId": "turn", "phase": null}',300.0);
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
INSERT INTO sqlite_master(type,name,tbl_name,rootpage,sql)VALUES('table','runtime_search','runtime_search',0,'CREATE VIRTUAL TABLE runtime_search USING fts5(id UNINDEXED,agent UNINDEXED,kind UNINDEXED,body,tokenize=''unicode61'')');
CREATE TABLE 'runtime_search_config'(k PRIMARY KEY, v) WITHOUT ROWID;
INSERT INTO "runtime_search_config" VALUES('version',4);
CREATE TABLE 'runtime_search_content'(id INTEGER PRIMARY KEY, c0, c1, c2, c3);
INSERT INTO "runtime_search_content" VALUES(1,'agent:old-0','agent','Agent','needle history 0');
INSERT INTO "runtime_search_content" VALUES(2,'agent:old-1','agent','Agent','needle history 1');
INSERT INTO "runtime_search_content" VALUES(3,'agent:old-2','agent','Agent','needle history 2');
INSERT INTO "runtime_search_content" VALUES(4,'agent:old-3','agent','Agent','needle history 3');
INSERT INTO "runtime_search_content" VALUES(5,'agent:old-4','agent','Agent','needle history 4');
INSERT INTO "runtime_search_content" VALUES(6,'agent:old-5','agent','Agent','needle history 5');
INSERT INTO "runtime_search_content" VALUES(7,'agent:old-6','agent','Agent','needle history 6');
INSERT INTO "runtime_search_content" VALUES(8,'agent:old-7','agent','Agent','needle history 7');
INSERT INTO "runtime_search_content" VALUES(9,'other:other','other','Agent','needle foreign');
INSERT INTO "runtime_search_content" VALUES(10,'agent:input','agent','You','Needle submitted input');
INSERT INTO "runtime_search_content" VALUES(12,'agent:command','agent','commandExecution','{"id": "command", "type": "commandExecution", "exitCode": 0, "aggregatedOutput": "needle tool output", "outputTruncated": false, "durationMs": 0}');
INSERT INTO "runtime_search_content" VALUES(13,'agent:compact','agent','contextCompaction','{"id": "compact", "type": "contextCompaction", "durationMs": 0}');
CREATE TABLE 'runtime_search_data'(id INTEGER PRIMARY KEY, block BLOB);
INSERT INTO "runtime_search_data" VALUES(1,X'0C00000031');
INSERT INTO "runtime_search_data" VALUES(10,X'00000000010E0E000E0101010201010301010401010501010601010701010801010901010A01010B01010C01010D01010E0101');
INSERT INTO "runtime_search_data" VALUES(137438953473,X'0000002702303001060103040107686973746F7279010601030301066E6565646C65010601030204080E');
INSERT INTO "runtime_search_data" VALUES(274877906945,X'0000002702303102060103040107686973746F7279020601030301066E6565646C65020601030204080E');
INSERT INTO "runtime_search_data" VALUES(412316860417,X'0000002702303203060103040107686973746F7279030601030301066E6565646C65030601030204080E');
INSERT INTO "runtime_search_data" VALUES(549755813889,X'0000002702303304060103040107686973746F7279040601030301066E6565646C65040601030204080E');
INSERT INTO "runtime_search_data" VALUES(687194767361,X'0000002702303405060103040107686973746F7279050601030301066E6565646C65050601030204080E');
INSERT INTO "runtime_search_data" VALUES(824633720833,X'0000002702303506060103040107686973746F7279060601030301066E6565646C65060601030204080E');
INSERT INTO "runtime_search_data" VALUES(962072674305,X'0000002702303607060103040107686973746F7279070601030301066E6565646C65070601030204080E');
INSERT INTO "runtime_search_data" VALUES(1099511627777,X'0000002702303708060103040107686973746F7279080601030301066E6565646C65080601030204080E');
INSERT INTO "runtime_search_data" VALUES(1236950581249,X'0000001F0830666F726569676E090601030301066E6565646C650906010302040E');
INSERT INTO "runtime_search_data" VALUES(1374389534721,X'0000002D0630696E7075740A0601030401066E6565646C650A0601030201097375626D69747465640A06010303040C0D');
INSERT INTO "runtime_search_data" VALUES(1511828488193,X'00000068033034320C0601030A0107636F6D6D616E640C08010303050809657865637574696F6E0C06010305010269640C0601030201066E6565646C650C0601030801067072696E74660C0601030703076F6365737369640C060103090104747970650C0601030404090F10090D0D0E');
INSERT INTO "runtime_search_data" VALUES(1649267441665,X'0000004F033034320C010107636F6D6D616E640C010809657865637574696F6E0C01010269640C0101066E6565646C650C0101067072696E74660C0103076F6365737369640C010104747970650C0104060B0D060A0A0B');
INSERT INTO "runtime_search_data" VALUES(1786706395137,X'000000B60230300C080103070A0110616767726567617465646F75747075740C060103080107636F6D6D616E640C060103030809657865637574696F6E0C06010305010A6475726174696F6E6D730C0601030E010865786974636F64650C06010306010566616C73650C0601030D010269640C0601030201066E6565646C650C0601030901066F75747075740C0601030B07097472756E63617465640C0601030C0104746F6F6C0C0601030A02037970650C060103040409170E10110F0C090D0D100B');
INSERT INTO "runtime_search_data" VALUES(1924145348609,X'000000550230300D060103070107636F6D706163740D06010303030F6E74657874636F6D70616374696F6E0D06010305010A6475726174696F6E6D730D06010306010269640D060103020104747970650D0601030404080E161109');
CREATE TABLE 'runtime_search_docsize'(id INTEGER PRIMARY KEY, sz BLOB);
INSERT INTO "runtime_search_docsize" VALUES(1,X'00000003');
INSERT INTO "runtime_search_docsize" VALUES(2,X'00000003');
INSERT INTO "runtime_search_docsize" VALUES(3,X'00000003');
INSERT INTO "runtime_search_docsize" VALUES(4,X'00000003');
INSERT INTO "runtime_search_docsize" VALUES(5,X'00000003');
INSERT INTO "runtime_search_docsize" VALUES(6,X'00000003');
INSERT INTO "runtime_search_docsize" VALUES(7,X'00000003');
INSERT INTO "runtime_search_docsize" VALUES(8,X'00000003');
INSERT INTO "runtime_search_docsize" VALUES(9,X'00000002');
INSERT INTO "runtime_search_docsize" VALUES(10,X'00000003');
INSERT INTO "runtime_search_docsize" VALUES(12,X'0000000E');
INSERT INTO "runtime_search_docsize" VALUES(13,X'00000006');
CREATE TABLE 'runtime_search_idx'(segid, term, pgno, PRIMARY KEY(segid, term)) WITHOUT ROWID;
INSERT INTO "runtime_search_idx" VALUES(1,X'',2);
INSERT INTO "runtime_search_idx" VALUES(2,X'',2);
INSERT INTO "runtime_search_idx" VALUES(3,X'',2);
INSERT INTO "runtime_search_idx" VALUES(4,X'',2);
INSERT INTO "runtime_search_idx" VALUES(5,X'',2);
INSERT INTO "runtime_search_idx" VALUES(6,X'',2);
INSERT INTO "runtime_search_idx" VALUES(7,X'',2);
INSERT INTO "runtime_search_idx" VALUES(8,X'',2);
INSERT INTO "runtime_search_idx" VALUES(9,X'',2);
INSERT INTO "runtime_search_idx" VALUES(10,X'',2);
INSERT INTO "runtime_search_idx" VALUES(11,X'',2);
INSERT INTO "runtime_search_idx" VALUES(12,X'',2);
INSERT INTO "runtime_search_idx" VALUES(13,X'',2);
INSERT INTO "runtime_search_idx" VALUES(14,X'',2);
CREATE TABLE runtime_search_indexed (id TEXT PRIMARY KEY);
INSERT INTO "runtime_search_indexed" VALUES('agent:old-0');
INSERT INTO "runtime_search_indexed" VALUES('agent:old-1');
INSERT INTO "runtime_search_indexed" VALUES('agent:old-2');
INSERT INTO "runtime_search_indexed" VALUES('agent:old-3');
INSERT INTO "runtime_search_indexed" VALUES('agent:old-4');
INSERT INTO "runtime_search_indexed" VALUES('agent:old-5');
INSERT INTO "runtime_search_indexed" VALUES('agent:old-6');
INSERT INTO "runtime_search_indexed" VALUES('agent:old-7');
INSERT INTO "runtime_search_indexed" VALUES('other:other');
INSERT INTO "runtime_search_indexed" VALUES('agent:input');
INSERT INTO "runtime_search_indexed" VALUES('agent:command');
INSERT INTO "runtime_search_indexed" VALUES('agent:compact');
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
INSERT INTO "runtime_search_rollout" VALUES(1,'building',0,1792689637.8706896);
CREATE TABLE runtime_search_rows (id TEXT PRIMARY KEY, search_rowid INTEGER NOT NULL UNIQUE);
INSERT INTO "runtime_search_rows" VALUES('agent:old-0',1);
INSERT INTO "runtime_search_rows" VALUES('agent:old-1',2);
INSERT INTO "runtime_search_rows" VALUES('agent:old-2',3);
INSERT INTO "runtime_search_rows" VALUES('agent:old-3',4);
INSERT INTO "runtime_search_rows" VALUES('agent:old-4',5);
INSERT INTO "runtime_search_rows" VALUES('agent:old-5',6);
INSERT INTO "runtime_search_rows" VALUES('agent:old-6',7);
INSERT INTO "runtime_search_rows" VALUES('agent:old-7',8);
INSERT INTO "runtime_search_rows" VALUES('other:other',9);
INSERT INTO "runtime_search_rows" VALUES('agent:input',10);
INSERT INTO "runtime_search_rows" VALUES('agent:command',12);
INSERT INTO "runtime_search_rows" VALUES('agent:compact',13);
CREATE TABLE runtime_search_rows_rollout (id INTEGER PRIMARY KEY CHECK(id=1),cursor INTEGER NOT NULL DEFAULT 0);
INSERT INTO "runtime_search_rows_rollout" VALUES(1,0);
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
INSERT INTO "runtime_transcript_revisions" VALUES('agent',35);
INSERT INTO "runtime_transcript_revisions" VALUES('other',2);
INSERT INTO "runtime_transcript_revisions" VALUES('child',1);
CREATE TABLE runtime_turn_item_link_state_v1 (id INTEGER PRIMARY KEY CHECK(id=1),epoch TEXT NOT NULL,source_rootpage INTEGER NOT NULL,cursor INTEGER,target INTEGER,complete INTEGER NOT NULL CHECK(complete IN (0,1)),scanned_rows INTEGER NOT NULL,source_bytes INTEGER NOT NULL,updated REAL NOT NULL);
INSERT INTO "runtime_turn_item_link_state_v1" VALUES(1,'814eff3c807b4a799cc3f63d64d7d0ce',73,NULL,NULL,1,0,0,1792689637.776191);
CREATE TABLE runtime_turn_item_links_v1 (id TEXT PRIMARY KEY,agent TEXT NOT NULL,turn_id,created REAL NOT NULL,epoch TEXT NOT NULL);
INSERT INTO "runtime_turn_item_links_v1" VALUES('agent:old-0','agent','old-turn',100.0,'814eff3c807b4a799cc3f63d64d7d0ce');
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
