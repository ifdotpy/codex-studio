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
INSERT INTO runtime_agent_record_generation VALUES(1,13);
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
INSERT INTO runtime_context_manifests VALUES('777c97c9-d1ee-4c3d-970d-ff0badc41e7c','thread-1',0,1,'777c97c9-d1ee-4c3d-970d-ff0badc41e7c:initial','{"epoch":["thread-1",0],"versions":{"agentMode":"cbaab587bdd3e5b2b44c85e8","progressFile":"5ea10bda4b522c2ab8f4ea52","roleSkill":"7b2dc3aeeb5bbec678451fe6"},"sequence":1}');
CREATE TABLE runtime_context_reminders (
                  agent TEXT NOT NULL, version TEXT NOT NULL, event_id TEXT NOT NULL,
                  PRIMARY KEY(agent,version));
CREATE TABLE runtime_event_meta (id TEXT PRIMARY KEY, record TEXT NOT NULL);
INSERT INTO runtime_event_meta VALUES('777c97c9-d1ee-4c3d-970d-ff0badc41e7c:initial','{"contextManifest":{"epoch":["thread-1",0],"sequence":1,"versions":{"agentMode":"cbaab587bdd3e5b2b44c85e8","progressFile":"5ea10bda4b522c2ab8f4ea52","roleSkill":"7b2dc3aeeb5bbec678451fe6"}},"delivery":"current_turn","modelEventProjection":1,"remoteRequestId":null,"timing":{"acceptedAt":223611195179100,"connectedAt":223611167800099,"contextReadyAt":223611174356712,"dispatchPickedAt":223611077277098,"enqueuedAt":223611023181956,"eventMetaReadyAt":223611172943557,"fastSkipReason":"scheduleOverride","itemReadyAt":223611175660052,"modelTextReadyAt":223611172834372,"nativeSubmitBeganAt":223611177352500,"postPrepareLockedAt":223611166243765,"postPrepareWaitBeganAt":223611165292505,"prepareBeganAt":223611090487196,"prepareChecksDoneAt":223611092557722,"prepareConnectedAt":223611103683378,"prepareGuardAcquiredAt":223611090504649,"prepareGuardReadyAt":223611090504408,"preparedAt":223611166244176,"programReadyAt":223611088168004,"repairBeganAt":223611088170750,"repairCheckedAt":223611090480353,"repairReadyAt":223611165292114,"reservationCommittedAt":223611177352079,"startBeganAt":223611078466875,"submittedAt":223611177700101,"threadParamsReadyAt":223611157631344,"threadReadyAt":223611165283939,"threadSubmittedAt":223611161013215,"transcriptChecksDoneAt":223611168963036,"transcriptLockWaitBeganAt":223611167800490,"transcriptLockedAt":223611168710503,"transcriptReadyAt":223611175660573,"turnParamsReadyAt":223611175670572,"validatedAt":223611086956076},"transcriptItemId":"777c97c9-d1ee-4c3d-970d-ff0badc41e7c:777c97c9-d1ee-4c3d-970d-ff0badc41e7c:initial"}');
CREATE TABLE runtime_events (
                  id TEXT PRIMARY KEY, agent TEXT NOT NULL, kind TEXT NOT NULL,
                  text TEXT NOT NULL, status TEXT NOT NULL, created REAL NOT NULL,
                  epoch INTEGER NOT NULL, turn_id TEXT, error TEXT, receipt_enum_guard INTEGER GENERATED ALWAYS AS (NULL) VIRTUAL CHECK("kind" IN ('user','followup','agent_message','child_result','child_stopped','complaint','complaint_response','chat_review','work_review','work_decision','work_ready','monitor_exit','monitor_stall','rule','rule_missed','rule_stall','radio_turn','browser_declined','browser_joined','browser_outcome','browser_unanswered','event_wake','monitor_cancelled','monitor_output','plan_update','shell_policy','work_released','work_result_invalid','context_repair_wait','monitor_correction','rule_hold')) CHECK("status" IN ('stored_only','pending','reserved','dispatching','delivered','uncertain','cancelled','failed')));
INSERT INTO runtime_events VALUES('777c97c9-d1ee-4c3d-970d-ff0badc41e7c:initial','777c97c9-d1ee-4c3d-970d-ff0badc41e7c','user','Persist a turn','delivered',2000000000,0,'turn-2',NULL);
CREATE TABLE runtime_execution_attempts (id TEXT PRIMARY KEY, run TEXT NOT NULL, record TEXT NOT NULL, submission TEXT GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN record->>'$.submission' END) VIRTUAL CHECK("submission" IN ('unsent','pending','accepted','observed','rejected','unknown')), turn_status TEXT GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN record->>'$.turnStatus' END) VIRTUAL CHECK("turn_status" IN ('unsent','pending','accepted','observed','running','unknown','rejected','completed','failed','interrupted','cancelled')));
INSERT INTO runtime_execution_attempts VALUES('0897d819-94be-44cd-930e-574c1c026bb7','run:0897d819-94be-44cd-930e-574c1c026bb7','{"accountKey":"default","activeAtReservation":false,"admission":{"created":1793451957257,"id":"[\"execution-attempt\",\"0897d819-94be-44cd-930e-574c1c026bb7\"]","signature":"{\"agent\":\"777c97c9-d1ee-4c3d-970d-ff0badc41e7c\",\"attempt\":\"0897d819-94be-44cd-930e-574c1c026bb7\",\"epoch\":0}"},"connectionId":"cdd0a7b5-7876-4a7f-9955-d1325a58ecdb","created":2000000000.0,"epoch":0,"events":["777c97c9-d1ee-4c3d-970d-ff0badc41e7c:initial"],"id":"0897d819-94be-44cd-930e-574c1c026bb7","nativeOperationId":"turn:777c97c9-d1ee-4c3d-970d-ff0badc41e7c:777c97c9-d1ee-4c3d-970d-ff0badc41e7c:initial:attempt:0897d819-94be-44cd-930e-574c1c026bb7","observedTurnId":"turn-2","prepareAttemptId":"a8d9e68e-2041-4afd-a183-eee37a5316d7","resultRunId":null,"runId":"run:0897d819-94be-44cd-930e-574c1c026bb7","settingsFixed":true,"submission":"accepted","submitted":true,"supervisorIdentity":null,"threadId":"thread-1","turnId":"turn-2","turnStatus":null}');
CREATE TABLE runtime_execution_effects (id TEXT PRIMARY KEY, run TEXT NOT NULL, kind TEXT NOT NULL, reference TEXT NOT NULL, record TEXT NOT NULL, status TEXT GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN record->>'$.status' END) VIRTUAL CHECK("status" IN ('unknown','idle','queued','starting','running','waiting','waiting_on_monitor','approval','completed','interrupted','failed','blocked','paused','parked','abandoned','capacity-retry','pending','cancelled','answering','answered','uncertain','resolved','lost','expired','deleted')), receipt_enum_guard INTEGER GENERATED ALWAYS AS (NULL) VIRTUAL CHECK("kind" IN ('spawn','tool_requests','requests','monitors','task_submit','unrecognized')));
INSERT INTO runtime_execution_effects VALUES('requests:777c97c9-d1ee-4c3d-970d-ff0badc41e7c:question:retained-question','run:0897d819-94be-44cd-930e-574c1c026bb7','requests','777c97c9-d1ee-4c3d-970d-ff0badc41e7c:question:retained-question','{"id": "requests:777c97c9-d1ee-4c3d-970d-ff0badc41e7c:question:retained-question", "runId": "run:0897d819-94be-44cd-930e-574c1c026bb7", "kind": "requests", "referenceId": "777c97c9-d1ee-4c3d-970d-ff0badc41e7c:question:retained-question", "attemptId": "0897d819-94be-44cd-930e-574c1c026bb7", "requestId": null, "status": "pending", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:8a0fd963-9799-4fd8-9201-d7e7cd13744f','run:0897d819-94be-44cd-930e-574c1c026bb7','requests','8a0fd963-9799-4fd8-9201-d7e7cd13744f','{"id": "requests:8a0fd963-9799-4fd8-9201-d7e7cd13744f", "runId": "run:0897d819-94be-44cd-930e-574c1c026bb7", "kind": "requests", "referenceId": "8a0fd963-9799-4fd8-9201-d7e7cd13744f", "attemptId": "0897d819-94be-44cd-930e-574c1c026bb7", "requestId": null, "status": "pending", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('monitors:ef833e85-ebe4-5750-a66f-ccf85ea2043c','run:0897d819-94be-44cd-930e-574c1c026bb7','monitors','ef833e85-ebe4-5750-a66f-ccf85ea2043c','{"id": "monitors:ef833e85-ebe4-5750-a66f-ccf85ea2043c", "runId": "run:0897d819-94be-44cd-930e-574c1c026bb7", "kind": "monitors", "referenceId": "ef833e85-ebe4-5750-a66f-ccf85ea2043c", "attemptId": "0897d819-94be-44cd-930e-574c1c026bb7", "requestId": "pending-monitor", "status": "approval", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('requests:fa930719-b220-4226-96cd-1e175ce13b0c','run:0897d819-94be-44cd-930e-574c1c026bb7','requests','fa930719-b220-4226-96cd-1e175ce13b0c','{"id": "requests:fa930719-b220-4226-96cd-1e175ce13b0c", "runId": "run:0897d819-94be-44cd-930e-574c1c026bb7", "kind": "requests", "referenceId": "fa930719-b220-4226-96cd-1e175ce13b0c", "attemptId": "0897d819-94be-44cd-930e-574c1c026bb7", "requestId": null, "status": "pending", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('monitors:7476dd05-6094-5a65-9ba0-3007fab6be6b','run:0897d819-94be-44cd-930e-574c1c026bb7','monitors','7476dd05-6094-5a65-9ba0-3007fab6be6b','{"id": "monitors:7476dd05-6094-5a65-9ba0-3007fab6be6b", "runId": "run:0897d819-94be-44cd-930e-574c1c026bb7", "kind": "monitors", "referenceId": "7476dd05-6094-5a65-9ba0-3007fab6be6b", "attemptId": "0897d819-94be-44cd-930e-574c1c026bb7", "requestId": "running-monitor", "status": "running", "outcome": null, "resultReference": null}');
INSERT INTO runtime_execution_effects VALUES('monitors:rule-linked-monitor','run:0897d819-94be-44cd-930e-574c1c026bb7','monitors','rule-linked-monitor','{"id": "monitors:rule-linked-monitor", "runId": "run:0897d819-94be-44cd-930e-574c1c026bb7", "kind": "monitors", "referenceId": "rule-linked-monitor", "attemptId": "0897d819-94be-44cd-930e-574c1c026bb7", "requestId": "running-monitor", "status": "running", "outcome": null, "resultReference": null}');
CREATE TABLE runtime_execution_inputs (run TEXT NOT NULL, event TEXT NOT NULL, PRIMARY KEY(run,event));
INSERT INTO runtime_execution_inputs VALUES('run:0897d819-94be-44cd-930e-574c1c026bb7','777c97c9-d1ee-4c3d-970d-ff0badc41e7c:initial');
CREATE TABLE runtime_execution_nodes (id TEXT PRIMARY KEY, run TEXT NOT NULL, record TEXT NOT NULL, status TEXT GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN record->>'$.status' END) VIRTUAL CHECK("status" IN ('unknown','idle','queued','starting','running','waiting','waiting_on_monitor','approval','completed','interrupted','failed','blocked','paused','parked','abandoned','capacity-retry')));
CREATE TABLE runtime_execution_runs (id TEXT PRIMARY KEY, agent TEXT NOT NULL, account TEXT NOT NULL, epoch INTEGER NOT NULL, thread TEXT, turn TEXT, created REAL NOT NULL, record TEXT NOT NULL, status TEXT GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN record->>'$.status' END) VIRTUAL CHECK("status" IN ('unsent','pending','accepted','observed','running','unknown','rejected','completed','failed','interrupted','cancelled')));
INSERT INTO runtime_execution_runs VALUES('run:0897d819-94be-44cd-930e-574c1c026bb7','777c97c9-d1ee-4c3d-970d-ff0badc41e7c','default',0,'thread-1','turn-2',2000000000,'{"id": "run:0897d819-94be-44cd-930e-574c1c026bb7", "agent": "777c97c9-d1ee-4c3d-970d-ff0badc41e7c", "accountKey": "default", "epoch": 0, "threadId": "thread-1", "turnId": "turn-2", "created": 2000000000.0, "status": "running", "firstAttemptId": "0897d819-94be-44cd-930e-574c1c026bb7", "rootAttemptId": "0897d819-94be-44cd-930e-574c1c026bb7", "latestAttemptId": "0897d819-94be-44cd-930e-574c1c026bb7"}');
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
INSERT INTO runtime_items VALUES('777c97c9-d1ee-4c3d-970d-ff0badc41e7c:777c97c9-d1ee-4c3d-970d-ff0badc41e7c:initial','777c97c9-d1ee-4c3d-970d-ff0badc41e7c','{"id": "777c97c9-d1ee-4c3d-970d-ff0badc41e7c:777c97c9-d1ee-4c3d-970d-ff0badc41e7c:initial", "role": "user", "title": "User", "text": "Persist a turn\n\n[Studio subagent concurrency, revision 0] Multi agent mode (subagent concurrency 32): delegate sensibly up to 32 simultaneous descendant turns across all providers and reviewer roles. The lead does not use a subagent slot. Excess worker turns queue. Do not poll unchanged status.", "truncated": false, "at": 2000000000.0, "assets": [], "inputs": [{"id": "777c97c9-d1ee-4c3d-970d-ff0badc41e7c:initial", "at": 2000000000.0, "kind": "user", "text": "Persist a turn", "truncated": false, "assets": []}], "turnId": "turn-2"}',2000000000);
INSERT INTO runtime_items VALUES('777c97c9-d1ee-4c3d-970d-ff0badc41e7c:retained-question','777c97c9-d1ee-4c3d-970d-ff0badc41e7c','{"id": "777c97c9-d1ee-4c3d-970d-ff0badc41e7c:retained-question", "role": "assistant", "title": "Assistant", "text": "Choose", "truncated": false, "at": 2000000000.0, "streaming": false, "turnId": "turn-2", "phase": null}',2000000000);
CREATE TABLE runtime_lead_requests (id TEXT PRIMARY KEY, agent TEXT NOT NULL, signature TEXT NOT NULL);
CREATE TABLE runtime_plans (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_profiles (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_projects (id TEXT PRIMARY KEY, record TEXT NOT NULL, removal_reason TEXT GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN record->>'$.removal.reason' END) VIRTUAL CHECK("removal_reason" IN ('delete')));
INSERT INTO runtime_projects VALUES('/FROZEN_STARTUP_STATE','{"id":"/FROZEN_STARTUP_STATE","path":"/FROZEN_STARTUP_STATE","name":"startup","created":2000000000.0,"accountKey":"default","accountRevision":1,"homeServerId":"b44a8684bd59488da64b35c0805633ab","locations":[{"serverId":"b44a8684bd59488da64b35c0805633ab","path":"/FROZEN_STARTUP_STATE","projectId":"/FROZEN_STARTUP_STATE"}],"locationsRevision":0,"peerTeams":[]}');
CREATE TABLE runtime_requests (id TEXT PRIMARY KEY, record TEXT NOT NULL);
INSERT INTO runtime_requests VALUES('777c97c9-d1ee-4c3d-970d-ff0badc41e7c:question:retained-question','{"id":"777c97c9-d1ee-4c3d-970d-ff0badc41e7c:question:retained-question","agent":"777c97c9-d1ee-4c3d-970d-ff0badc41e7c","method":"agent/asyncQuestion","status":"pending","createdAt":2000000000.0,"params":{"questions": [{"id": "0", "question": "Keep this question", "options": [{"label": "yes"}, {"label": "no"}], "multiSelect": false, "isSecret": false}]},"epoch":0,"turnId":"turn-2"}');
INSERT INTO runtime_requests VALUES('8a0fd963-9799-4fd8-9201-d7e7cd13744f','{"id":"8a0fd963-9799-4fd8-9201-d7e7cd13744f","agent":"777c97c9-d1ee-4c3d-970d-ff0badc41e7c","method":"item/commandExecution/requestApproval","status":"pending","createdAt":2000000000.0,"params":{"threadId": "thread-1", "turnId": "turn-2", "itemId": "approval-item", "command": "printf safe"},"accountKey":"default","epoch":0,"rpcId":"provider-approval"}');
INSERT INTO runtime_requests VALUES('fa930719-b220-4226-96cd-1e175ce13b0c','{"id":"fa930719-b220-4226-96cd-1e175ce13b0c","agent":"777c97c9-d1ee-4c3d-970d-ff0badc41e7c","method":"monitor/approve","status":"pending","createdAt":2000000000.0,"params":{"monitorId": "ef833e85-ebe4-5750-a66f-ccf85ea2043c", "command": "printf needs approval", "cwd": "/FROZEN_STARTUP_STATE"}}');
INSERT INTO runtime_requests VALUES('beb71d90-e19c-4a9e-bad5-a017675f4492','{"id":"beb71d90-e19c-4a9e-bad5-a017675f4492","agent":"orphan","method":"mcpServer/elicitation/request","status":"pending","createdAt":2000000000.0,"params":{"mode": "form", "requestedSchema": {"properties": {}}},"connectionId":"cdd0a7b5-7876-4a7f-9955-d1325a58ecdb","accountKey":"default","rpcId":"orphan-request"}');
CREATE TABLE runtime_rooms (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_rules (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_scheduler_agent_change_state (id INTEGER PRIMARY KEY CHECK(id=1),floor INTEGER NOT NULL,generation INTEGER NOT NULL,entries INTEGER NOT NULL,total INTEGER NOT NULL);
INSERT INTO runtime_scheduler_agent_change_state VALUES(1,0,13,1,13);
CREATE TABLE runtime_scheduler_agent_changes (id TEXT PRIMARY KEY,generation INTEGER NOT NULL CHECK(typeof(generation)='integer' AND generation>=0));
INSERT INTO runtime_scheduler_agent_changes VALUES('777c97c9-d1ee-4c3d-970d-ff0badc41e7c',13);
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
INSERT INTO sync_entities VALUES('project','/FROZEN_STARTUP_STATE',1,'23ed4a7271bcc94acf0c1bdec7873a2079eece618b554e7a8679569f4ea762a2','{"collection":"project","id":"/FROZEN_STARTUP_STATE","value":{"accountKey":"default","accountRevision":1,"created":2000000000.0,"homeServerId":"b44a8684bd59488da64b35c0805633ab","id":"/FROZEN_STARTUP_STATE","locations":[{"path":"/FROZEN_STARTUP_STATE","projectId":"/FROZEN_STARTUP_STATE","serverId":"b44a8684bd59488da64b35c0805633ab"}],"locationsRevision":0,"name":"startup","path":"/FROZEN_STARTUP_STATE"}}',0);
INSERT INTO sync_entities VALUES('agent','777c97c9-d1ee-4c3d-970d-ff0badc41e7c',13,'4c79abeef52445497e44b569807b501193611ad0e1c1a635105b6881f40f6fb4','{"collection": "agent", "id": "777c97c9-d1ee-4c3d-970d-ff0badc41e7c", "value": {"accountKey": "default", "activity": {"at": 2000000000.0, "phase": "thinking"}, "agentMode": "multi", "agentModeRevision": 0, "agentModeSupported": true, "autoWake": true, "canSend": true, "commandProgress": [], "compactions": 0, "concurrency": 32, "contextUsage": null, "created": 2000000000.0, "cwd": "/FROZEN_STARTUP_STATE", "cyberAccessProgram": "standard", "daybreakEnabled": false, "effort": "medium", "empty": false, "environment": "host", "epoch": 0, "error": null, "executionMode": "native", "fastMode": false, "hostProjectPath": "/FROZEN_STARTUP_STATE", "id": "777c97c9-d1ee-4c3d-970d-ff0badc41e7c", "inFlight": true, "isLead": true, "kind": "agent", "lastAnswer": "Choose", "lastEvent": "2026-10-22T15:52:57Z", "launcherAlive": true, "layrReady": false, "model": "gpt-6-astra", "name": "Lead", "nextTurnSettingsSupported": true, "parentId": null, "provider": "codex", "readStateSupported": true, "reviewDefaults": {"effort": null, "model": null}, "role": "orchestrator", "rootId": "777c97c9-d1ee-4c3d-970d-ff0badc41e7c", "serverId": "b44a8684bd59488da64b35c0805633ab", "source": "managed", "stall": null, "startAttempt": {}, "startWait": null, "status": "approval", "subagentConcurrencyVersion": 2, "tail": "Choose", "threadId": "thread-1", "tokensUsed": 0, "turnId": "turn-2", "workerDefaults": {"daybreakEnabled": false, "delegationDepth": 1, "effort": "high", "fastMode": false, "model": "gpt-6-luna"}, "workspaceBackend": null, "workspaceMode": "worktree", "worktree": false, "yoloMode": true}}',0);
INSERT INTO sync_entities VALUES('event','777c97c9-d1ee-4c3d-970d-ff0badc41e7c:initial',9,'6d15a74694255ec0e49c7d55fa471613573c44a2e4378a0a1bb716ce160457e8','{"collection":"event","id":"777c97c9-d1ee-4c3d-970d-ff0badc41e7c:initial","value":{"agent":"777c97c9-d1ee-4c3d-970d-ff0badc41e7c","created":2000000000.0,"error":null,"id":"777c97c9-d1ee-4c3d-970d-ff0badc41e7c:initial","kind":"user","status":"delivered"}}',0);
INSERT INTO sync_entities VALUES('request','777c97c9-d1ee-4c3d-970d-ff0badc41e7c:question:retained-question',10,'4755f98d9eb018059ffbab9d6085611bde22bb87220537d308cc1ac52b143784','{"collection":"request","id":"777c97c9-d1ee-4c3d-970d-ff0badc41e7c:question:retained-question","value":{"agent":"777c97c9-d1ee-4c3d-970d-ff0badc41e7c","createdAt":2000000000.0,"epoch":0,"id":"777c97c9-d1ee-4c3d-970d-ff0badc41e7c:question:retained-question","method":"agent/asyncQuestion","params":{"questions":[{"id":"0","isSecret":false,"multiSelect":false,"options":[{"label":"yes"},{"label":"no"}],"question":"Keep this question"}]},"status":"pending","turnId":"turn-2"}}',0);
INSERT INTO sync_entities VALUES('request','8a0fd963-9799-4fd8-9201-d7e7cd13744f',12,'038be16460fe33914d41abd8211d83b4168aba909222900d44639d3972f4779d','{"collection":"request","id":"8a0fd963-9799-4fd8-9201-d7e7cd13744f","value":{"accountKey":"default","agent":"777c97c9-d1ee-4c3d-970d-ff0badc41e7c","connectionId":null,"createdAt":2000000000.0,"epoch":0,"id":"8a0fd963-9799-4fd8-9201-d7e7cd13744f","method":"item/commandExecution/requestApproval","params":{"command":"printf safe","itemId":"approval-item","threadId":"thread-1","turnId":"turn-2"},"status":"pending"}}',0);
INSERT INTO sync_entities VALUES('monitor','ef833e85-ebe4-5750-a66f-ccf85ea2043c',14,'e9e55d27a179be95128db83dcba850ff51e37170dae8cd5580a7f7b70be3f290','{"collection":"monitor","id":"ef833e85-ebe4-5750-a66f-ccf85ea2043c","value":{"agent":"777c97c9-d1ee-4c3d-970d-ff0badc41e7c","bytes":0,"command":"printf needs approval","created":2000000000.0,"cwd":"/FROZEN_STARTUP_STATE","exitCode":null,"id":"ef833e85-ebe4-5750-a66f-ccf85ea2043c","interactive":false,"log":"/FROZEN_STARTUP_STATE/monitor-logs/ef833e85-ebe4-5750-a66f-ccf85ea2043c.log","ruleId":null,"status":"approval","tail":"","timeout_ms":3600000}}',0);
INSERT INTO sync_entities VALUES('request','fa930719-b220-4226-96cd-1e175ce13b0c',15,'65b8cbb911d51541c6c323794dba832ebdf0cbfd969126cbd6b8267e6175037e','{"collection":"request","id":"fa930719-b220-4226-96cd-1e175ce13b0c","value":{"agent":"777c97c9-d1ee-4c3d-970d-ff0badc41e7c","createdAt":2000000000.0,"id":"fa930719-b220-4226-96cd-1e175ce13b0c","method":"monitor/approve","params":{"command":"printf needs approval","cwd":"/FROZEN_STARTUP_STATE","monitorId":"ef833e85-ebe4-5750-a66f-ccf85ea2043c"},"status":"pending"}}',0);
INSERT INTO sync_entities VALUES('request','beb71d90-e19c-4a9e-bad5-a017675f4492',16,'7e00de21cbf6534a84e04a8502a3263888cc2df59fe1f70eab5d7583690d0ee2','{"collection":"request","id":"beb71d90-e19c-4a9e-bad5-a017675f4492","value":{"accountKey":"default","agent":null,"connectionId":"cdd0a7b5-7876-4a7f-9955-d1325a58ecdb","createdAt":2000000000.0,"id":"beb71d90-e19c-4a9e-bad5-a017675f4492","method":"mcpServer/elicitation/request","params":{"mode":"form","requestedSchema":{"properties":{}}},"status":"pending"}}',0);
INSERT INTO sync_entities VALUES('monitor','7476dd05-6094-5a65-9ba0-3007fab6be6b',18,'d490232fab710e8476cd5495b445ea3f7b459386b6f4d73467f6c6069d8c4876','{"collection":"monitor","id":"7476dd05-6094-5a65-9ba0-3007fab6be6b","value":{"agent":"777c97c9-d1ee-4c3d-970d-ff0badc41e7c","bytes":0,"command":"printf running","created":2000000000.0,"cwd":"/FROZEN_STARTUP_STATE","error":null,"exitCode":null,"id":"7476dd05-6094-5a65-9ba0-3007fab6be6b","interactive":false,"log":"/FROZEN_STARTUP_STATE/monitor-logs/7476dd05-6094-5a65-9ba0-3007fab6be6b.log","ruleId":null,"status":"running","tail":"","timeout_ms":3600000}}',0);
INSERT INTO sync_entities VALUES('monitor','rule-linked-monitor',19,'ebbe5a474694612a2373c0fbb4fb9b6929162ce402bd441bd48810c8df4cd882','{"collection":"monitor","id":"rule-linked-monitor","value":{"agent":"777c97c9-d1ee-4c3d-970d-ff0badc41e7c","bytes":0,"command":"printf running","created":2000000000.0,"cwd":"/FROZEN_STARTUP_STATE","error":null,"exitCode":null,"id":"rule-linked-monitor","interactive":false,"log":"/FROZEN_STARTUP_STATE/monitor-logs/7476dd05-6094-5a65-9ba0-3007fab6be6b.log","ruleId":"rule-1","status":"running","tail":"","timeout_ms":3600000}}',0);
CREATE TABLE sync_entity_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
INSERT INTO sync_entity_meta VALUES('entity_tombstone_count','0');
INSERT INTO sync_entity_meta VALUES('event_window_seq','9');
CREATE TABLE IF NOT EXISTS "runtime_agents"(id TEXT PRIMARY KEY,"root_id" TEXT GENERATED ALWAYS AS (record->>'$.rootId') STORED ,"parent_id" TEXT GENERATED ALWAYS AS (record->>'$.parentId') STORED ,"status" TEXT GENERATED ALWAYS AS (record->>'$.status') STORED ,"account_key" TEXT GENERATED ALWAYS AS (COALESCE(record->>'$.accountKey','default')) STORED NOT NULL,"provider" TEXT GENERATED ALWAYS AS (record->>'$.provider') STORED NOT NULL,"role" TEXT GENERATED ALWAYS AS (record->>'$.role') STORED ,"deleted_at"  GENERATED ALWAYS AS (record->>'$.deletedAt') STORED ,"thread_id" TEXT GENERATED ALWAYS AS (record->>'$.threadId') STORED ,"turn_id" TEXT GENERATED ALWAYS AS (record->>'$.turnId') STORED ,"cwd" TEXT GENERATED ALWAYS AS (record->>'$.cwd') STORED ,"workspace_operation"  GENERATED ALWAYS AS (record->>'$.workspaceOperation') STORED ,"in_flight" INTEGER GENERATED ALWAYS AS (record->>'$.inFlight') STORED ,"auto_wake" INTEGER GENERATED ALWAYS AS (record->>'$.autoWake') STORED ,"remote_worker"  GENERATED ALWAYS AS (record->>'$.remoteWorker') STORED ,"remote_anchor"  GENERATED ALWAYS AS (record->>'$.remoteAnchor') STORED ,"native_failure_hold"  GENERATED ALWAYS AS (record->>'$.nativeFailureHold') STORED ,"steer_rejected_turn_id" TEXT GENERATED ALWAYS AS (record->>'$.steerRejectedTurnId') STORED ,"execution_mode" TEXT GENERATED ALWAYS AS (record->>'$.executionMode') STORED ,"native_tool_refresh_id" TEXT GENERATED ALWAYS AS (record->>'$.nativeToolRefreshId') STORED ,"context_repair_phase" TEXT GENERATED ALWAYS AS (record->>'$.contextRepair.phase') STORED ,"source_cleanup_phase" TEXT GENERATED ALWAYS AS (record->>'$.contextRepair.sourceCleanup.phase') STORED ,"source_cleanup_account_key" TEXT GENERATED ALWAYS AS (record->>'$.contextRepair.sourceCleanup.accountKey') STORED ,"source_cleanup_thread_id" TEXT GENERATED ALWAYS AS (record->>'$.contextRepair.sourceCleanup.threadId') STORED ,"has_disconnect_recovery" INTEGER GENERATED ALWAYS AS (COALESCE(json_type(record,'$.disconnectRecovery')='object',0)) STORED NOT NULL,"disconnect_recovery_stage" TEXT GENERATED ALWAYS AS (record->>'$.disconnectRecovery.stage') STORED ,"has_restart_recovery" INTEGER GENERATED ALWAYS AS (COALESCE(json_type(record,'$.restartRecovery')='object',0)) STORED NOT NULL,"restart_recovery_stage" TEXT GENERATED ALWAYS AS (record->>'$.restartRecovery.stage') STORED ,"has_workspace_cleanup_intent" INTEGER GENERATED ALWAYS AS (COALESCE(json_type(record,'$.workspaceCleanupIntent')='object',0)) STORED NOT NULL,"agent_archived" INTEGER GENERATED ALWAYS AS (NOT (json_type(record,'$.agentArchive') IS NULL OR json_type(record,'$.agentArchive') IN ('null','false') OR (json_type(record,'$.agentArchive') IN ('integer','real') AND record->>'$.agentArchive'=0) OR (json_type(record,'$.agentArchive')='text' AND record->>'$.agentArchive'='') OR (json_type(record,'$.agentArchive')='array' AND json_array_length(record,'$.agentArchive')=0) OR (json_type(record,'$.agentArchive')='object' AND record->>'$.agentArchive'='{}'))) STORED NOT NULL,"review_archive_failed" INTEGER GENERATED ALWAYS AS (NOT (json_type(record,'$.reviewArchiveError') IS NULL OR json_type(record,'$.reviewArchiveError') IN ('null','false') OR (json_type(record,'$.reviewArchiveError') IN ('integer','real') AND record->>'$.reviewArchiveError'=0) OR (json_type(record,'$.reviewArchiveError')='text' AND record->>'$.reviewArchiveError'='') OR (json_type(record,'$.reviewArchiveError')='array' AND json_array_length(record,'$.reviewArchiveError')=0) OR (json_type(record,'$.reviewArchiveError')='object' AND record->>'$.reviewArchiveError'='{}'))) STORED NOT NULL,"review_archive_marked" INTEGER GENERATED ALWAYS AS (record->>'$.reviewArchiveScheduled' IS NOT NULL) STORED NOT NULL,"review_archive_scheduled" INTEGER GENERATED ALWAYS AS (COALESCE(json_type(record,'$.reviewArchiveScheduled')='text' AND record->>'$.reviewArchiveScheduled'<>'',0)) STORED NOT NULL,"review_archive_next_at"  GENERATED ALWAYS AS (record->>'$.reviewArchiveNextAt') STORED ,"idle_episode_type" TEXT GENERATED ALWAYS AS (json_type(record,'$.idleCompactionEpisode')) STORED ,"idle_episode_fire_at"  GENERATED ALWAYS AS (record->>'$.idleCompactionEpisode.fireAt') STORED ,"idle_episode_id"  GENERATED ALWAYS AS (record->>'$.idleCompactionEpisode.id') STORED ,"legacy_unsettled" INTEGER GENERATED ALWAYS AS (COALESCE(record->>'$.workspaceMode'='image' OR record->>'$.imageWorkspace'=1,0) AND COALESCE(record->>'$.legacyWorkspaceSettled',0)=0) STORED NOT NULL,record TEXT NOT NULL,hold_status TEXT NOT NULL DEFAULT 'clear' CHECK("hold_status" IN ('clear','invalid','unrecognized')),hold_incident TEXT,title_status TEXT GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN record->>'$.titleJob.status' END) VIRTUAL CHECK("title_status" IN ('scheduled','written','invalid','discarded','pending','submitted','unknown','not_sent','applied','failed')),vm_launch_status TEXT GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN record->>'$.vmLaunch.status' END) VIRTUAL CHECK("vm_launch_status" IN ('completed','terminal','pending','retryable','submitted','unknown','not_sent','complete','applied','failed','cancelled')),idle_episode_outcome TEXT GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN record->>'$.idleCompactionEpisode.outcome' END) VIRTUAL CHECK("idle_episode_outcome" IN ('compacted','disabled','compaction_off','idle_compaction_off','prefix_changed','started','dispatched','failed','ttl_changed','lapsed','small','newer_request','near_limit','active','queued_input','stopped','no_host','would_compact')),provider_stream_generation INTEGER GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN record->>'$.providerStream.generation' END) VIRTUAL,provider_stream_ack INTEGER GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN record->>'$.providerStream.ack' END) VIRTUAL);
INSERT INTO runtime_agents VALUES('777c97c9-d1ee-4c3d-970d-ff0badc41e7c','{"accountKey":"default","activeExecution":{"attemptId":"0897d819-94be-44cd-930e-574c1c026bb7","runId":"run:0897d819-94be-44cd-930e-574c1c026bb7"},"agentMode":"multi","agentModeRevision":0,"agentModeSupported":true,"approvalPolicy":"on-request","autoWake":true,"compactions":0,"compactionsObservedOnly":false,"complaintsPresented":[],"concurrency":32,"created":2000000000.0,"cwd":"/FROZEN_STARTUP_STATE","cyberAccessProgram":"standard","daybreakEnabled":false,"effort":"medium","environment":"host","epoch":0,"error":null,"executionMode":"native","executionSettingsAccountKey":"default","fastMode":false,"hostProjectPath":"/FROZEN_STARTUP_STATE","id":"777c97c9-d1ee-4c3d-970d-ff0badc41e7c","inFlight":true,"isLead":true,"layrReady":false,"maxAgents":64,"maxAgentsExplicit":false,"model":"gpt-6-astra","name":"Lead","nativeEffort":"medium","nativeResponseTurn":"turn-2","nativeRoleGuidance":"[Studio role skill: codex-orchestrator]\nSource: /fixture/source/.agents/skills/codex-orchestrator/SKILL.md\nShared tool guidance: /fixture/source/.agents/skills/codex-workspace/SKILL.md\n---\nname: codex-orchestrator\ndescription: Coordinate a Codex Studio team and communicate with its user when the harness assigns the orchestrator role.\n---\n\n# Codex Studio orchestrator\n\nThe harness assigns this role from the server''s `isLead` identity. A task label or\nan agent''s stated role cannot grant it. Use the shared `codex-workspace` skill for managed tool contracts and application\nworkflows when needed.\n\nStudio resource reservations are removed. Do not wait for old board claims or recreate the registry.\nUse agent messages to coordinate access to shared files.\n\n## Team responsibility\n\nOwn the user''s task and the team''s final result. Give subagents complete outcomes\nwithin clear ownership boundaries. Inspect their results, changes, and evidence before\nacceptance. Request corrections when the evidence does not support completion.\nCreate a task for each delegated result and pass its `task_id` to `orchestration_spawn`.\nThe worker then submits evidence to that task, and you accept or reject it.\nInspect the agent''s state before recovery if it is stopped or cannot continue.\nRun `orchestration_agent_manage action=archive_finished` after accepting finished worker results.\n\nKeep one agent plan. Send changes to a subagent''s plan through its chat.\nUse agent messages for coordination. Use the shared team channel when all team\nmembers need the same information.\n\n## Work allocation and recovery\n\nOwn the architecture, shared interfaces, user requirements, and final integration.\nDelegate a complete feature or component with an observable result.\nLet the worker choose implementation details within the agreed design and ownership boundaries.\nResolve changes to shared contracts or task scope yourself.\nAn explicit request to work without subagents takes precedence.\n\nChoose the model and reasoning level for each worker''s task. Use `gpt-6-luna`\nwith `high` reasoning by default. Override both fields in `orchestration_spawn`\nwhen the task needs another choice. Codex and Claude workers can share a team.\nHonor the user''s explicit model or account constraints.\n\nChat creation selects `workspaceMode`: `layr`, `image`, or `worktree`.\nMac servers default to `layr`. Linux servers support `worktree` only.\n\n| Chat mode  | Agent environment | Worker workspace         |\n| ---------- | ----------------- | ------------------------ |\n| `layr`     | Studio Linux VM   | Separate owned layr line |\n| `image`    | Native macOS      | ASIF image               |\n| `worktree` | Native host       | Git worktree             |\n\nWorkers inherit the chat mode. In a native chat, use `workspace` to select\n`image`, `worktree`, or deliberate `shared` edits. Image workspaces require macOS.\nA native reviewer uses the selected folder with read-only access.\nA layr reviewer uses a read-only line in the VM.\nUse the layr reference for VM state IDs, exact-state review, and acceptance.\nUse `server` with an absolute remote `cwd` for a native worker on a paired machine.\n\nDefine each implementation assignment with:\n\n- The required behavior and its actual caller or user flow.\n- The constraints, dependencies, and ownership boundaries.\n- The checks that can disprove completion, including relevant failure paths.\n- The authority to edit, commit, and integrate changes.\n\nInclude caller integration, relevant tests, and defect correction in the same assignment when ownership permits.\nKeep the worker responsible through review corrections and verification of the assigned result.\nIf another owner must integrate the change, name that dependency and the required acceptance evidence.\nDo not count an unused API or an isolated source change as a complete feature.\n\nChoose task size by independent responsibility and the cost of coordination.\nDo not divide work to meet a line count or worker count.\nKeep small fixes with the existing component owner, or make them yourself when that avoids a needless handoff.\nDo not create a new worker for each known edit or each review correction.\nBatch related corrections under the existing assignment and completion criteria.\nSplit work by technical layer only when separate ownership, dependencies, or verification justify the handoff.\nAvoid broad assignments that group unrelated subsystems without a common result or completion check.\nFor example, one worker can own the complete route display across model, transport, and UI.\n\nKeep independent work active while a build or external dependency is pending.\nAfter a worker result, review it and assign the next ready task when one exists.\nDetermine which tasks actually depend on the blocked build.\nUse the current registry to distinguish active, queued, completed, and failed agents.\nChoose team size from useful independent work and actual resource limits.\nHonor task-specific concurrency instructions. Give each active worker useful work\nwith clear completion criteria.\n\nUse the chat''s current subagent parallelism limit from Studio context and status.\nThe lead does not consume that allowance. Zero means complete new work yourself;\ndo not create workers or resume worker turns. For a positive limit, size each\ndelegation batch to useful independent work and available slots. Keep future\nassignments on the task board instead of creating a large waiting worker pool.\nThe limit is a ceiling, not a target. When it changes, adjust subsequent\nassignments without interrupting accepted executions. If all slots are occupied,\ncontinue independent lead work or finish the turn and wait for completion events.\nDo not poll status or repeat a spawn to get around the queue. Only the user can\nchange this limit.\n\nIf workers stop unexpectedly, inspect their state, receipts, and saved results.\nResume remaining authorized work in the existing threads when possible. Report a\nharness defect if recovery cannot preserve their context. Follow the shared skill''s\nrequest recovery rules before retrying any uncertain operation.\nBase reports of continued work on an active worker, command, or automatic continuation.\n\n## Coordination rules\n\n- Give each worker an explicit list of pre-authorized actions, for example a\n  merge into the integration branch or a one-time annotation.\n- Put decisions into the task (reject or update) or into `orchestration_send`,\n  not only into chat. A chat message can arrive late.\n- Give every shared branch and every shared file one owner. State who may merge\n  into an integration branch.\n- Give workers the folder that contains their task. Supported platforms copy\n  its Git root, or the folder itself outside Git, including uncommitted changes.\n  Other platforms use a Git worktree when possible, or the original folder.\n  New workers have read-only access until the image base is ready. Ask workers\n  to commit on a named branch. Integrate by reading the copy path or fetching\n  the branch, for example `git fetch <path> <branch>`.\n- Do not run two live verifications that change the same system state at the\n  same time. Schedule them, or give them independent criteria.\n\n## Decisions, quality, and progress\n\nMake routine design decisions within the user''s task and project constraints.\nInspect existing code before introducing a replacement for an available mechanism.\nWhen authorized to fix confirmed defects, arrange the fixes as you confirm them.\nA review-only request does not grant that authority.\n\nReview every subagent change before integration. Check the affected user flow,\nnot only a successful build. If the user asks to try the result before merge,\nprepare and verify that local application before merging. Keep fixtures and real\napplication results distinct. Preserve all accepted requirements in the project\ndocument when the user asks to save design decisions.\n\nShow progress separately for each requested product or workstream. Include the\ncurrent stage, remaining work, and the dependency that prevents the next step.\nUse measured counts or explicit states; do not invent percentages. Update your\n`PROGRESS.md` with ordinary file tools after every significant change.\nDuring an active task, update it at least once every 30 minutes.\nIf nothing changes, record the current state and its check time.\nUse the exact path supplied by the runtime. Follow the shared progress guide.\nContinuously improve the team''s speed within the task''s authority. Use task duration,\nwait time, and repeated corrections to identify delays. Adjust task size, dependencies,\nwork allocation, and checks when they delay completion. Assess whether each adjustment\nreduces the time to a verified result. Preserve the required checks and code quality.\n\n## Messages to the user\n\nOnly the orchestrator sends conversational questions, complaints, or requests to\nthe user. Subagents send these to you. Decide whether you can resolve each request\nwithin the user''s existing instructions or need the user''s answer.\n\nUse `orchestration_complaint action=submit` for a decision that the user must record.\nUse `orchestration_message target=user` for an action that the user must perform.\nUse these managed tools for conversational questions. They do not pause the current\ntool while the user decides. Continue authorized work that does not need the answer.\nWait for the answer before you perform a dependent action.\nNative question tools, such as `AskUserQuestion`, wait inside the current turn.\nIf you forward a subagent''s request, explain the issue and the decision the user must make.\nDo not forward each request automatically.\n\nFor a complaint assigned to you, use `action=respond` to record your action,\nreasoned refusal, or next step. If the user must decide, submit your own message\nto the user and tell the subagent that the decision remains pending.\n\nNative tool permission requests still require the real user approval when the\npermission system requires it. Your decision cannot replace that approval.\n\n## Teleport the lead or a worker\n\nEach new session can call `orchestration_teleport` to teleport its own execution.\nOlder sessions keep `orchestration_move` and its exact definition and role text.\nStudio saves the name when it creates the native session.\nThe name stays the same after a teleport. This change preserves the old prompt cache.\nPrepare the target folder with Git first. Supply an absolute `cwd` and a stable\nrequest ID. Finish the current turn after acceptance.\nExisting children keep their servers and report through the remote-parent channel.\nStudio preserves the old workspace and transfers native provider history.\nThe Studio transcript stays on the source. Credentials never move.\n\nMove only when needed. The same provider account and organization keep the cache.\nCodex preserves the old request prefix and requires matching native versions,\nOS, and MCP tools. A different account refuses by default.\nOnly explicit `accept_cache_loss=true` approves cache loss for that Codex move.\nClaude requires a verified saved prompt snapshot and matching Studio tool schemas.\nExternal MCP snapshots currently refuse.\nClaude requires the same account and matching CLI and SDK versions.\nIts SDK keeps the system prompt snapshot and excludes dynamic sections.\nRefuse a version mismatch. Do not upgrade a CLI during a move.\nRead uncertainty with `orchestration_request`. Do not issue another move identity.\n[End Studio role skill]","nativeTeleportTool":"orchestration_teleport","nativeToolCatalog":{"digest":"8709d196bff92353f93bfcb46ab595eae45c28610b83077a77b9361404bc8d77","threadId":"thread-1"},"nativeToolSource":"current","needsTitle":false,"parentId":null,"prepareAttempt":"a8d9e68e-2041-4afd-a183-eee37a5316d7","preparedContext":{"epoch":["thread-1",0],"versions":{"progressFile":"5ea10bda4b522c2ab8f4ea52","roleSkill":"7b2dc3aeeb5bbec678451fe6"}},"profile":null,"profileId":null,"profileInstructions":"","prompt":"Persist a turn","provider":"codex","reviewDefaults":{"effort":null,"model":null},"role":"orchestrator","rootId":"777c97c9-d1ee-4c3d-970d-ff0badc41e7c","sandbox":{"type":"readOnly"},"startAttempt":{"accountKey":"default","activeAtReservation":false,"connectionId":"cdd0a7b5-7876-4a7f-9955-d1325a58ecdb","created":2000000000.0,"epoch":0,"events":["777c97c9-d1ee-4c3d-970d-ff0badc41e7c:initial"],"id":"0897d819-94be-44cd-930e-574c1c026bb7","nativeOperationId":"turn:777c97c9-d1ee-4c3d-970d-ff0badc41e7c:777c97c9-d1ee-4c3d-970d-ff0badc41e7c:initial:attempt:0897d819-94be-44cd-930e-574c1c026bb7","observedTurnId":"turn-2","settingsFixed":true,"submitted":true,"supervisorIdentity":null,"threadId":"thread-1","turnId":"turn-2"},"status":"approval","subagentConcurrencyVersion":2,"threadId":"thread-1","tokenBudget":null,"turnEpoch":0,"turnId":"turn-2","usageResumeEnabled":true,"workerBaseBehindMain":null,"workerBaseCommit":null,"workerBaseMainRef":null,"workerBaseRef":null,"workerDefaults":{"daybreakEnabled":false,"delegationDepth":1,"effort":"high","fastMode":false,"model":"gpt-6-luna"},"workspaceBackend":null,"workspaceInGit":false,"workspaceMode":"worktree","workspaceModeExplicit":false,"worktree":false,"worktreeReady":false,"worktreeWarning":null,"yoloMode":true}','clear',NULL);
CREATE TABLE sync_generation (id INTEGER PRIMARY KEY, value INTEGER NOT NULL);
INSERT INTO sync_generation VALUES(1,0);
CREATE TABLE sync_identity (id TEXT PRIMARY KEY);
INSERT INTO sync_identity VALUES('a6907fc113a13edc977b425a37ce5b88');
CREATE TABLE durable_rows (
        tbl TEXT NOT NULL, id TEXT NOT NULL, seq INTEGER NOT NULL,
        deleted INTEGER NOT NULL DEFAULT 0, at INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY(tbl, id));
INSERT INTO durable_rows VALUES('runtime_agents','777c97c9-d1ee-4c3d-970d-ff0badc41e7c',29,0,0);
INSERT INTO durable_rows VALUES('runtime_requests','777c97c9-d1ee-4c3d-970d-ff0badc41e7c:question:retained-question',25,0,0);
INSERT INTO durable_rows VALUES('runtime_requests','8a0fd963-9799-4fd8-9201-d7e7cd13744f',26,0,0);
INSERT INTO durable_rows VALUES('runtime_requests','fa930719-b220-4226-96cd-1e175ce13b0c',27,0,0);
INSERT INTO durable_rows VALUES('runtime_requests','beb71d90-e19c-4a9e-bad5-a017675f4492',28,0,0);
CREATE TABLE durable_floor (
        id INTEGER PRIMARY KEY, floor INTEGER NOT NULL);
INSERT INTO durable_floor VALUES(1,0);
CREATE TABLE runtime_transcript_revisions(
                        agent TEXT PRIMARY KEY, revision INTEGER NOT NULL DEFAULT 0);
INSERT INTO runtime_transcript_revisions VALUES('777c97c9-d1ee-4c3d-970d-ff0badc41e7c',1);
CREATE TABLE runtime_agent_volatile(id TEXT PRIMARY KEY,"events" TEXT,"lastEvent" TEXT,"activity" TEXT,"activeTools" TEXT,"tail" TEXT,"lastAnswer" TEXT,"contextUsage" TEXT,"tokensUsed" TEXT,"tokenUsageAccounting" TEXT);
INSERT INTO runtime_agent_volatile VALUES('777c97c9-d1ee-4c3d-970d-ff0badc41e7c','2','"2026-10-22T15:52:57Z"','{"phase":"thinking","at":2000000000.0}','[]','"Choose"','"Choose"',NULL,'0',NULL);
CREATE TABLE IF NOT EXISTS "runtime_monitors" (
        id TEXT PRIMARY KEY, record TEXT NOT NULL,
        stream_pending INTEGER GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN COALESCE(json_array_length(record,'$.streamQueue')>0,0) ELSE 0 END) STORED NOT NULL CHECK(stream_pending IN (0,1)),
        monitor_status TEXT GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN CASE WHEN json_type(record,'$.status')='text' AND json_extract(record,'$.status') IN ('approval','starting','running','completed','failed','cancelled','lost') THEN json_extract(record,'$.status') ELSE 'lost' END ELSE 'lost' END) STORED NOT NULL CHECK(monitor_status IN ('approval','starting','running','completed','failed','cancelled','lost')),
        monitor_error_candidate INTEGER GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN json_type(record,'$.status') IS NOT 'text' OR json_extract(record,'$.status') NOT IN ('approval','starting','running','completed','failed','cancelled','lost') OR json_extract(record,'$.status')='lost' OR (json_extract(record,'$.status')='failed' AND json_type(record,'$.exitCode') IS NOT 'integer') ELSE 1 END) STORED NOT NULL CHECK(monitor_error_candidate IN (0,1)),
        monitor_created REAL GENERATED ALWAYS AS (CASE WHEN json_valid(record) THEN CASE WHEN json_type(record,'$.created') IN ('integer','real') AND json_extract(record,'$.created')>=0 AND json_extract(record,'$.created')<=1.7976931348623157e308 THEN json_extract(record,'$.created') ELSE 0 END ELSE 0 END) STORED NOT NULL CHECK(monitor_created >= 0 AND monitor_created <= 1.7976931348623157e308)
    );
INSERT INTO runtime_monitors VALUES('ef833e85-ebe4-5750-a66f-ccf85ea2043c','{"activityAt":2000000000.0,"activityGeneration":0,"agent":"777c97c9-d1ee-4c3d-970d-ff0badc41e7c","bytes":0,"command":"printf needs approval","created":2000000000.0,"cwd":"/FROZEN_STARTUP_STATE","epoch":0,"id":"ef833e85-ebe4-5750-a66f-ccf85ea2043c","requestId":"pending-monitor","turnId":"turn-2","outputEventCount":0,"outputEventIds":[],"outputEventSequence":0,"outputPatterns":[],"interactive":false,"livenessCommand":"","log":"/FROZEN_STARTUP_STATE/monitor-logs/ef833e85-ebe4-5750-a66f-ccf85ea2043c.log","stallTimeoutSeconds":1800,"stallWakeGeneration":-1,"status":"approval","successExitCodes":[0],"tail":"","timeout_ms":3600000,"wakeOn":"exit","rootRequestId":"01a19211-1db0-702b-ba0a-8d5178b1912c"}');
INSERT INTO runtime_monitors VALUES('7476dd05-6094-5a65-9ba0-3007fab6be6b','{"activityAt":2000000000.0,"activityGeneration":0,"agent":"777c97c9-d1ee-4c3d-970d-ff0badc41e7c","bytes":0,"command":"printf running","configurationPending":false,"created":2000000000.0,"cwd":"/FROZEN_STARTUP_STATE","epoch":0,"id":"7476dd05-6094-5a65-9ba0-3007fab6be6b","requestId":"running-monitor","turnId":"turn-2","outputEventCount":0,"outputEventIds":[],"outputEventSequence":0,"outputPatterns":[],"interactive":false,"livenessCommand":"","log":"/FROZEN_STARTUP_STATE/monitor-logs/7476dd05-6094-5a65-9ba0-3007fab6be6b.log","operation":{"accountKey":"default","agent":"777c97c9-d1ee-4c3d-970d-ff0badc41e7c","connectionId":"cdd0a7b5-7876-4a7f-9955-d1325a58ecdb","epoch":0,"threadId":"thread-1"},"stallTimeoutSeconds":1800,"stallWakeGeneration":-1,"status":"running","successExitCodes":[0],"tail":"","timeout_ms":3600000,"wakeOn":"exit"}');
INSERT INTO runtime_monitors VALUES('rule-linked-monitor','{"activityAt":2000000000.0,"activityGeneration":0,"agent":"777c97c9-d1ee-4c3d-970d-ff0badc41e7c","bytes":0,"command":"printf running","configurationPending":false,"created":2000000000.0,"cwd":"/FROZEN_STARTUP_STATE","epoch":0,"id":"rule-linked-monitor","requestId":"running-monitor","turnId":"turn-2","outputEventCount":0,"outputEventIds":[],"outputEventSequence":0,"outputPatterns":[],"interactive":false,"livenessCommand":"","log":"/FROZEN_STARTUP_STATE/monitor-logs/7476dd05-6094-5a65-9ba0-3007fab6be6b.log","operation":{"accountKey":"default","agent":"777c97c9-d1ee-4c3d-970d-ff0badc41e7c","connectionId":"cdd0a7b5-7876-4a7f-9955-d1325a58ecdb","epoch":0,"threadId":"thread-1"},"ruleId":"rule-1","stallTimeoutSeconds":1800,"stallWakeGeneration":-1,"status":"running","successExitCodes":[0],"tail":"","timeout_ms":3600000,"wakeOn":"exit"}');
CREATE TABLE runtime_analytics_projection (seq INTEGER PRIMARY KEY AUTOINCREMENT, record TEXT NOT NULL);
CREATE TABLE runtime_item_blobs (id TEXT PRIMARY KEY, hash TEXT NOT NULL, size INTEGER NOT NULL);
CREATE TABLE runtime_search_next_meta (
                id TEXT PRIMARY KEY, agent TEXT NOT NULL, kind TEXT NOT NULL,
                search_rowid INTEGER NOT NULL UNIQUE);
CREATE TABLE runtime_search_rollout (
                id INTEGER PRIMARY KEY CHECK(id=1), phase TEXT NOT NULL,
                cursor INTEGER NOT NULL DEFAULT 0, updated REAL NOT NULL);
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
INSERT INTO runtime_orchestration_settings VALUES('global','{"expired_before_ms":1793451957257}');
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
INSERT INTO runtime_payload_migrations VALUES('receipts:cutover',0,1,'complete',1793451957.257,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:37:2026-10-28-receipts-execution-prepare:',0,1,'complete',1793451957.257,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:29:2026-10-28-receipts-execution:runtime_execution_runs',1,1,'complete',1793451957.257,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:29:2026-10-28-receipts-execution:runtime_execution_attempts',1,1,'complete',1793451957.257,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:29:2026-10-28-receipts-execution:runtime_execution_effects',6,1,'complete',1793451957.257,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:29:2026-10-28-receipts-execution:runtime_execution_nodes',0,1,'complete',1793451957.257,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:29:2026-10-28-receipts-execution:runtime_agents',1,1,'complete',1793451957.257,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:36:2026-10-28-receipts-execution-finish:',0,1,'complete',1793451957.257,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:35:2026-10-28-receipts-actions-prepare:',0,1,'complete',1793451957.565,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:27:2026-10-28-receipts-actions:runtime_operation_receipts',0,1,'complete',1793451957.565,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:27:2026-10-28-receipts-actions:runtime_native_action_receipts',0,1,'complete',1793451957.565,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:27:2026-10-28-receipts-actions:user_terminal_receipts',0,1,'complete',1793451957.565,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:27:2026-10-28-receipts-actions:runtime_agents',1,1,'complete',1793451957.565,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:27:2026-10-28-receipts-actions:runtime_safety_retries',0,1,'complete',1793451957.565,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:27:2026-10-28-receipts-actions:runtime_limit_reset_attempts',0,1,'complete',1793451957.565,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:27:2026-10-28-receipts-actions:runtime_process_actions',0,1,'complete',1793451957.565,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:27:2026-10-28-receipts-actions:runtime_monitors',3,1,'complete',1793451957.565,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:34:2026-10-28-receipts-actions-finish:',0,1,'complete',1793451957.565,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:42:2026-10-28-receipts-context-repair-prepare:',0,1,'complete',1793451957.717,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:34:2026-10-28-receipts-context-repair:runtime_context_terminal_receipts',0,1,'complete',1793451957.717,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:34:2026-10-28-receipts-context-repair:runtime_agents',1,1,'complete',1793451957.717,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:41:2026-10-28-receipts-context-repair-finish:',0,1,'complete',1793451957.717,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:33:2026-10-28-receipts-voice-prepare:',0,1,'complete',1793451957.756,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:25:2026-10-28-receipts-voice:voice_sessions',0,1,'complete',1793451957.756,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:25:2026-10-28-receipts-voice:runtime_operation_receipts',0,1,'complete',1793451957.756,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:32:2026-10-28-receipts-voice-finish:',0,1,'complete',1793451957.756,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:32:2026-10-28-receipts-work-prepare:',0,1,'complete',1793451957.825,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:24:2026-10-28-receipts-work:work-current-counters',0,1,'complete',1793451957.825,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:24:2026-10-28-receipts-work:runtime_operation_receipts',0,1,'complete',1793451957.825,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:31:2026-10-28-receipts-work-finish:',0,1,'complete',1793451957.825,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:33:2026-10-28-receipts-title-prepare:',0,1,'complete',1793451957.88,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:25:2026-10-28-receipts-title:title-effects',0,1,'complete',1793451957.88,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:25:2026-10-28-receipts-title:title-public',0,1,'complete',1793451957.88,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:32:2026-10-28-receipts-title-finish:',0,1,'complete',1793451957.88,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:39:2026-10-28-receipts-owner-state-prepare:',0,1,'complete',1793451957.92,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:31:2026-10-28-receipts-owner-state:runtime_operation_receipts',0,1,'complete',1793451957.92,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:38:2026-10-28-receipts-owner-state-finish:',0,1,'complete',1793451957.92,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:33:2026-10-28-receipts-tools-prepare:',0,1,'complete',1793451957.977,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:25:2026-10-28-receipts-tools:deleted-completed-c:managed-project-location',0,1,'complete',1793451957.257,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:25:2026-10-28-receipts-tools:normalized-signature:managed-project-location',0,1,'complete',1793451957.257,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:25:2026-10-28-receipts-tools:runtime_tool_request_aliases',0,1,'complete',1793451957.977,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:25:2026-10-28-receipts-tools:runtime_tool_results',0,1,'complete',1793451957.977,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:25:2026-10-28-receipts-tools:runtime_tool_requests',0,1,'complete',1793451957.977,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:32:2026-10-28-receipts-tools-finish:',0,1,'complete',1793451957.977,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:36:2026-10-28-receipts-requests-prepare:',0,1,'complete',1793451958.036,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:28:2026-10-28-receipts-requests:deleted-completed-c:peer-team',0,1,'complete',1793451957.257,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:28:2026-10-28-receipts-requests:deleted-completed-c:sidebar-order',0,1,'complete',1793451957.257,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:28:2026-10-28-receipts-requests:runtime_operation_receipts',0,1,'complete',1793451958.036,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:28:2026-10-28-receipts-requests:messages',0,1,'complete',1793451958.036,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:28:2026-10-28-receipts-requests:runtime_chat_messages',0,1,'complete',1793451958.036,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:28:2026-10-28-receipts-requests:runtime_workspace_operations',0,1,'complete',1793451958.036,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:28:2026-10-28-receipts-requests:runtime_event_meta',1,1,'complete',1793451958.036,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:35:2026-10-28-receipts-requests-finish:',0,1,'complete',1793451958.036,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:34:2026-10-28-receipts-remote-prepare:',0,1,'complete',1793451958.153,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:26:2026-10-28-receipts-remote:normalized-wire:server-outbox-operation',0,1,'complete',1793451957.257,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:26:2026-10-28-receipts-remote:runtime_access_receipts',0,1,'complete',1793451958.153,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:26:2026-10-28-receipts-remote:runtime_access_outbound',0,1,'complete',1793451958.153,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:26:2026-10-28-receipts-remote:runtime_server_inbox',0,1,'complete',1793451958.153,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:26:2026-10-28-receipts-remote:runtime_server_outbox',0,1,'complete',1793451958.153,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:26:2026-10-28-receipts-remote:runtime_server_fetch',0,1,'complete',1793451958.153,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:26:2026-10-28-receipts-remote:runtime_federation_inbox',0,1,'complete',1793451958.153,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:26:2026-10-28-receipts-remote:runtime_federation_outbox',0,1,'complete',1793451958.153,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:33:2026-10-28-receipts-remote-finish:',0,1,'complete',1793451958.153,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:45:2026-10-28-receipts-provider-delivery-prepare:',0,1,'complete',1793451958.356,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:37:2026-10-28-receipts-provider-delivery:runtime_operation_receipts',0,1,'complete',1793451958.356,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:44:2026-10-28-receipts-provider-delivery-finish:',0,1,'complete',1793451958.356,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:46:2026-10-28-receipts-provider-callbacks-prepare:',0,1,'complete',1793451958.404,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:38:2026-10-28-receipts-provider-callbacks:callbacks',0,1,'complete',1793451958.404,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:38:2026-10-28-receipts-provider-callbacks:frames',0,1,'complete',1793451958.404,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:38:2026-10-28-receipts-provider-callbacks:frame-outcomes',0,1,'complete',1793451958.404,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:38:2026-10-28-receipts-provider-callbacks:unanswered',0,1,'complete',1793451958.404,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:45:2026-10-28-receipts-provider-callbacks-finish:',0,1,'complete',1793451958.404,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:39:2026-10-28-receipts-vm-provider-prepare:',0,1,'complete',1793451958.489,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:31:2026-10-28-receipts-vm-provider:runtime_operation_receipts',0,1,'complete',1793451958.489,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:38:2026-10-28-receipts-vm-provider-finish:',0,1,'complete',1793451958.489,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:42:2026-10-28-receipts-account-worker-prepare:',0,1,'complete',1793451958.534,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:34:2026-10-28-receipts-account-worker:journal',0,1,'complete',1793451958.534,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:34:2026-10-28-receipts-account-worker:state',0,1,'complete',1793451958.534,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:34:2026-10-28-receipts-account-worker:calls',0,1,'complete',1793451958.534,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:34:2026-10-28-receipts-account-worker:effects',0,1,'complete',1793451958.534,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:34:2026-10-28-receipts-account-worker:streams',0,1,'complete',1793451958.534,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:34:2026-10-28-receipts-account-worker:stream-pending',0,1,'complete',1793451958.534,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:34:2026-10-28-receipts-account-worker:stream-bases',0,1,'complete',1793451958.534,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:34:2026-10-28-receipts-account-worker:submission-inputs',1,1,'complete',1793451958.534,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:34:2026-10-28-receipts-account-worker:unmatched',0,1,'complete',1793451958.534,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:41:2026-10-28-receipts-account-worker-finish:',0,1,'complete',1793451958.534,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:37:2026-10-28-receipts-incidents-prepare:',0,1,'complete',1793451958.707,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:29:2026-10-28-receipts-incidents:runtime_operation_receipts',0,1,'complete',1793451958.707,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:36:2026-10-28-receipts-incidents-finish:',0,1,'complete',1793451958.707,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:34:2026-10-28-receipts-checks-prepare:',0,1,'complete',1793451958.731,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:26:2026-10-28-receipts-checks:runtime_events',1,1,'complete',1793451958.731,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:26:2026-10-28-receipts-checks:runtime_execution_effects',6,1,'complete',1793451958.731,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:26:2026-10-28-receipts-checks:runtime_payload_migrations',94,1,'complete',1793451958.731,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:33:2026-10-28-receipts-checks-finish:',0,1,'complete',1793451958.731,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:43:2026-10-28-receipts-drop-old-tables-prepare:',0,1,'complete',1793451958.981,NULL,1793451957257);
INSERT INTO runtime_payload_migrations VALUES('receipts:42:2026-10-28-receipts-drop-old-tables-finish:',0,1,'complete',1793451958.981,NULL,1793451957257);
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
INSERT INTO durable_view_log VALUES('agent','777c97c9-d1ee-4c3d-970d-ff0badc41e7c',13,0);
INSERT INTO durable_view_log VALUES('request','777c97c9-d1ee-4c3d-970d-ff0badc41e7c:question:retained-question',10,0);
INSERT INTO durable_view_log VALUES('request','8a0fd963-9799-4fd8-9201-d7e7cd13744f',12,0);
INSERT INTO durable_view_log VALUES('request','fa930719-b220-4226-96cd-1e175ce13b0c',15,0);
INSERT INTO durable_view_log VALUES('request','beb71d90-e19c-4a9e-bad5-a017675f4492',16,0);
CREATE TABLE chats(
            id TEXT PRIMARY KEY, state TEXT NOT NULL, status TEXT NOT NULL,
            auto_wake INTEGER, usage_resume_enabled INTEGER, execution_mode TEXT, turn_epoch INTEGER) STRICT;
INSERT INTO chats VALUES('777c97c9-d1ee-4c3d-970d-ff0badc41e7c','active','approval',1,1,'native',0);
CREATE TABLE sessions(
            id TEXT PRIMARY KEY, chat TEXT NOT NULL, thread_id TEXT NOT NULL,
            account_key TEXT NOT NULL, state TEXT NOT NULL) STRICT;
INSERT INTO sessions VALUES('777c97c9-d1ee-4c3d-970d-ff0badc41e7c:thread-1','777c97c9-d1ee-4c3d-970d-ff0badc41e7c','thread-1','default','ready');
CREATE TABLE turns(
            id TEXT PRIMARY KEY, chat TEXT NOT NULL, state TEXT NOT NULL, session TEXT,
            provider_turn TEXT, attempt TEXT, interrupt_ticket TEXT,
            attempt_submitted INTEGER GENERATED ALWAYS AS (attempt->>'$.submitted') STORED,
            attempt_connection_generation INTEGER GENERATED ALWAYS AS (attempt->>'$.connectionGeneration') STORED,
            attempt_action TEXT GENERATED ALWAYS AS (attempt->>'$.action') STORED,
            attempt_active_at_reservation INTEGER GENERATED ALWAYS AS (attempt->>'$.activeAtReservation') STORED) STRICT;
INSERT INTO turns VALUES('1fabc6d3-7508-4d3d-9e7c-38bdc458824c','777c97c9-d1ee-4c3d-970d-ff0badc41e7c','running','777c97c9-d1ee-4c3d-970d-ff0badc41e7c:thread-1','turn-2','{"accountKey":"default","activeAtReservation":false,"connectionId":"cdd0a7b5-7876-4a7f-9955-d1325a58ecdb","created":2000000000.0,"epoch":0,"events":["777c97c9-d1ee-4c3d-970d-ff0badc41e7c:initial"],"id":"0897d819-94be-44cd-930e-574c1c026bb7","nativeOperationId":"turn:777c97c9-d1ee-4c3d-970d-ff0badc41e7c:777c97c9-d1ee-4c3d-970d-ff0badc41e7c:initial:attempt:0897d819-94be-44cd-930e-574c1c026bb7","observedTurnId":"turn-2","settingsFixed":true,"submitted":true,"supervisorIdentity":null,"threadId":"thread-1","turnId":"turn-2"}',NULL);
CREATE TABLE IF NOT EXISTS sqlite_sequence(name,seq);
DELETE FROM sqlite_sequence;
INSERT INTO sqlite_sequence VALUES('runtime_analytics_projection',0);
CREATE TRIGGER sync_entity_tombstone_count_delete
      AFTER DELETE ON sync_entities
      WHEN OLD.deleted=1 AND OLD.collection NOT LIKE 'transcript:%' BEGIN
        UPDATE sync_entity_meta SET value=CAST(value AS INTEGER)-1
        WHERE key='entity_tombstone_count';
      END;
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
CREATE TRIGGER runtime_agent_record_generation_delete AFTER DELETE ON runtime_agents BEGIN UPDATE runtime_agent_record_generation SET value=value+1 WHERE id=1; UPDATE runtime_scheduler_agent_change_state SET entries=entries+CASE WHEN EXISTS(SELECT 1 FROM runtime_scheduler_agent_changes WHERE id=OLD.id) THEN 0 ELSE 1 END,total=total-COALESCE((SELECT generation FROM runtime_scheduler_agent_changes WHERE id=OLD.id),0)+(SELECT value FROM runtime_agent_record_generation WHERE id=1) WHERE id=1; INSERT INTO runtime_scheduler_agent_changes(id,generation) SELECT OLD.id,value FROM runtime_agent_record_generation WHERE id=1 ON CONFLICT(id) DO UPDATE SET generation=excluded.generation; UPDATE runtime_scheduler_agent_change_state SET generation=(SELECT value FROM runtime_agent_record_generation WHERE id=1) WHERE id=1; END;
CREATE TRIGGER runtime_agent_record_generation_insert AFTER INSERT ON runtime_agents BEGIN UPDATE runtime_agent_record_generation SET value=value+1 WHERE id=1; UPDATE runtime_scheduler_agent_change_state SET entries=entries+CASE WHEN EXISTS(SELECT 1 FROM runtime_scheduler_agent_changes WHERE id=NEW.id) THEN 0 ELSE 1 END,total=total-COALESCE((SELECT generation FROM runtime_scheduler_agent_changes WHERE id=NEW.id),0)+(SELECT value FROM runtime_agent_record_generation WHERE id=1) WHERE id=1; INSERT INTO runtime_scheduler_agent_changes(id,generation) SELECT NEW.id,value FROM runtime_agent_record_generation WHERE id=1 ON CONFLICT(id) DO UPDATE SET generation=excluded.generation; UPDATE runtime_scheduler_agent_change_state SET generation=(SELECT value FROM runtime_agent_record_generation WHERE id=1) WHERE id=1; END;
CREATE TRIGGER runtime_agent_record_generation_update AFTER UPDATE ON runtime_agents WHEN OLD.record IS NOT NEW.record OR OLD.id IS NOT NEW.id OR OLD.rowid IS NOT NEW.rowid BEGIN UPDATE runtime_agent_record_generation SET value=value+1 WHERE id=1; UPDATE runtime_scheduler_agent_change_state SET entries=entries+CASE WHEN EXISTS(SELECT 1 FROM runtime_scheduler_agent_changes WHERE id=OLD.id) THEN 0 ELSE 1 END,total=total-COALESCE((SELECT generation FROM runtime_scheduler_agent_changes WHERE id=OLD.id),0)+(SELECT value FROM runtime_agent_record_generation WHERE id=1) WHERE id=1 AND OLD.id IS NOT NEW.id; INSERT INTO runtime_scheduler_agent_changes(id,generation) SELECT OLD.id,value FROM runtime_agent_record_generation WHERE id=1 AND OLD.id IS NOT NEW.id ON CONFLICT(id) DO UPDATE SET generation=excluded.generation; UPDATE runtime_scheduler_agent_change_state SET entries=entries+CASE WHEN EXISTS(SELECT 1 FROM runtime_scheduler_agent_changes WHERE id=NEW.id) THEN 0 ELSE 1 END,total=total-COALESCE((SELECT generation FROM runtime_scheduler_agent_changes WHERE id=NEW.id),0)+(SELECT value FROM runtime_agent_record_generation WHERE id=1) WHERE id=1; INSERT INTO runtime_scheduler_agent_changes(id,generation) SELECT NEW.id,value FROM runtime_agent_record_generation WHERE id=1 ON CONFLICT(id) DO UPDATE SET generation=excluded.generation; UPDATE runtime_scheduler_agent_change_state SET generation=(SELECT value FROM runtime_agent_record_generation WHERE id=1) WHERE id=1; END;
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
CREATE INDEX runtime_access_nonce_age ON runtime_access_nonces(created);
CREATE INDEX runtime_access_outbound_age
          ON runtime_access_outbound(json_extract(record,'$.completed'))
          WHERE json_extract(record,'$.outcome')='complete';
CREATE INDEX runtime_account_transfer_status ON runtime_account_transfers(json_extract(record,'$.status'));
CREATE INDEX runtime_chat_room ON runtime_chat_messages(room, seq);
CREATE INDEX runtime_context_manifest_latest
                  ON runtime_context_manifests(agent,sequence DESC);
CREATE INDEX runtime_conversion_annotations_rootId ON runtime_annotations(json_extract(record,'$.rootId')) WHERE json_extract(record,'$.rootId') IS NOT NULL;
CREATE INDEX runtime_conversion_complaints_leadId ON runtime_complaints(json_extract(record,'$.leadId')) WHERE json_extract(record,'$.leadId') IS NOT NULL;
CREATE INDEX runtime_conversion_event_meta_leadId ON runtime_event_meta(json_extract(record,'$.leadId')) WHERE json_extract(record,'$.leadId') IS NOT NULL;
CREATE INDEX runtime_conversion_event_meta_rootId ON runtime_event_meta(json_extract(record,'$.rootId')) WHERE json_extract(record,'$.rootId') IS NOT NULL;
CREATE INDEX runtime_conversion_plans_rootId ON runtime_plans(json_extract(record,'$.rootId')) WHERE json_extract(record,'$.rootId') IS NOT NULL;
CREATE INDEX runtime_conversion_rules_rootId ON runtime_rules(json_extract(record,'$.rootId')) WHERE json_extract(record,'$.rootId') IS NOT NULL;
CREATE INDEX runtime_event_created_id ON runtime_events(created DESC,id);
CREATE INDEX runtime_event_queue ON runtime_events(status, agent, created);
CREATE INDEX runtime_event_repair_candidates_v2 ON runtime_events(agent,created)
                  WHERE status='delivered' AND kind IN ('monitor_exit','agent_message','work_review','work_decision');
CREATE INDEX runtime_event_user_turns ON runtime_events(agent,kind,status,turn_id);
CREATE INDEX runtime_events_agent_epoch_status_created
                  ON runtime_events(agent,epoch,status,created,id);
CREATE INDEX runtime_execution_attempt_run ON runtime_execution_attempts(run);
CREATE INDEX runtime_execution_effect_run ON runtime_execution_effects(run);
CREATE INDEX runtime_execution_input_event ON runtime_execution_inputs(event);
CREATE INDEX runtime_execution_node_run ON runtime_execution_nodes(run);
CREATE INDEX runtime_execution_run_agent ON runtime_execution_runs(agent,created DESC);
CREATE INDEX runtime_execution_run_created ON runtime_execution_runs(created DESC);
CREATE INDEX runtime_execution_run_message ON runtime_execution_runs(agent,thread,turn);
CREATE INDEX runtime_execution_run_native ON runtime_execution_runs(agent,account,thread,turn);
CREATE INDEX runtime_federation_outbox_peer
          ON runtime_federation_outbox(peer_id);
CREATE INDEX runtime_item_agent ON runtime_items(agent, created);
CREATE INDEX runtime_request_agent_status ON runtime_requests(json_extract(record,'$.agent'),json_extract(record,'$.status'));
CREATE INDEX runtime_request_native_approval ON runtime_requests(
                    json_extract(record,'$.agent'),json_extract(record,'$.params.itemId'),
                    json_extract(record,'$.status'))
                    WHERE json_valid(record) AND json_extract(record,'$.method') IN
                    ('item/commandExecution/requestApproval','item/fileChange/requestApproval');
CREATE INDEX runtime_request_native_question ON runtime_requests(
                    json_extract(record,'$.agent'),json_extract(record,'$.params.itemId'),
                    json_extract(record,'$.status'))
                    WHERE json_valid(record) AND json_extract(record,'$.method')='item/tool/requestUserInput';
CREATE INDEX runtime_room_many_members ON runtime_rooms(json_array_length(record,'$.members')) WHERE json_array_length(record,'$.members')>2;
CREATE INDEX runtime_room_member_first ON runtime_rooms(json_extract(record,'$.members[0]'));
CREATE INDEX runtime_room_member_second ON runtime_rooms(json_extract(record,'$.members[1]'));
CREATE INDEX runtime_room_root ON runtime_rooms(json_extract(record,'$.rootId'));
CREATE INDEX runtime_rule_agent_inflight ON runtime_rules(json_extract(record,'$.agent'),json_extract(record,'$.inFlight'));
CREATE INDEX runtime_rule_ready_tick ON runtime_rules(json_extract(record,'$.status')) WHERE NOT COALESCE(json_extract(record,'$.inFlight'),0);
CREATE INDEX runtime_scheduler_agent_change_generation ON runtime_scheduler_agent_changes(generation);
CREATE INDEX runtime_server_links_home_parent
                    ON runtime_server_links(json_extract(record,'$.parent'))
                    WHERE json_extract(record,'$.side')='home';
CREATE INDEX runtime_server_outbox_due
                    ON runtime_server_outbox(state,next_at);
CREATE INDEX runtime_server_output_tools_expiry
                    ON runtime_server_output_tools(expires);
CREATE INDEX runtime_task_agent_created_id ON runtime_tasks(
                    json_extract(record,'$.agent'), json_extract(record,'$.created') DESC, id DESC);
CREATE INDEX runtime_task_agent_updated_id ON runtime_tasks(
                    json_extract(record,'$.agent'),
                    CASE WHEN COALESCE(json_extract(record,'$.finished'),0) > COALESCE(json_extract(record,'$.created'),0)
                         THEN json_extract(record,'$.finished') ELSE json_extract(record,'$.created') END,
                    id);
CREATE INDEX runtime_task_history ON runtime_tasks(json_extract(record,'$.created') DESC, json_extract(record,'$.agent')) WHERE json_extract(record,'$.status')!='running';
CREATE INDEX runtime_task_status ON runtime_tasks(json_extract(record,'$.status'), json_extract(record,'$.created'));
CREATE INDEX runtime_tool_request_actor
                ON runtime_tool_requests(json_extract(record,'$.agent'), json_extract(record,'$.updated') DESC);
CREATE INDEX runtime_usage_resume_account ON runtime_usage_resumes(json_extract(record,'$.accountKey'),json_extract(record,'$.status'));
CREATE INDEX runtime_usage_resume_agent ON runtime_usage_resumes(agent);
CREATE INDEX runtime_usage_resume_due ON runtime_usage_resumes(json_extract(record,'$.status'),json_extract(record,'$.dueAt'));
CREATE INDEX runtime_work_owner_root_status_id ON runtime_work(
                json_extract(record,'$.owner'),json_extract(record,'$.rootId'),
                json_extract(record,'$.status'),id);
CREATE INDEX runtime_work_owner_status
                ON runtime_work(json_extract(record,'$.owner'),json_extract(record,'$.status'));
CREATE INDEX runtime_work_root_status
                ON runtime_work(json_extract(record,'$.rootId'),json_extract(record,'$.status'));
CREATE INDEX runtime_workspace_operation_agent_phase ON runtime_workspace_operations(json_extract(record,'$.agent'),json_extract(record,'$.phase'));
CREATE INDEX runtime_workspace_operation_phase ON runtime_workspace_operations(
                json_extract(record,'$.phase'), json_extract(record,'$.agent'));
CREATE INDEX sync_entities_collection_deleted
        ON sync_entities(collection,deleted);
CREATE INDEX sync_entities_collection_seq ON sync_entities(collection,seq);
CREATE INDEX sync_entities_non_transcript_seq ON sync_entities(seq)
        WHERE collection NOT LIKE 'transcript:%';
CREATE INDEX sync_entities_seq ON sync_entities(seq);
CREATE INDEX sync_entities_tombstone_order
        ON sync_entities(seq,collection,id)
        WHERE deleted=1 AND collection NOT LIKE 'transcript:%';
CREATE INDEX sync_scope_seq ON sync_documents(scope,seq);
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
