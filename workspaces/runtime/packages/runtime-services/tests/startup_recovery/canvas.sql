-- Frozen legacy SQLite schema and rows; see the generating commit for provenance.
PRAGMA journal_mode=WAL;
PRAGMA synchronous=FULL;
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
          record TEXT NOT NULL, PRIMARY KEY(server,request_id));
CREATE TABLE runtime_access_receipts (
          client TEXT NOT NULL, request_id TEXT NOT NULL, hash TEXT NOT NULL, session TEXT NOT NULL,
          status TEXT NOT NULL, record TEXT NOT NULL, PRIMARY KEY(client,request_id));
CREATE TABLE runtime_account_transfers(id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_agent_moves(id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_agent_record_generation (
                    id INTEGER PRIMARY KEY CHECK(id=1), value INTEGER NOT NULL);
CREATE TABLE runtime_agents (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_annotations (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_assets (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_budget (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_budget_usage (id TEXT PRIMARY KEY, agent TEXT NOT NULL, kind TEXT NOT NULL, tokens INTEGER NOT NULL, at REAL NOT NULL);
CREATE TABLE runtime_capacity_retries (id TEXT PRIMARY KEY, agent TEXT NOT NULL, record TEXT NOT NULL);
CREATE TABLE runtime_chat_messages (
                  seq INTEGER PRIMARY KEY AUTOINCREMENT, id TEXT UNIQUE NOT NULL,
                  room TEXT NOT NULL, sender TEXT NOT NULL, text TEXT NOT NULL,
                  created REAL NOT NULL, deliveries TEXT NOT NULL);
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
CREATE TABLE runtime_events (
                  id TEXT PRIMARY KEY, agent TEXT NOT NULL, kind TEXT NOT NULL,
                  text TEXT NOT NULL, status TEXT NOT NULL, created REAL NOT NULL,
                  epoch INTEGER NOT NULL, turn_id TEXT, error TEXT);
CREATE TABLE runtime_execution_attempts (id TEXT PRIMARY KEY, run TEXT NOT NULL, record TEXT NOT NULL);
CREATE TABLE runtime_execution_effects (id TEXT PRIMARY KEY, run TEXT NOT NULL, kind TEXT NOT NULL, reference TEXT NOT NULL, record TEXT NOT NULL);
CREATE TABLE runtime_execution_inputs (run TEXT NOT NULL, event TEXT NOT NULL, PRIMARY KEY(run,event));
CREATE TABLE runtime_execution_nodes (id TEXT PRIMARY KEY, run TEXT NOT NULL, record TEXT NOT NULL);
CREATE TABLE runtime_execution_runs (id TEXT PRIMARY KEY, agent TEXT NOT NULL, account TEXT NOT NULL, epoch INTEGER NOT NULL, thread TEXT, turn TEXT, created REAL NOT NULL, record TEXT NOT NULL);
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
CREATE TABLE runtime_rules (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_scheduler_agent_change_state (id INTEGER PRIMARY KEY CHECK(id=1),floor INTEGER NOT NULL,generation INTEGER NOT NULL,entries INTEGER NOT NULL,total INTEGER NOT NULL);
CREATE TABLE runtime_scheduler_agent_changes (id TEXT PRIMARY KEY,generation INTEGER NOT NULL CHECK(typeof(generation)='integer' AND generation>=0));
CREATE VIRTUAL TABLE runtime_search USING fts5(id UNINDEXED,agent UNINDEXED,kind UNINDEXED,body,tokenize='unicode61');
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
CREATE TABLE runtime_sidebar_order (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_supervisor_cursor (handle TEXT PRIMARY KEY, sequence INTEGER NOT NULL, epoch TEXT);
CREATE TABLE runtime_tasks (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_tool_request_aliases
                (agent TEXT NOT NULL, alias TEXT NOT NULL, request TEXT NOT NULL, PRIMARY KEY(agent,alias,request));
CREATE TABLE runtime_tool_requests (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_tool_results (id TEXT PRIMARY KEY, result TEXT NOT NULL);
CREATE TABLE runtime_turn_item_link_state_v1 (id INTEGER PRIMARY KEY CHECK(id=1),epoch TEXT NOT NULL,source_rootpage INTEGER NOT NULL,cursor INTEGER,target INTEGER,complete INTEGER NOT NULL CHECK(complete IN (0,1)),scanned_rows INTEGER NOT NULL,source_bytes INTEGER NOT NULL,updated REAL NOT NULL);
CREATE TABLE runtime_turn_item_links_v1 (id TEXT PRIMARY KEY,agent TEXT NOT NULL,turn_id,created REAL NOT NULL,epoch TEXT NOT NULL);
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
CREATE TABLE sync_entity_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE sync_versions (
        seq INTEGER PRIMARY KEY, scope TEXT NOT NULL UNIQUE,
        hash TEXT NOT NULL, deleted INTEGER NOT NULL, updated REAL NOT NULL);
INSERT INTO "runtime_agent_record_generation" VALUES(1,12);
INSERT INTO "runtime_agents" VALUES('777c97c9-d1ee-4c3d-970d-ff0badc41e7c','{"id": "777c97c9-d1ee-4c3d-970d-ff0badc41e7c", "threadId": "thread-1", "accountKey": "default", "executionSettingsAccountKey": "default", "provider": "codex", "yoloMode": true, "name": "Lead", "prompt": "Persist a turn", "cwd": "/FROZEN_STARTUP_STATE", "role": "orchestrator", "isLead": true, "needsTitle": false, "parentId": null, "rootId": "777c97c9-d1ee-4c3d-970d-ff0badc41e7c", "subagentConcurrencyVersion": 2, "maxAgentsExplicit": false, "model": "gpt-6-astra", "effort": "medium", "fastMode": false, "daybreakEnabled": false, "cyberAccessProgram": "standard", "concurrency": 32, "maxAgents": 64, "tokenBudget": null, "status": "approval", "autoWake": true, "epoch": 0, "turnId": "turn-2", "inFlight": true, "turnEpoch": 0, "usageResumeEnabled": true, "tokensUsed": 0, "compactions": 0, "compactionsObservedOnly": false, "contextUsage": null, "events": 2, "created": 2000000000.0, "error": null, "profileId": null, "profileInstructions": "", "tail": "Choose", "worktree": false, "workspaceMode": "worktree", "executionMode": "native", "hostProjectPath": "/FROZEN_STARTUP_STATE", "layrReady": false, "workspaceBackend": null, "workspaceModeExplicit": false, "workspaceInGit": false, "worktreeReady": false, "environment": "host", "imageWorkspace": false, "imageWorkspaceReady": false, "imageWorkspacePhase": null, "imageWorkspaceRepo": null, "imageWorkspaceSubpath": ".", "imageWorkspaceBaseRef": null, "imageWorkspaceHasGit": false, "imageWorkspaceCreatedAt": null, "imageWorkspaceError": null, "worktreeWarning": null, "workerBaseRef": null, "workerBaseCommit": null, "workerBaseBehindMain": null, "workerBaseMainRef": null, "workerDefaults": {"model": "gpt-6-luna", "effort": "high", "fastMode": false, "daybreakEnabled": false, "delegationDepth": 1}, "reviewDefaults": {"model": null, "effort": null}, "agentMode": "multi", "agentModeRevision": 0, "agentModeSupported": true, "startAttempt": {"id": "0897d819-94be-44cd-930e-574c1c026bb7", "epoch": 0, "activeAtReservation": false, "accountKey": "default", "events": ["777c97c9-d1ee-4c3d-970d-ff0badc41e7c:initial"], "submitted": true, "created": 2000000000.0, "settingsFixed": true, "nativeOperationId": "turn:777c97c9-d1ee-4c3d-970d-ff0badc41e7c:777c97c9-d1ee-4c3d-970d-ff0badc41e7c:initial:attempt:0897d819-94be-44cd-930e-574c1c026bb7", "connectionId": "cdd0a7b5-7876-4a7f-9955-d1325a58ecdb", "threadId": "thread-1", "supervisorIdentity": null, "observedTurnId": "turn-2", "turnId": "turn-2"}, "nativeEffort": "medium", "prepareAttempt": "a8d9e68e-2041-4afd-a183-eee37a5316d7", "nativeTeleportTool": "orchestration_teleport", "nativeRoleGuidance": "[Studio role skill: codex-orchestrator]\nSource: /fixture/source/.agents/skills/codex-orchestrator/SKILL.md\nShared tool guidance: /fixture/source/.agents/skills/codex-workspace/SKILL.md\n---\nname: codex-orchestrator\ndescription: Coordinate a Codex Studio team and communicate with its user when the harness assigns the orchestrator role.\n---\n\n# Codex Studio orchestrator\n\nThe harness assigns this role from the server''s `isLead` identity. A task label or\nan agent''s stated role cannot grant it. Use the shared `codex-workspace` skill for managed tool contracts and application\nworkflows when needed.\n\nStudio resource reservations are removed. Do not wait for old board claims or recreate the registry.\nUse agent messages to coordinate access to shared files.\n\n## Team responsibility\n\nOwn the user''s task and the team''s final result. Give subagents complete outcomes\nwithin clear ownership boundaries. Inspect their results, changes, and evidence before\nacceptance. Request corrections when the evidence does not support completion.\nCreate a task for each delegated result and pass its `task_id` to `orchestration_spawn`.\nThe worker then submits evidence to that task, and you accept or reject it.\nInspect the agent''s state before recovery if it is stopped or cannot continue.\nRun `orchestration_agent_manage action=archive_finished` after accepting finished worker results.\n\nKeep one agent plan. Send changes to a subagent''s plan through its chat.\nUse agent messages for coordination. Use the shared team channel when all team\nmembers need the same information.\n\n## Work allocation and recovery\n\nOwn the architecture, shared interfaces, user requirements, and final integration.\nDelegate a complete feature or component with an observable result.\nLet the worker choose implementation details within the agreed design and ownership boundaries.\nResolve changes to shared contracts or task scope yourself.\nAn explicit request to work without subagents takes precedence.\n\nChoose the model and reasoning level for each worker''s task. Use `gpt-6-luna`\nwith `high` reasoning by default. Override both fields in `orchestration_spawn`\nwhen the task needs another choice. Codex and Claude workers can share a team.\nHonor the user''s explicit model or account constraints.\n\nChat creation selects `workspaceMode`: `layr`, `image`, or `worktree`.\nMac servers default to `layr`. Linux servers support `worktree` only.\n\n| Chat mode  | Agent environment | Worker workspace         |\n| ---------- | ----------------- | ------------------------ |\n| `layr`     | Studio Linux VM   | Separate owned layr line |\n| `image`    | Native macOS      | ASIF image               |\n| `worktree` | Native host       | Git worktree             |\n\nWorkers inherit the chat mode. In a native chat, use `workspace` to select\n`image`, `worktree`, or deliberate `shared` edits. Image workspaces require macOS.\nA native reviewer uses the selected folder with read-only access.\nA layr reviewer uses a read-only line in the VM.\nUse the layr reference for VM state IDs, exact-state review, and acceptance.\nUse `server` with an absolute remote `cwd` for a native worker on a paired machine.\n\nDefine each implementation assignment with:\n\n- The required behavior and its actual caller or user flow.\n- The constraints, dependencies, and ownership boundaries.\n- The checks that can disprove completion, including relevant failure paths.\n- The authority to edit, commit, and integrate changes.\n\nInclude caller integration, relevant tests, and defect correction in the same assignment when ownership permits.\nKeep the worker responsible through review corrections and verification of the assigned result.\nIf another owner must integrate the change, name that dependency and the required acceptance evidence.\nDo not count an unused API or an isolated source change as a complete feature.\n\nChoose task size by independent responsibility and the cost of coordination.\nDo not divide work to meet a line count or worker count.\nKeep small fixes with the existing component owner, or make them yourself when that avoids a needless handoff.\nDo not create a new worker for each known edit or each review correction.\nBatch related corrections under the existing assignment and completion criteria.\nSplit work by technical layer only when separate ownership, dependencies, or verification justify the handoff.\nAvoid broad assignments that group unrelated subsystems without a common result or completion check.\nFor example, one worker can own the complete route display across model, transport, and UI.\n\nKeep independent work active while a build or external dependency is pending.\nAfter a worker result, review it and assign the next ready task when one exists.\nDetermine which tasks actually depend on the blocked build.\nUse the current registry to distinguish active, queued, completed, and failed agents.\nChoose team size from useful independent work and actual resource limits.\nHonor task-specific concurrency instructions. Give each active worker useful work\nwith clear completion criteria.\n\nUse the chat''s current subagent parallelism limit from Studio context and status.\nThe lead does not consume that allowance. Zero means complete new work yourself;\ndo not create workers or resume worker turns. For a positive limit, size each\ndelegation batch to useful independent work and available slots. Keep future\nassignments on the task board instead of creating a large waiting worker pool.\nThe limit is a ceiling, not a target. When it changes, adjust subsequent\nassignments without interrupting accepted executions. If all slots are occupied,\ncontinue independent lead work or finish the turn and wait for completion events.\nDo not poll status or repeat a spawn to get around the queue. Only the user can\nchange this limit.\n\nIf workers stop unexpectedly, inspect their state, receipts, and saved results.\nResume remaining authorized work in the existing threads when possible. Report a\nharness defect if recovery cannot preserve their context. Follow the shared skill''s\nrequest recovery rules before retrying any uncertain operation.\nBase reports of continued work on an active worker, command, or automatic continuation.\n\n## Coordination rules\n\n- Give each worker an explicit list of pre-authorized actions, for example a\n  merge into the integration branch or a one-time annotation.\n- Put decisions into the task (reject or update) or into `orchestration_send`,\n  not only into chat. A chat message can arrive late.\n- Give every shared branch and every shared file one owner. State who may merge\n  into an integration branch.\n- Give workers the folder that contains their task. Supported platforms copy\n  its Git root, or the folder itself outside Git, including uncommitted changes.\n  Other platforms use a Git worktree when possible, or the original folder.\n  New workers have read-only access until the image base is ready. Ask workers\n  to commit on a named branch. Integrate by reading the copy path or fetching\n  the branch, for example `git fetch <path> <branch>`.\n- Do not run two live verifications that change the same system state at the\n  same time. Schedule them, or give them independent criteria.\n\n## Decisions, quality, and progress\n\nMake routine design decisions within the user''s task and project constraints.\nInspect existing code before introducing a replacement for an available mechanism.\nWhen authorized to fix confirmed defects, arrange the fixes as you confirm them.\nA review-only request does not grant that authority.\n\nReview every subagent change before integration. Check the affected user flow,\nnot only a successful build. If the user asks to try the result before merge,\nprepare and verify that local application before merging. Keep fixtures and real\napplication results distinct. Preserve all accepted requirements in the project\ndocument when the user asks to save design decisions.\n\nShow progress separately for each requested product or workstream. Include the\ncurrent stage, remaining work, and the dependency that prevents the next step.\nUse measured counts or explicit states; do not invent percentages. Update your\n`PROGRESS.md` with ordinary file tools after every significant change.\nDuring an active task, update it at least once every 30 minutes.\nIf nothing changes, record the current state and its check time.\nUse the exact path supplied by the runtime. Follow the shared progress guide.\nContinuously improve the team''s speed within the task''s authority. Use task duration,\nwait time, and repeated corrections to identify delays. Adjust task size, dependencies,\nwork allocation, and checks when they delay completion. Assess whether each adjustment\nreduces the time to a verified result. Preserve the required checks and code quality.\n\n## Messages to the user\n\nOnly the orchestrator sends conversational questions, complaints, or requests to\nthe user. Subagents send these to you. Decide whether you can resolve each request\nwithin the user''s existing instructions or need the user''s answer.\n\nUse `orchestration_complaint action=submit` for a decision that the user must record.\nUse `orchestration_message target=user` for an action that the user must perform.\nUse these managed tools for conversational questions. They do not pause the current\ntool while the user decides. Continue authorized work that does not need the answer.\nWait for the answer before you perform a dependent action.\nNative question tools, such as `AskUserQuestion`, wait inside the current turn.\nIf you forward a subagent''s request, explain the issue and the decision the user must make.\nDo not forward each request automatically.\n\nFor a complaint assigned to you, use `action=respond` to record your action,\nreasoned refusal, or next step. If the user must decide, submit your own message\nto the user and tell the subagent that the decision remains pending.\n\nNative tool permission requests still require the real user approval when the\npermission system requires it. Your decision cannot replace that approval.\n\n## Teleport the lead or a worker\n\nEach new session can call `orchestration_teleport` to teleport its own execution.\nOlder sessions keep `orchestration_move` and its exact definition and role text.\nStudio saves the name when it creates the native session.\nThe name stays the same after a teleport. This change preserves the old prompt cache.\nPrepare the target folder with Git first. Supply an absolute `cwd` and a stable\nrequest ID. Finish the current turn after acceptance.\nExisting children keep their servers and report through the remote-parent channel.\nStudio preserves the old workspace and transfers native provider history.\nThe Studio transcript stays on the source. Credentials never move.\n\nMove only when needed. The same provider account and organization keep the cache.\nCodex preserves the old request prefix and requires matching native versions,\nOS, and MCP tools. A different account refuses by default.\nOnly explicit `accept_cache_loss=true` approves cache loss for that Codex move.\nClaude requires a verified saved prompt snapshot and matching Studio tool schemas.\nExternal MCP snapshots currently refuse.\nClaude requires the same account and matching CLI and SDK versions.\nIts SDK keeps the system prompt snapshot and excludes dynamic sections.\nRefuse a version mismatch. Do not upgrade a CLI during a move.\nRead uncertainty with `orchestration_request`. Do not issue another move identity.\n[End Studio role skill]", "nativeToolSource": "current", "sandbox": {"type": "readOnly"}, "approvalPolicy": "on-request", "profile": null, "preparedContext": {"epoch": ["thread-1", 0], "versions": {"roleSkill": "7b2dc3aeeb5bbec678451fe6", "progressFile": "5ea10bda4b522c2ab8f4ea52"}}, "nativeToolCatalog": {"threadId": "thread-1", "digest": "8709d196bff92353f93bfcb46ab595eae45c28610b83077a77b9361404bc8d77"}, "complaintsPresented": [], "lastEvent": "2026-10-22T15:52:57Z", "lastAnswer": "Choose", "activity": {"phase": "thinking", "at": 2000000000.0}, "activeTools": [], "nativeResponseTurn": "turn-2"}');
INSERT INTO "runtime_context_manifests" VALUES('777c97c9-d1ee-4c3d-970d-ff0badc41e7c','thread-1',0,1,'777c97c9-d1ee-4c3d-970d-ff0badc41e7c:initial','{"epoch":["thread-1",0],"versions":{"agentMode":"cbaab587bdd3e5b2b44c85e8","progressFile":"5ea10bda4b522c2ab8f4ea52","roleSkill":"7b2dc3aeeb5bbec678451fe6"},"sequence":1}');
INSERT INTO "runtime_event_meta" VALUES('777c97c9-d1ee-4c3d-970d-ff0badc41e7c:initial','{"delivery":"current_turn","timing":{"enqueuedAt":223611023181956,"fastSkipReason":"scheduleOverride","dispatchPickedAt":223611077277098,"startBeganAt":223611078466875,"validatedAt":223611086956076,"programReadyAt":223611088168004,"repairBeganAt":223611088170750,"repairCheckedAt":223611090480353,"prepareBeganAt":223611090487196,"prepareGuardReadyAt":223611090504408,"prepareGuardAcquiredAt":223611090504649,"prepareChecksDoneAt":223611092557722,"prepareConnectedAt":223611103683378,"threadParamsReadyAt":223611157631344,"threadSubmittedAt":223611161013215,"threadReadyAt":223611165283939,"repairReadyAt":223611165292114,"postPrepareWaitBeganAt":223611165292505,"postPrepareLockedAt":223611166243765,"preparedAt":223611166244176,"connectedAt":223611167800099,"transcriptLockWaitBeganAt":223611167800490,"transcriptLockedAt":223611168710503,"transcriptChecksDoneAt":223611168963036,"modelTextReadyAt":223611172834372,"eventMetaReadyAt":223611172943557,"contextReadyAt":223611174356712,"itemReadyAt":223611175660052,"transcriptReadyAt":223611175660573,"turnParamsReadyAt":223611175670572,"reservationCommittedAt":223611177352079,"nativeSubmitBeganAt":223611177352500,"submittedAt":223611177700101,"acceptedAt":223611195179100},"modelEventProjection":1,"contextManifest":{"epoch":["thread-1",0],"versions":{"agentMode":"cbaab587bdd3e5b2b44c85e8","progressFile":"5ea10bda4b522c2ab8f4ea52","roleSkill":"7b2dc3aeeb5bbec678451fe6"},"sequence":1},"transcriptItemId":"777c97c9-d1ee-4c3d-970d-ff0badc41e7c:777c97c9-d1ee-4c3d-970d-ff0badc41e7c:initial"}');
INSERT INTO "runtime_events" VALUES('777c97c9-d1ee-4c3d-970d-ff0badc41e7c:initial','777c97c9-d1ee-4c3d-970d-ff0badc41e7c','user','Persist a turn','delivered',2000000000.0,0,'turn-2',NULL);
INSERT INTO "runtime_execution_attempts" VALUES('0897d819-94be-44cd-930e-574c1c026bb7','run:0897d819-94be-44cd-930e-574c1c026bb7','{"id": "0897d819-94be-44cd-930e-574c1c026bb7", "epoch": 0, "activeAtReservation": false, "accountKey": "default", "events": ["777c97c9-d1ee-4c3d-970d-ff0badc41e7c:initial"], "submitted": true, "created": 2000000000.0, "settingsFixed": true, "nativeOperationId": "turn:777c97c9-d1ee-4c3d-970d-ff0badc41e7c:777c97c9-d1ee-4c3d-970d-ff0badc41e7c:initial:attempt:0897d819-94be-44cd-930e-574c1c026bb7", "connectionId": "cdd0a7b5-7876-4a7f-9955-d1325a58ecdb", "threadId": "thread-1", "supervisorIdentity": null, "observedTurnId": "turn-2", "turnId": "turn-2", "runId": "run:0897d819-94be-44cd-930e-574c1c026bb7", "submission": "accepted", "prepareAttemptId": "a8d9e68e-2041-4afd-a183-eee37a5316d7", "turnStatus": null, "resultRunId": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:777c97c9-d1ee-4c3d-970d-ff0badc41e7c:question:retained-question','run:0897d819-94be-44cd-930e-574c1c026bb7','requests','777c97c9-d1ee-4c3d-970d-ff0badc41e7c:question:retained-question','{"id": "requests:777c97c9-d1ee-4c3d-970d-ff0badc41e7c:question:retained-question", "runId": "run:0897d819-94be-44cd-930e-574c1c026bb7", "kind": "requests", "referenceId": "777c97c9-d1ee-4c3d-970d-ff0badc41e7c:question:retained-question", "attemptId": "0897d819-94be-44cd-930e-574c1c026bb7", "requestId": null, "status": "pending", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:8a0fd963-9799-4fd8-9201-d7e7cd13744f','run:0897d819-94be-44cd-930e-574c1c026bb7','requests','8a0fd963-9799-4fd8-9201-d7e7cd13744f','{"id": "requests:8a0fd963-9799-4fd8-9201-d7e7cd13744f", "runId": "run:0897d819-94be-44cd-930e-574c1c026bb7", "kind": "requests", "referenceId": "8a0fd963-9799-4fd8-9201-d7e7cd13744f", "attemptId": "0897d819-94be-44cd-930e-574c1c026bb7", "requestId": null, "status": "pending", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('monitors:ef833e85-ebe4-5750-a66f-ccf85ea2043c','run:0897d819-94be-44cd-930e-574c1c026bb7','monitors','ef833e85-ebe4-5750-a66f-ccf85ea2043c','{"id": "monitors:ef833e85-ebe4-5750-a66f-ccf85ea2043c", "runId": "run:0897d819-94be-44cd-930e-574c1c026bb7", "kind": "monitors", "referenceId": "ef833e85-ebe4-5750-a66f-ccf85ea2043c", "attemptId": "0897d819-94be-44cd-930e-574c1c026bb7", "requestId": "pending-monitor", "status": "approval", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:fa930719-b220-4226-96cd-1e175ce13b0c','run:0897d819-94be-44cd-930e-574c1c026bb7','requests','fa930719-b220-4226-96cd-1e175ce13b0c','{"id": "requests:fa930719-b220-4226-96cd-1e175ce13b0c", "runId": "run:0897d819-94be-44cd-930e-574c1c026bb7", "kind": "requests", "referenceId": "fa930719-b220-4226-96cd-1e175ce13b0c", "attemptId": "0897d819-94be-44cd-930e-574c1c026bb7", "requestId": null, "status": "pending", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('monitors:7476dd05-6094-5a65-9ba0-3007fab6be6b','run:0897d819-94be-44cd-930e-574c1c026bb7','monitors','7476dd05-6094-5a65-9ba0-3007fab6be6b','{"id": "monitors:7476dd05-6094-5a65-9ba0-3007fab6be6b", "runId": "run:0897d819-94be-44cd-930e-574c1c026bb7", "kind": "monitors", "referenceId": "7476dd05-6094-5a65-9ba0-3007fab6be6b", "attemptId": "0897d819-94be-44cd-930e-574c1c026bb7", "requestId": "running-monitor", "status": "running", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('monitors:rule-linked-monitor','run:0897d819-94be-44cd-930e-574c1c026bb7','monitors','rule-linked-monitor','{"id": "monitors:rule-linked-monitor", "runId": "run:0897d819-94be-44cd-930e-574c1c026bb7", "kind": "monitors", "referenceId": "rule-linked-monitor", "attemptId": "0897d819-94be-44cd-930e-574c1c026bb7", "requestId": "running-monitor", "status": "running", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_inputs" VALUES('run:0897d819-94be-44cd-930e-574c1c026bb7','777c97c9-d1ee-4c3d-970d-ff0badc41e7c:initial');
INSERT INTO "runtime_execution_runs" VALUES('run:0897d819-94be-44cd-930e-574c1c026bb7','777c97c9-d1ee-4c3d-970d-ff0badc41e7c','default',0,'thread-1','turn-2',2000000000.0,'{"id": "run:0897d819-94be-44cd-930e-574c1c026bb7", "agent": "777c97c9-d1ee-4c3d-970d-ff0badc41e7c", "accountKey": "default", "epoch": 0, "threadId": "thread-1", "turnId": "turn-2", "created": 2000000000.0, "status": "running", "firstAttemptId": "0897d819-94be-44cd-930e-574c1c026bb7", "rootAttemptId": "0897d819-94be-44cd-930e-574c1c026bb7", "latestAttemptId": "0897d819-94be-44cd-930e-574c1c026bb7"}');
INSERT INTO "runtime_items" VALUES('777c97c9-d1ee-4c3d-970d-ff0badc41e7c:777c97c9-d1ee-4c3d-970d-ff0badc41e7c:initial','777c97c9-d1ee-4c3d-970d-ff0badc41e7c','{"id": "777c97c9-d1ee-4c3d-970d-ff0badc41e7c:777c97c9-d1ee-4c3d-970d-ff0badc41e7c:initial", "role": "user", "title": "User", "text": "Persist a turn\n\n[Studio subagent concurrency, revision 0] Multi agent mode (subagent concurrency 32): delegate sensibly up to 32 simultaneous descendant turns across all providers and reviewer roles. The lead does not use a subagent slot. Excess worker turns queue. Do not poll unchanged status.", "truncated": false, "at": 2000000000.0, "assets": [], "inputs": [{"id": "777c97c9-d1ee-4c3d-970d-ff0badc41e7c:initial", "at": 2000000000.0, "kind": "user", "text": "Persist a turn", "truncated": false, "assets": []}], "turnId": "turn-2"}',2000000000.0);
INSERT INTO "runtime_items" VALUES('777c97c9-d1ee-4c3d-970d-ff0badc41e7c:retained-question','777c97c9-d1ee-4c3d-970d-ff0badc41e7c','{"id": "777c97c9-d1ee-4c3d-970d-ff0badc41e7c:retained-question", "role": "assistant", "title": "Assistant", "text": "Choose", "truncated": false, "at": 2000000000.0, "streaming": false, "turnId": "turn-2", "phase": null}',2000000000.0);
INSERT INTO "runtime_monitors" VALUES('ef833e85-ebe4-5750-a66f-ccf85ea2043c','{"id": "ef833e85-ebe4-5750-a66f-ccf85ea2043c", "agent": "777c97c9-d1ee-4c3d-970d-ff0badc41e7c", "epoch": 0, "turnId": "turn-2", "requestId": "pending-monitor", "command": "printf needs approval", "cwd": "/FROZEN_STARTUP_STATE", "timeout_ms": 3600000, "stallTimeoutSeconds": 1800, "livenessCommand": "", "activityAt": 2000000000.0, "activityGeneration": 0, "stallWakeGeneration": -1, "wakeOn": "exit", "successExitCodes": [0], "outputPatterns": [], "outputEventSequence": 0, "outputEventCount": 0, "outputEventIds": [], "status": "approval", "created": 2000000000.0, "exitCode": null, "tail": "", "bytes": 0, "interactive": false, "ruleId": null, "log": "/FROZEN_STARTUP_STATE/monitor-logs/ef833e85-ebe4-5750-a66f-ccf85ea2043c.log"}');
INSERT INTO "runtime_monitors" VALUES('7476dd05-6094-5a65-9ba0-3007fab6be6b','{"id": "7476dd05-6094-5a65-9ba0-3007fab6be6b", "agent": "777c97c9-d1ee-4c3d-970d-ff0badc41e7c", "epoch": 0, "turnId": "turn-2", "requestId": "running-monitor", "command": "printf running", "cwd": "/FROZEN_STARTUP_STATE", "timeout_ms": 3600000, "stallTimeoutSeconds": 1800, "livenessCommand": "", "activityAt": 2000000000.0, "activityGeneration": 0, "stallWakeGeneration": -1, "wakeOn": "exit", "successExitCodes": [0], "outputPatterns": [], "outputEventSequence": 0, "outputEventCount": 0, "outputEventIds": [], "status": "running", "created": 2000000000.0, "exitCode": null, "tail": "", "bytes": 0, "interactive": false, "ruleId": null, "log": "/FROZEN_STARTUP_STATE/monitor-logs/7476dd05-6094-5a65-9ba0-3007fab6be6b.log", "error": null, "operation": {"agent": "777c97c9-d1ee-4c3d-970d-ff0badc41e7c", "epoch": 0, "accountKey": "default", "connectionId": "cdd0a7b5-7876-4a7f-9955-d1325a58ecdb", "threadId": "thread-1"}, "configurationPending": false}');
INSERT INTO "runtime_monitors" VALUES('rule-linked-monitor','{"id": "rule-linked-monitor", "agent": "777c97c9-d1ee-4c3d-970d-ff0badc41e7c", "epoch": 0, "turnId": "turn-2", "requestId": "running-monitor", "command": "printf running", "cwd": "/FROZEN_STARTUP_STATE", "timeout_ms": 3600000, "stallTimeoutSeconds": 1800, "livenessCommand": "", "activityAt": 2000000000.0, "activityGeneration": 0, "stallWakeGeneration": -1, "wakeOn": "exit", "successExitCodes": [0], "outputPatterns": [], "outputEventSequence": 0, "outputEventCount": 0, "outputEventIds": [], "status": "running", "created": 2000000000.0, "exitCode": null, "tail": "", "bytes": 0, "interactive": false, "ruleId": "rule-1", "log": "/FROZEN_STARTUP_STATE/monitor-logs/7476dd05-6094-5a65-9ba0-3007fab6be6b.log", "error": null, "operation": {"agent": "777c97c9-d1ee-4c3d-970d-ff0badc41e7c", "epoch": 0, "accountKey": "default", "connectionId": "cdd0a7b5-7876-4a7f-9955-d1325a58ecdb", "threadId": "thread-1"}, "configurationPending": false}');
INSERT INTO "runtime_projects" VALUES('/FROZEN_STARTUP_STATE','{"id": "/FROZEN_STARTUP_STATE", "path": "/FROZEN_STARTUP_STATE", "name": "startup", "created": 2000000000.0, "accountKey": "default", "accountRevision": 1, "homeServerId": "b44a8684bd59488da64b35c0805633ab", "locations": [{"serverId": "b44a8684bd59488da64b35c0805633ab", "path": "/FROZEN_STARTUP_STATE", "projectId": "/FROZEN_STARTUP_STATE"}], "locationsRevision": 0}');
INSERT INTO "runtime_requests" VALUES('777c97c9-d1ee-4c3d-970d-ff0badc41e7c:question:retained-question','{"id": "777c97c9-d1ee-4c3d-970d-ff0badc41e7c:question:retained-question", "method": "agent/asyncQuestion", "agent": "777c97c9-d1ee-4c3d-970d-ff0badc41e7c", "epoch": 0, "turnId": "turn-2", "params": {"questions": [{"id": "0", "question": "Keep this question", "options": [{"label": "yes"}, {"label": "no"}], "multiSelect": false, "isSecret": false}]}, "status": "pending", "createdAt": 2000000000.0}');
INSERT INTO "runtime_requests" VALUES('8a0fd963-9799-4fd8-9201-d7e7cd13744f','{"id": "8a0fd963-9799-4fd8-9201-d7e7cd13744f", "rpcId": "provider-approval", "method": "item/commandExecution/requestApproval", "params": {"threadId": "thread-1", "turnId": "turn-2", "itemId": "approval-item", "command": "printf safe"}, "agent": "777c97c9-d1ee-4c3d-970d-ff0badc41e7c", "status": "pending", "accountKey": "default", "connectionId": null, "createdAt": 2000000000.0, "epoch": 0, "supervisor": null}');
INSERT INTO "runtime_requests" VALUES('fa930719-b220-4226-96cd-1e175ce13b0c','{"id": "fa930719-b220-4226-96cd-1e175ce13b0c", "method": "monitor/approve", "agent": "777c97c9-d1ee-4c3d-970d-ff0badc41e7c", "params": {"monitorId": "ef833e85-ebe4-5750-a66f-ccf85ea2043c", "command": "printf needs approval", "cwd": "/FROZEN_STARTUP_STATE"}, "status": "pending", "createdAt": 2000000000.0}');
INSERT INTO "runtime_requests" VALUES('beb71d90-e19c-4a9e-bad5-a017675f4492','{"id": "beb71d90-e19c-4a9e-bad5-a017675f4492", "rpcId": "orphan-request", "method": "mcpServer/elicitation/request", "params": {"mode": "form", "requestedSchema": {"properties": {}}}, "agent": null, "status": "pending", "accountKey": "default", "connectionId": "cdd0a7b5-7876-4a7f-9955-d1325a58ecdb", "createdAt": 2000000000.0}');
INSERT INTO "runtime_scheduler_agent_change_state" VALUES(1,0,12,1,12);
INSERT INTO "runtime_scheduler_agent_changes" VALUES('777c97c9-d1ee-4c3d-970d-ff0badc41e7c',12);
INSERT INTO "runtime_server_sequence" VALUES(1,0);
INSERT INTO "runtime_turn_item_link_state_v1" VALUES(1,'c3c7cffd38a84d658b21ddd76a084534',96,NULL,NULL,1,0,0,2000000000.0);
INSERT INTO "runtime_turn_item_links_v1" VALUES('777c97c9-d1ee-4c3d-970d-ff0badc41e7c:777c97c9-d1ee-4c3d-970d-ff0badc41e7c:initial','777c97c9-d1ee-4c3d-970d-ff0badc41e7c','turn-2',2000000000.0,'c3c7cffd38a84d658b21ddd76a084534');
INSERT INTO "runtime_turn_item_links_v1" VALUES('777c97c9-d1ee-4c3d-970d-ff0badc41e7c:retained-question','777c97c9-d1ee-4c3d-970d-ff0badc41e7c','turn-2',2000000000.0,'c3c7cffd38a84d658b21ddd76a084534');
INSERT INTO "sync_entities" VALUES('project','/FROZEN_STARTUP_STATE',1,'23ed4a7271bcc94acf0c1bdec7873a2079eece618b554e7a8679569f4ea762a2','{"collection":"project","id":"/FROZEN_STARTUP_STATE","value":{"accountKey":"default","accountRevision":1,"created":2000000000.0,"homeServerId":"b44a8684bd59488da64b35c0805633ab","id":"/FROZEN_STARTUP_STATE","locations":[{"path":"/FROZEN_STARTUP_STATE","projectId":"/FROZEN_STARTUP_STATE","serverId":"b44a8684bd59488da64b35c0805633ab"}],"locationsRevision":0,"name":"startup","path":"/FROZEN_STARTUP_STATE"}}',0);
INSERT INTO "sync_entities" VALUES('agent','777c97c9-d1ee-4c3d-970d-ff0badc41e7c',13,'4c79abeef52445497e44b569807b501193611ad0e1c1a635105b6881f40f6fb4','{"collection":"agent","id":"777c97c9-d1ee-4c3d-970d-ff0badc41e7c","value":{"accountKey":"default","activity":{"at":2000000000.0,"phase":"thinking"},"agentMode":"multi","agentModeRevision":0,"agentModeSupported":true,"autoWake":true,"canSend":true,"commandProgress":[],"compactions":0,"concurrency":32,"contextUsage":null,"created":2000000000.0,"cwd":"/FROZEN_STARTUP_STATE","cyberAccessProgram":"standard","daybreakEnabled":false,"effort":"medium","empty":false,"environment":"host","epoch":0,"error":null,"executionMode":"native","fastMode":false,"hostProjectPath":"/FROZEN_STARTUP_STATE","id":"777c97c9-d1ee-4c3d-970d-ff0badc41e7c","imageWorkspace":false,"imageWorkspaceBaseRef":null,"imageWorkspaceCreatedAt":null,"imageWorkspaceError":null,"imageWorkspaceHasGit":false,"imageWorkspacePhase":null,"imageWorkspaceReady":false,"imageWorkspaceRepo":null,"imageWorkspaceSubpath":".","inFlight":true,"isLead":true,"kind":"agent","lastAnswer":"Choose","lastEvent":"2026-10-22T15:52:57Z","launcherAlive":true,"layrReady":false,"model":"gpt-6-astra","name":"Lead","nextTurnSettingsSupported":true,"parentId":null,"provider":"codex","readStateSupported":true,"reviewDefaults":{"effort":null,"model":null},"role":"orchestrator","rootId":"777c97c9-d1ee-4c3d-970d-ff0badc41e7c","serverId":"b44a8684bd59488da64b35c0805633ab","source":"managed","stall":null,"startAttempt":{},"startWait":null,"status":"approval","subagentConcurrencyVersion":2,"tail":"Choose","threadId":"thread-1","tokensUsed":0,"turnId":"turn-2","workerDefaults":{"daybreakEnabled":false,"delegationDepth":1,"effort":"high","fastMode":false,"model":"gpt-6-luna"},"workspaceBackend":null,"workspaceMode":"worktree","worktree":false,"yoloMode":true}}',0);
INSERT INTO "sync_entities" VALUES('event','777c97c9-d1ee-4c3d-970d-ff0badc41e7c:initial',9,'6d15a74694255ec0e49c7d55fa471613573c44a2e4378a0a1bb716ce160457e8','{"collection":"event","id":"777c97c9-d1ee-4c3d-970d-ff0badc41e7c:initial","value":{"agent":"777c97c9-d1ee-4c3d-970d-ff0badc41e7c","created":2000000000.0,"error":null,"id":"777c97c9-d1ee-4c3d-970d-ff0badc41e7c:initial","kind":"user","status":"delivered"}}',0);
INSERT INTO "sync_entities" VALUES('request','777c97c9-d1ee-4c3d-970d-ff0badc41e7c:question:retained-question',10,'4755f98d9eb018059ffbab9d6085611bde22bb87220537d308cc1ac52b143784','{"collection":"request","id":"777c97c9-d1ee-4c3d-970d-ff0badc41e7c:question:retained-question","value":{"agent":"777c97c9-d1ee-4c3d-970d-ff0badc41e7c","createdAt":2000000000.0,"epoch":0,"id":"777c97c9-d1ee-4c3d-970d-ff0badc41e7c:question:retained-question","method":"agent/asyncQuestion","params":{"questions":[{"id":"0","isSecret":false,"multiSelect":false,"options":[{"label":"yes"},{"label":"no"}],"question":"Keep this question"}]},"status":"pending","turnId":"turn-2"}}',0);
INSERT INTO "sync_entities" VALUES('request','8a0fd963-9799-4fd8-9201-d7e7cd13744f',12,'038be16460fe33914d41abd8211d83b4168aba909222900d44639d3972f4779d','{"collection":"request","id":"8a0fd963-9799-4fd8-9201-d7e7cd13744f","value":{"accountKey":"default","agent":"777c97c9-d1ee-4c3d-970d-ff0badc41e7c","connectionId":null,"createdAt":2000000000.0,"epoch":0,"id":"8a0fd963-9799-4fd8-9201-d7e7cd13744f","method":"item/commandExecution/requestApproval","params":{"command":"printf safe","itemId":"approval-item","threadId":"thread-1","turnId":"turn-2"},"status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('monitor','ef833e85-ebe4-5750-a66f-ccf85ea2043c',14,'e9e55d27a179be95128db83dcba850ff51e37170dae8cd5580a7f7b70be3f290','{"collection":"monitor","id":"ef833e85-ebe4-5750-a66f-ccf85ea2043c","value":{"agent":"777c97c9-d1ee-4c3d-970d-ff0badc41e7c","bytes":0,"command":"printf needs approval","created":2000000000.0,"cwd":"/FROZEN_STARTUP_STATE","exitCode":null,"id":"ef833e85-ebe4-5750-a66f-ccf85ea2043c","interactive":false,"log":"/FROZEN_STARTUP_STATE/monitor-logs/ef833e85-ebe4-5750-a66f-ccf85ea2043c.log","ruleId":null,"status":"approval","tail":"","timeout_ms":3600000}}',0);
INSERT INTO "sync_entities" VALUES('request','fa930719-b220-4226-96cd-1e175ce13b0c',15,'65b8cbb911d51541c6c323794dba832ebdf0cbfd969126cbd6b8267e6175037e','{"collection":"request","id":"fa930719-b220-4226-96cd-1e175ce13b0c","value":{"agent":"777c97c9-d1ee-4c3d-970d-ff0badc41e7c","createdAt":2000000000.0,"id":"fa930719-b220-4226-96cd-1e175ce13b0c","method":"monitor/approve","params":{"command":"printf needs approval","cwd":"/FROZEN_STARTUP_STATE","monitorId":"ef833e85-ebe4-5750-a66f-ccf85ea2043c"},"status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('request','beb71d90-e19c-4a9e-bad5-a017675f4492',16,'7e00de21cbf6534a84e04a8502a3263888cc2df59fe1f70eab5d7583690d0ee2','{"collection":"request","id":"beb71d90-e19c-4a9e-bad5-a017675f4492","value":{"accountKey":"default","agent":null,"connectionId":"cdd0a7b5-7876-4a7f-9955-d1325a58ecdb","createdAt":2000000000.0,"id":"beb71d90-e19c-4a9e-bad5-a017675f4492","method":"mcpServer/elicitation/request","params":{"mode":"form","requestedSchema":{"properties":{}}},"status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('monitor','7476dd05-6094-5a65-9ba0-3007fab6be6b',18,'d490232fab710e8476cd5495b445ea3f7b459386b6f4d73467f6c6069d8c4876','{"collection":"monitor","id":"7476dd05-6094-5a65-9ba0-3007fab6be6b","value":{"agent":"777c97c9-d1ee-4c3d-970d-ff0badc41e7c","bytes":0,"command":"printf running","created":2000000000.0,"cwd":"/FROZEN_STARTUP_STATE","error":null,"exitCode":null,"id":"7476dd05-6094-5a65-9ba0-3007fab6be6b","interactive":false,"log":"/FROZEN_STARTUP_STATE/monitor-logs/7476dd05-6094-5a65-9ba0-3007fab6be6b.log","ruleId":null,"status":"running","tail":"","timeout_ms":3600000}}',0);
INSERT INTO "sync_entities" VALUES('monitor','rule-linked-monitor',19,'ebbe5a474694612a2373c0fbb4fb9b6929162ce402bd441bd48810c8df4cd882','{"collection":"monitor","id":"rule-linked-monitor","value":{"agent":"777c97c9-d1ee-4c3d-970d-ff0badc41e7c","bytes":0,"command":"printf running","created":2000000000.0,"cwd":"/FROZEN_STARTUP_STATE","error":null,"exitCode":null,"id":"rule-linked-monitor","interactive":false,"log":"/FROZEN_STARTUP_STATE/monitor-logs/7476dd05-6094-5a65-9ba0-3007fab6be6b.log","ruleId":"rule-1","status":"running","tail":"","timeout_ms":3600000}}',0);
INSERT INTO "sync_entity_meta" VALUES('entity_tombstone_count','0');
INSERT INTO "sync_entity_meta" VALUES('event_window_seq','9');
INSERT INTO "runtime_search"(rowid,id,agent,kind,body) VALUES(1,'777c97c9-d1ee-4c3d-970d-ff0badc41e7c:777c97c9-d1ee-4c3d-970d-ff0badc41e7c:initial','777c97c9-d1ee-4c3d-970d-ff0badc41e7c','user','Persist a turn

[Studio subagent concurrency, revision 0] Multi agent mode (subagent concurrency 32): delegate sensibly up to 32 simultaneous descendant turns across all providers and reviewer roles. The lead does not use a subagent slot. Excess worker turns queue. Do not poll unchanged status.');
INSERT INTO "runtime_search"(rowid,id,agent,kind,body) VALUES(2,'777c97c9-d1ee-4c3d-970d-ff0badc41e7c:retained-question','777c97c9-d1ee-4c3d-970d-ff0badc41e7c','assistant','Choose');

CREATE INDEX runtime_access_nonce_age ON runtime_access_nonces(created);
CREATE INDEX runtime_access_outbound_age
          ON runtime_access_outbound(json_extract(record,'$.completed'))
          WHERE json_extract(record,'$.outcome')='complete';
CREATE INDEX runtime_access_receipt_age
          ON runtime_access_receipts(json_extract(record,'$.completed')) WHERE status='complete';
CREATE INDEX runtime_account_transfer_status ON runtime_account_transfers(json_extract(record,'$.status'));
CREATE INDEX runtime_agent_account_scope ON runtime_agents(
                    CASE WHEN json_type(record,'$.accountKey') IS NULL THEN 'default'
                         ELSE json_extract(record,'$.accountKey') END);
CREATE INDEX runtime_agent_capacity_retry_state_due_v2 ON runtime_agents(
                    json_extract(record,'$.capacityRetry.status'),
                    json_extract(record,'$.capacityRetry.dueAt'));
CREATE INDEX runtime_agent_dispatch_active ON runtime_agents(json_extract(record,'$.rootId')) WHERE json_extract(record,'$.inFlight')=1 OR json_extract(record,'$.status') IN ('running','starting','approval');
CREATE INDEX runtime_agent_dispatch_busy_input ON runtime_agents(json_extract(record,'$.rootId')) WHERE json_extract(record,'$.startAttempt.activeAtReservation')=1;
CREATE INDEX runtime_agent_dispatch_workspace ON runtime_agents(json_extract(record,'$.cwd')) WHERE json_type(record,'$.workspaceOperation')='text' AND json_extract(record,'$.workspaceOperation')!='';
CREATE INDEX runtime_agent_global_active ON runtime_agents(
                    json_extract(record,'$.status')) WHERE json_extract(record,'$.status') IN
                    ('running','starting','approval');
CREATE INDEX runtime_agent_inflight ON runtime_agents(
                    json_extract(record,'$.inFlight')) WHERE json_extract(record,'$.inFlight')=1;
CREATE INDEX runtime_agent_live_root ON runtime_agents(
                    json_extract(record,'$.rootId'),id)
                    WHERE json_extract(record,'$.deletedAt') IS NULL;
CREATE INDEX runtime_agent_native_reset_pending ON runtime_agents(id)
                    WHERE json_extract(record,'$.nativeRelease.resetPending')=1;
CREATE INDEX runtime_agent_native_scope ON runtime_agents(
                    json_extract(record,'$.threadId'),
                    CASE WHEN json_type(record,'$.accountKey') IS NULL THEN 'default'
                         ELSE json_extract(record,'$.accountKey') END);
CREATE INDEX runtime_agent_parent ON runtime_agents(
                    json_extract(record,'$.parentId'));
CREATE TRIGGER runtime_agent_record_generation_delete AFTER DELETE ON runtime_agents BEGIN UPDATE runtime_agent_record_generation SET value=value+1 WHERE id=1; UPDATE runtime_scheduler_agent_change_state SET entries=entries+CASE WHEN EXISTS(SELECT 1 FROM runtime_scheduler_agent_changes WHERE id=OLD.id) THEN 0 ELSE 1 END,total=total-COALESCE((SELECT generation FROM runtime_scheduler_agent_changes WHERE id=OLD.id),0)+(SELECT value FROM runtime_agent_record_generation WHERE id=1) WHERE id=1; INSERT INTO runtime_scheduler_agent_changes(id,generation) SELECT OLD.id,value FROM runtime_agent_record_generation WHERE id=1 ON CONFLICT(id) DO UPDATE SET generation=excluded.generation; UPDATE runtime_scheduler_agent_change_state SET generation=(SELECT value FROM runtime_agent_record_generation WHERE id=1) WHERE id=1; END;
CREATE TRIGGER runtime_agent_record_generation_insert AFTER INSERT ON runtime_agents BEGIN UPDATE runtime_agent_record_generation SET value=value+1 WHERE id=1; UPDATE runtime_scheduler_agent_change_state SET entries=entries+CASE WHEN EXISTS(SELECT 1 FROM runtime_scheduler_agent_changes WHERE id=NEW.id) THEN 0 ELSE 1 END,total=total-COALESCE((SELECT generation FROM runtime_scheduler_agent_changes WHERE id=NEW.id),0)+(SELECT value FROM runtime_agent_record_generation WHERE id=1) WHERE id=1; INSERT INTO runtime_scheduler_agent_changes(id,generation) SELECT NEW.id,value FROM runtime_agent_record_generation WHERE id=1 ON CONFLICT(id) DO UPDATE SET generation=excluded.generation; UPDATE runtime_scheduler_agent_change_state SET generation=(SELECT value FROM runtime_agent_record_generation WHERE id=1) WHERE id=1; END;
CREATE TRIGGER runtime_agent_record_generation_update AFTER UPDATE ON runtime_agents WHEN OLD.record IS NOT NEW.record OR OLD.id IS NOT NEW.id OR OLD.rowid IS NOT NEW.rowid BEGIN UPDATE runtime_agent_record_generation SET value=value+1 WHERE id=1; UPDATE runtime_scheduler_agent_change_state SET entries=entries+CASE WHEN EXISTS(SELECT 1 FROM runtime_scheduler_agent_changes WHERE id=OLD.id) THEN 0 ELSE 1 END,total=total-COALESCE((SELECT generation FROM runtime_scheduler_agent_changes WHERE id=OLD.id),0)+(SELECT value FROM runtime_agent_record_generation WHERE id=1) WHERE id=1 AND OLD.id IS NOT NEW.id; INSERT INTO runtime_scheduler_agent_changes(id,generation) SELECT OLD.id,value FROM runtime_agent_record_generation WHERE id=1 AND OLD.id IS NOT NEW.id ON CONFLICT(id) DO UPDATE SET generation=excluded.generation; UPDATE runtime_scheduler_agent_change_state SET entries=entries+CASE WHEN EXISTS(SELECT 1 FROM runtime_scheduler_agent_changes WHERE id=NEW.id) THEN 0 ELSE 1 END,total=total-COALESCE((SELECT generation FROM runtime_scheduler_agent_changes WHERE id=NEW.id),0)+(SELECT value FROM runtime_agent_record_generation WHERE id=1) WHERE id=1; INSERT INTO runtime_scheduler_agent_changes(id,generation) SELECT NEW.id,value FROM runtime_agent_record_generation WHERE id=1 ON CONFLICT(id) DO UPDATE SET generation=excluded.generation; UPDATE runtime_scheduler_agent_change_state SET generation=(SELECT value FROM runtime_agent_record_generation WHERE id=1) WHERE id=1; END;
CREATE INDEX runtime_agent_reservation_cwd ON runtime_agents(
                    json_extract(record,'$.cwd'))
                    WHERE json_type(record,'$.workspaceOperation')='text'
                      AND json_extract(record,'$.workspaceOperation')!='';
CREATE INDEX runtime_agent_root ON runtime_agents(
                    json_extract(record,'$.rootId'));
CREATE INDEX runtime_budget_usage_agent ON runtime_budget_usage(agent,kind);
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
CREATE INDEX runtime_federation_nonce_age
          ON runtime_federation_nonces(created);
CREATE INDEX runtime_federation_outbox_peer
          ON runtime_federation_outbox(peer_id);
CREATE INDEX runtime_item_agent ON runtime_items(agent, created);
CREATE INDEX runtime_monitor_agent_status ON runtime_monitors(
                    json_extract(record,'$.agent'),json_extract(record,'$.status'));
CREATE INDEX runtime_monitor_stall_due ON runtime_monitors(
                    COALESCE(json_extract(record,'$.activityAt'),json_extract(record,'$.created'),1e99)+
                    COALESCE(json_extract(record,'$.stallTimeoutSeconds'),1800))
                    WHERE json_extract(record,'$.status')='running';
CREATE INDEX runtime_monitor_status ON runtime_monitors(json_extract(record,'$.status'),json_extract(record,'$.created'));
CREATE TRIGGER runtime_payload_checkpoints_delete AFTER DELETE ON runtime_checkpoints BEGIN
            DELETE FROM runtime_payload_refs WHERE table_name='runtime_checkpoints' AND record_id=OLD.id;
        END;
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
CREATE INDEX runtime_payload_refs_hash ON runtime_payload_refs(sha256);
CREATE TRIGGER runtime_payload_requests_delete AFTER DELETE ON runtime_tool_requests BEGIN
            DELETE FROM runtime_payload_refs WHERE table_name='runtime_tool_requests' AND record_id=OLD.id;
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
CREATE TRIGGER runtime_payload_results_delete AFTER DELETE ON runtime_tool_results BEGIN
            DELETE FROM runtime_payload_refs WHERE table_name='runtime_tool_results' AND record_id=OLD.id;
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
CREATE TRIGGER runtime_turn_item_link_delete_v1 AFTER DELETE ON runtime_items BEGIN DELETE FROM runtime_turn_item_links_v1 WHERE id=OLD.id;  END;
CREATE TRIGGER runtime_turn_item_link_insert_v1 AFTER INSERT ON runtime_items BEGIN INSERT INTO runtime_turn_item_links_v1(id,agent,turn_id,created,epoch) VALUES (NEW.id,NEW.agent,json_extract(NEW.record,'$.turnId'),NEW.created,COALESCE((SELECT epoch FROM runtime_turn_item_link_state_v1 WHERE id=1),'')) ON CONFLICT(id) DO UPDATE SET agent=excluded.agent,turn_id=excluded.turn_id,created=excluded.created,epoch=excluded.epoch WHERE runtime_turn_item_links_v1.agent IS NOT excluded.agent OR runtime_turn_item_links_v1.turn_id IS NOT excluded.turn_id OR runtime_turn_item_links_v1.created IS NOT excluded.created OR runtime_turn_item_links_v1.epoch IS NOT excluded.epoch; END;
CREATE INDEX runtime_turn_item_link_scope_v1 ON runtime_turn_item_links_v1(agent,turn_id,epoch,created,id);
CREATE TRIGGER runtime_turn_item_link_update_v1 AFTER UPDATE ON runtime_items WHEN OLD.id IS NOT NEW.id OR OLD.agent IS NOT NEW.agent OR OLD.record IS NOT NEW.record OR OLD.created IS NOT NEW.created OR OLD.rowid IS NOT NEW.rowid BEGIN DELETE FROM runtime_turn_item_links_v1 WHERE id=OLD.id AND OLD.id IS NOT NEW.id; INSERT INTO runtime_turn_item_links_v1(id,agent,turn_id,created,epoch) VALUES (NEW.id,NEW.agent,json_extract(NEW.record,'$.turnId'),NEW.created,COALESCE((SELECT epoch FROM runtime_turn_item_link_state_v1 WHERE id=1),'')) ON CONFLICT(id) DO UPDATE SET agent=excluded.agent,turn_id=excluded.turn_id,created=excluded.created,epoch=excluded.epoch WHERE runtime_turn_item_links_v1.agent IS NOT excluded.agent OR runtime_turn_item_links_v1.turn_id IS NOT excluded.turn_id OR runtime_turn_item_links_v1.created IS NOT excluded.created OR runtime_turn_item_links_v1.epoch IS NOT excluded.epoch; END;
CREATE INDEX runtime_usage_resume_account ON runtime_usage_resumes(json_extract(record,'$.accountKey'),json_extract(record,'$.status'));
CREATE INDEX runtime_usage_resume_agent ON runtime_usage_resumes(agent);
CREATE INDEX runtime_usage_resume_due ON runtime_usage_resumes(json_extract(record,'$.status'),json_extract(record,'$.dueAt'));
CREATE INDEX runtime_work_archive_due ON runtime_work(
                COALESCE(json_extract(record,'$.archiveIntent.nextAttemptAt'),0),id)
                WHERE json_extract(record,'$.archiveIntent.status')='pending';
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
CREATE INDEX sync_scope_seq ON sync_documents(scope,seq);
COMMIT;
