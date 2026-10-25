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
INSERT INTO "runtime_agent_record_generation" VALUES(1,140);
CREATE TABLE runtime_agents (id TEXT PRIMARY KEY, record TEXT NOT NULL);
INSERT INTO "runtime_agents" VALUES('abf56fbb-f8dc-4241-ad30-3960b0afa460','{"id": "abf56fbb-f8dc-4241-ad30-3960b0afa460", "threadId": "thread-0", "accountKey": "default", "executionSettingsAccountKey": "default", "provider": "codex", "yoloMode": true, "name": "Callback lead", "prompt": "fixture", "cwd": "__CALLBACK_STATE__", "role": "orchestrator", "isLead": true, "needsTitle": false, "parentId": null, "rootId": "abf56fbb-f8dc-4241-ad30-3960b0afa460", "subagentConcurrencyVersion": 2, "maxAgentsExplicit": false, "model": "gpt-6-astra", "effort": "medium", "fastMode": false, "daybreakEnabled": false, "cyberAccessProgram": "standard", "concurrency": 32, "maxAgents": 64, "tokenBudget": null, "status": "running", "autoWake": true, "epoch": 3, "turnId": "turn-0", "inFlight": false, "turnEpoch": 3, "usageResumeEnabled": true, "tokensUsed": 0, "compactions": 0, "compactionsObservedOnly": false, "contextUsage": null, "events": 0, "created": 1792684446.7842586, "error": null, "profileId": null, "profileInstructions": "", "tail": "", "worktree": false, "workspaceMode": "worktree", "executionMode": "native", "hostProjectPath": "__CALLBACK_STATE__", "layrReady": false, "workspaceBackend": null, "workspaceModeExplicit": false, "workspaceInGit": false, "worktreeReady": false, "environment": "host", "worktreeWarning": null, "workerBaseRef": null, "workerBaseCommit": null, "workerBaseBehindMain": null, "workerBaseMainRef": null, "workerDefaults": {"model": "gpt-6-luna", "effort": "high", "fastMode": false, "daybreakEnabled": false, "delegationDepth": 1}, "reviewDefaults": {"model": null, "effort": null}, "agentMode": "multi", "agentModeRevision": 0, "agentModeSupported": true}');
INSERT INTO "runtime_agents" VALUES('3e73e736-e287-42b7-963c-6bef9312a291','{"id": "3e73e736-e287-42b7-963c-6bef9312a291", "threadId": "thread-1", "accountKey": "default", "executionSettingsAccountKey": "default", "provider": "codex", "yoloMode": true, "name": "Callback worker", "prompt": "fixture", "cwd": "__CALLBACK_STATE__", "role": "orchestrator", "isLead": false, "needsTitle": false, "parentId": "abf56fbb-f8dc-4241-ad30-3960b0afa460", "rootId": "abf56fbb-f8dc-4241-ad30-3960b0afa460", "subagentConcurrencyVersion": 2, "maxAgentsExplicit": false, "model": "gpt-6-astra", "effort": "medium", "fastMode": false, "daybreakEnabled": false, "cyberAccessProgram": "standard", "concurrency": 32, "maxAgents": 64, "tokenBudget": null, "status": "running", "autoWake": true, "epoch": 3, "turnId": "turn-1", "inFlight": false, "turnEpoch": 3, "usageResumeEnabled": true, "tokensUsed": 0, "compactions": 0, "compactionsObservedOnly": false, "contextUsage": null, "events": 0, "created": 1792684446.7901473, "error": null, "profileId": null, "profileInstructions": "", "tail": "", "worktree": false, "workspaceMode": "worktree", "executionMode": "native", "hostProjectPath": "__CALLBACK_STATE__", "layrReady": false, "workspaceBackend": null, "workspaceModeExplicit": false, "workspaceInGit": false, "worktreeReady": false, "environment": "host", "worktreeWarning": null, "workerBaseRef": null, "workerBaseCommit": null, "workerBaseBehindMain": null, "workerBaseMainRef": null, "workerDefaults": {"model": "gpt-6-luna", "effort": "high", "fastMode": false, "daybreakEnabled": false, "delegationDepth": 1}, "reviewDefaults": {"model": null, "effort": null}, "agentMode": "multi", "agentModeRevision": 0, "agentModeSupported": true}');
INSERT INTO "runtime_agents" VALUES('7c2692e5-4303-49f4-8ba0-cdc5a6f1db4a','{"id": "7c2692e5-4303-49f4-8ba0-cdc5a6f1db4a", "threadId": "thread-2", "accountKey": "default", "executionSettingsAccountKey": "default", "provider": "codex", "yoloMode": true, "name": "Other team", "prompt": "fixture", "cwd": "__CALLBACK_STATE__", "role": "orchestrator", "isLead": true, "needsTitle": false, "parentId": null, "rootId": "7c2692e5-4303-49f4-8ba0-cdc5a6f1db4a", "subagentConcurrencyVersion": 2, "maxAgentsExplicit": false, "model": "gpt-6-astra", "effort": "medium", "fastMode": false, "daybreakEnabled": false, "cyberAccessProgram": "standard", "concurrency": 32, "maxAgents": 64, "tokenBudget": null, "status": "running", "autoWake": true, "epoch": 3, "turnId": "turn-2", "inFlight": false, "turnEpoch": 3, "usageResumeEnabled": true, "tokensUsed": 0, "compactions": 0, "compactionsObservedOnly": false, "contextUsage": null, "events": 0, "created": 1792684446.7960482, "error": null, "profileId": null, "profileInstructions": "", "tail": "", "worktree": false, "workspaceMode": "worktree", "executionMode": "native", "hostProjectPath": "__CALLBACK_STATE__", "layrReady": false, "workspaceBackend": null, "workspaceModeExplicit": false, "workspaceInGit": false, "worktreeReady": false, "environment": "host", "worktreeWarning": null, "workerBaseRef": null, "workerBaseCommit": null, "workerBaseBehindMain": null, "workerBaseMainRef": null, "workerDefaults": {"model": "gpt-6-luna", "effort": "high", "fastMode": false, "daybreakEnabled": false, "delegationDepth": 1}, "reviewDefaults": {"model": null, "effort": null}, "agentMode": "multi", "agentModeRevision": 0, "agentModeSupported": true}');
INSERT INTO "runtime_agents" VALUES('aaaaaaaa-1111-4111-8111-111111111111','{"id": "aaaaaaaa-1111-4111-8111-111111111111", "threadId": "thread-1", "accountKey": "default", "executionSettingsAccountKey": "default", "provider": "codex", "yoloMode": true, "name": "Callback worker", "prompt": "fixture", "cwd": "__CALLBACK_STATE__", "role": "orchestrator", "isLead": false, "needsTitle": false, "parentId": "abf56fbb-f8dc-4241-ad30-3960b0afa460", "rootId": "abf56fbb-f8dc-4241-ad30-3960b0afa460", "subagentConcurrencyVersion": 2, "maxAgentsExplicit": false, "model": "gpt-6-astra", "effort": "medium", "fastMode": false, "daybreakEnabled": false, "cyberAccessProgram": "standard", "maxAgents": 64, "tokenBudget": null, "status": "interrupted", "autoWake": false, "epoch": 3, "turnId": "turn-1", "inFlight": false, "turnEpoch": 3, "usageResumeEnabled": true, "tokensUsed": 0, "compactions": 0, "compactionsObservedOnly": false, "contextUsage": null, "events": 0, "created": 1792684446.7901473, "error": "Server restarted during a turn. Review history, then send a new instruction.", "profileId": null, "profileInstructions": "", "tail": "", "worktree": false, "workspaceMode": "worktree", "executionMode": "native", "hostProjectPath": "__CALLBACK_STATE__", "layrReady": false, "workspaceBackend": null, "workspaceModeExplicit": false, "workspaceInGit": false, "worktreeReady": false, "environment": "host", "worktreeWarning": null, "workerBaseRef": null, "workerBaseCommit": null, "workerBaseBehindMain": null, "workerBaseMainRef": null, "workerDefaults": {"model": "gpt-6-luna", "effort": "high", "fastMode": false, "daybreakEnabled": false, "delegationDepth": 1}, "reviewDefaults": {"model": null, "effort": null}, "agentMode": "multi", "agentModeRevision": 0, "agentModeSupported": true, "deletedAt": null, "restartRecovery": {"epoch": 3, "accountKey": "default", "threadId": "thread-1", "turnId": "turn-1", "autoWake": true, "at": 1792684447.7965105, "stage": "pending", "startAttempt": null}, "disconnectRecovery": {"epoch": 3, "accountKey": "default", "threadId": "thread-1", "turnId": "turn-1", "autoWake": true, "at": 1792684447.7965105, "source": "restart"}, "supervisorRestore": {"status": "not_restored", "reason": "supervisor_mode_disabled", "accountKey": "default", "at": 1792684447.8197367}}');
INSERT INTO "runtime_agents" VALUES('aaaaaaaa-2222-4222-8222-222222222222','{"id": "aaaaaaaa-2222-4222-8222-222222222222", "threadId": "thread-1", "accountKey": "default", "executionSettingsAccountKey": "default", "provider": "codex", "yoloMode": true, "name": "Callback worker", "prompt": "fixture", "cwd": "__CALLBACK_STATE__", "role": "orchestrator", "isLead": false, "needsTitle": false, "parentId": "abf56fbb-f8dc-4241-ad30-3960b0afa460", "rootId": "abf56fbb-f8dc-4241-ad30-3960b0afa460", "subagentConcurrencyVersion": 2, "maxAgentsExplicit": false, "model": "gpt-6-astra", "effort": "medium", "fastMode": false, "daybreakEnabled": false, "cyberAccessProgram": "standard", "maxAgents": 64, "tokenBudget": null, "status": "interrupted", "autoWake": false, "epoch": 3, "turnId": "turn-1", "inFlight": false, "turnEpoch": 3, "usageResumeEnabled": true, "tokensUsed": 0, "compactions": 0, "compactionsObservedOnly": false, "contextUsage": null, "events": 0, "created": 1792684446.7901473, "error": "Server restarted during a turn. Review history, then send a new instruction.", "profileId": null, "profileInstructions": "", "tail": "", "worktree": false, "workspaceMode": "worktree", "executionMode": "native", "hostProjectPath": "__CALLBACK_STATE__", "layrReady": false, "workspaceBackend": null, "workspaceModeExplicit": false, "workspaceInGit": false, "worktreeReady": false, "environment": "host", "worktreeWarning": null, "workerBaseRef": null, "workerBaseCommit": null, "workerBaseBehindMain": null, "workerBaseMainRef": null, "workerDefaults": {"model": "gpt-6-luna", "effort": "high", "fastMode": false, "daybreakEnabled": false, "delegationDepth": 1}, "reviewDefaults": {"model": null, "effort": null}, "agentMode": "multi", "agentModeRevision": 0, "agentModeSupported": true, "deletedAt": null, "restartRecovery": {"epoch": 3, "accountKey": "default", "threadId": "thread-1", "turnId": "turn-1", "autoWake": true, "at": 1792684447.7969077, "stage": "pending", "startAttempt": null}, "disconnectRecovery": {"epoch": 3, "accountKey": "default", "threadId": "thread-1", "turnId": "turn-1", "autoWake": true, "at": 1792684447.7969077, "source": "restart"}, "supervisorRestore": {"status": "not_restored", "reason": "supervisor_mode_disabled", "accountKey": "default", "at": 1792684447.8197367}}');
INSERT INTO "runtime_agents" VALUES('bbbbbbbb-1111-4111-8111-111111111111','{"id": "bbbbbbbb-1111-4111-8111-111111111111", "threadId": "thread-2", "accountKey": "default", "executionSettingsAccountKey": "default", "provider": "codex", "yoloMode": true, "name": "Other team", "prompt": "fixture", "cwd": "__CALLBACK_STATE__", "role": "orchestrator", "isLead": true, "needsTitle": false, "parentId": null, "rootId": "bbbbbbbb-1111-4111-8111-111111111111", "subagentConcurrencyVersion": 2, "maxAgentsExplicit": false, "model": "gpt-6-astra", "effort": "medium", "fastMode": false, "daybreakEnabled": false, "cyberAccessProgram": "standard", "concurrency": 32, "maxAgents": 64, "tokenBudget": null, "status": "interrupted", "autoWake": false, "epoch": 3, "turnId": "turn-2", "inFlight": false, "turnEpoch": 3, "usageResumeEnabled": true, "tokensUsed": 0, "compactions": 0, "compactionsObservedOnly": false, "contextUsage": null, "events": 0, "created": 1792684446.7960482, "error": "Server restarted during a turn. Review history, then send a new instruction.", "profileId": null, "profileInstructions": "", "tail": "", "worktree": false, "workspaceMode": "worktree", "executionMode": "native", "hostProjectPath": "__CALLBACK_STATE__", "layrReady": false, "workspaceBackend": null, "workspaceModeExplicit": false, "workspaceInGit": false, "worktreeReady": false, "environment": "host", "worktreeWarning": null, "workerBaseRef": null, "workerBaseCommit": null, "workerBaseBehindMain": null, "workerBaseMainRef": null, "workerDefaults": {"model": "gpt-6-luna", "effort": "high", "fastMode": false, "daybreakEnabled": false, "delegationDepth": 1}, "reviewDefaults": {"model": null, "effort": null}, "agentMode": "multi", "agentModeRevision": 0, "agentModeSupported": true, "deletedAt": null, "restartRecovery": {"epoch": 3, "accountKey": "default", "threadId": "thread-2", "turnId": "turn-2", "autoWake": true, "at": 1792684447.7972267, "stage": "pending", "startAttempt": null}, "disconnectRecovery": {"epoch": 3, "accountKey": "default", "threadId": "thread-2", "turnId": "turn-2", "autoWake": true, "at": 1792684447.7972267, "source": "restart"}, "supervisorRestore": {"status": "not_restored", "reason": "supervisor_mode_disabled", "accountKey": "default", "at": 1792684447.8197367}}');
INSERT INTO "runtime_agents" VALUES('bbbbbbbb-2222-4222-8222-222222222222','{"id": "bbbbbbbb-2222-4222-8222-222222222222", "threadId": "thread-1", "accountKey": "default", "executionSettingsAccountKey": "default", "provider": "codex", "yoloMode": true, "name": "Callback worker", "prompt": "fixture", "cwd": "__CALLBACK_STATE__", "role": "orchestrator", "isLead": false, "needsTitle": false, "parentId": "abf56fbb-f8dc-4241-ad30-3960b0afa460", "rootId": "abf56fbb-f8dc-4241-ad30-3960b0afa460", "subagentConcurrencyVersion": 2, "maxAgentsExplicit": false, "model": "gpt-6-astra", "effort": "medium", "fastMode": false, "daybreakEnabled": false, "cyberAccessProgram": "standard", "maxAgents": 64, "tokenBudget": null, "status": "interrupted", "autoWake": false, "epoch": 3, "turnId": "turn-1", "inFlight": false, "turnEpoch": 3, "usageResumeEnabled": true, "tokensUsed": 0, "compactions": 0, "compactionsObservedOnly": false, "contextUsage": null, "events": 0, "created": 1792684446.7901473, "error": "Server restarted during a turn. Review history, then send a new instruction.", "profileId": null, "profileInstructions": "", "tail": "", "worktree": false, "workspaceMode": "worktree", "executionMode": "native", "hostProjectPath": "__CALLBACK_STATE__", "layrReady": false, "workspaceBackend": null, "workspaceModeExplicit": false, "workspaceInGit": false, "worktreeReady": false, "environment": "host", "worktreeWarning": null, "workerBaseRef": null, "workerBaseCommit": null, "workerBaseBehindMain": null, "workerBaseMainRef": null, "workerDefaults": {"model": "gpt-6-luna", "effort": "high", "fastMode": false, "daybreakEnabled": false, "delegationDepth": 1}, "reviewDefaults": {"model": null, "effort": null}, "agentMode": "multi", "agentModeRevision": 0, "agentModeSupported": true, "deletedAt": null, "restartRecovery": {"epoch": 3, "accountKey": "default", "threadId": "thread-1", "turnId": "turn-1", "autoWake": true, "at": 1792684447.797499, "stage": "pending", "startAttempt": null}, "disconnectRecovery": {"epoch": 3, "accountKey": "default", "threadId": "thread-1", "turnId": "turn-1", "autoWake": true, "at": 1792684447.797499, "source": "restart"}, "supervisorRestore": {"status": "not_restored", "reason": "supervisor_mode_disabled", "accountKey": "default", "at": 1792684447.8197367}}');
INSERT INTO "runtime_agents" VALUES('cccccccc-1111-4111-8111-111111111111','{"id": "cccccccc-1111-4111-8111-111111111111", "threadId": "thread-1", "accountKey": "default", "executionSettingsAccountKey": "default", "provider": "codex", "yoloMode": true, "name": "Callback worker", "prompt": "fixture", "cwd": "__CALLBACK_STATE__", "role": "orchestrator", "isLead": false, "needsTitle": false, "parentId": "abf56fbb-f8dc-4241-ad30-3960b0afa460", "rootId": "abf56fbb-f8dc-4241-ad30-3960b0afa460", "subagentConcurrencyVersion": 2, "maxAgentsExplicit": false, "model": "gpt-6-astra", "effort": "medium", "fastMode": false, "daybreakEnabled": false, "cyberAccessProgram": "standard", "maxAgents": 64, "tokenBudget": null, "status": "interrupted", "autoWake": false, "epoch": 3, "turnId": "turn-1", "inFlight": false, "turnEpoch": 3, "usageResumeEnabled": true, "tokensUsed": 0, "compactions": 0, "compactionsObservedOnly": false, "contextUsage": null, "events": 0, "created": 1792684446.7901473, "error": "Server restarted during a turn. Review history, then send a new instruction.", "profileId": null, "profileInstructions": "", "tail": "", "worktree": false, "workspaceMode": "worktree", "executionMode": "native", "hostProjectPath": "__CALLBACK_STATE__", "layrReady": false, "workspaceBackend": null, "workspaceModeExplicit": false, "workspaceInGit": false, "worktreeReady": false, "environment": "host", "worktreeWarning": null, "workerBaseRef": null, "workerBaseCommit": null, "workerBaseBehindMain": null, "workerBaseMainRef": null, "workerDefaults": {"model": "gpt-6-luna", "effort": "high", "fastMode": false, "daybreakEnabled": false, "delegationDepth": 1}, "reviewDefaults": {"model": null, "effort": null}, "agentMode": "multi", "agentModeRevision": 0, "agentModeSupported": true, "deletedAt": 15.0}');
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
INSERT INTO "runtime_event_meta" VALUES('monitor:6a1cbd51-e035-54ce-af5c-7fd9850e3065','{"delivery": "current_turn"}');
CREATE TABLE runtime_events (
                  id TEXT PRIMARY KEY, agent TEXT NOT NULL, kind TEXT NOT NULL,
                  text TEXT NOT NULL, status TEXT NOT NULL, created REAL NOT NULL,
                  epoch INTEGER NOT NULL, turn_id TEXT, error TEXT);
INSERT INTO "runtime_events" VALUES('monitor:6a1cbd51-e035-54ce-af5c-7fd9850e3065','abf56fbb-f8dc-4241-ad30-3960b0afa460','monitor_exit','{"id": "6a1cbd51-e035-54ce-af5c-7fd9850e3065", "command": "printf fixture", "status": "lost", "exitCode": null, "error": "Server restarted. Command outcome unknown; not rerun.", "tail": "", "log": "__CALLBACK_STATE__/monitor-logs/6a1cbd51-e035-54ce-af5c-7fd9850e3065.log", "bytes": 0}','pending',1792684447.798625,3,NULL,NULL);
CREATE TABLE runtime_execution_attempts (id TEXT PRIMARY KEY, run TEXT NOT NULL, record TEXT NOT NULL);
CREATE TABLE runtime_execution_effects (id TEXT PRIMARY KEY, run TEXT NOT NULL, kind TEXT NOT NULL, reference TEXT NOT NULL, record TEXT NOT NULL);
INSERT INTO "runtime_execution_effects" VALUES('requests:answer-0','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','answer-0','{"id": "requests:answer-0", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "answer-0", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:answer-1','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','answer-1','{"id": "requests:answer-1", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "answer-1", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:answer-2','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','answer-2','{"id": "requests:answer-2", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "answer-2", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:answer-3','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','answer-3','{"id": "requests:answer-3", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "answer-3", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:answer-4','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','answer-4','{"id": "requests:answer-4", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "answer-4", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:answer-5','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','answer-5','{"id": "requests:answer-5", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "answer-5", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:answer-6','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','answer-6','{"id": "requests:answer-6", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "answer-6", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:answer-7','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','answer-7','{"id": "requests:answer-7", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "answer-7", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:answer-8','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','answer-8','{"id": "requests:answer-8", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "answer-8", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:answer-9','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','answer-9','{"id": "requests:answer-9", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "answer-9", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:answer-10','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','answer-10','{"id": "requests:answer-10", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "answer-10", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:answer-11','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','answer-11','{"id": "requests:answer-11", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "answer-11", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:answer-12','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','answer-12','{"id": "requests:answer-12", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "answer-12", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:answer-13','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','answer-13','{"id": "requests:answer-13", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "answer-13", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:answer-14','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','answer-14','{"id": "requests:answer-14", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "answer-14", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:answer-15','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','answer-15','{"id": "requests:answer-15", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "answer-15", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:answer-16','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','answer-16','{"id": "requests:answer-16", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "answer-16", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:answer-17','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','answer-17','{"id": "requests:answer-17", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "answer-17", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:answer-18','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','answer-18','{"id": "requests:answer-18", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "answer-18", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:answer-19','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','answer-19','{"id": "requests:answer-19", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "answer-19", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:answer-20','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','answer-20','{"id": "requests:answer-20", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "answer-20", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:answer-21','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','answer-21','{"id": "requests:answer-21", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "answer-21", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:answer-22','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','answer-22','{"id": "requests:answer-22", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "answer-22", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:answer-23','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','answer-23','{"id": "requests:answer-23", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "answer-23", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:answer-24','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','answer-24','{"id": "requests:answer-24", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "answer-24", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:answer-25','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','answer-25','{"id": "requests:answer-25", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "answer-25", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:answer-26','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','answer-26','{"id": "requests:answer-26", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "answer-26", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:answer-27','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','answer-27','{"id": "requests:answer-27", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "answer-27", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:answer-28','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','answer-28','{"id": "requests:answer-28", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "answer-28", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:answer-29','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','answer-29','{"id": "requests:answer-29", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "answer-29", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('tool_requests:thread-0:alias-0','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','tool_requests','thread-0:alias-0','{"id": "tool_requests:thread-0:alias-0", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "tool_requests", "referenceId": "thread-0:alias-0", "attemptId": null, "requestId": "alias-0", "status": "interrupted", "outcome": "unknown", "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('tool_requests:thread-0:spawn:request-1','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','tool_requests','thread-0:spawn:request-1','{"id": "tool_requests:thread-0:spawn:request-1", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "tool_requests", "referenceId": "thread-0:spawn:request-1", "attemptId": null, "requestId": "request-1", "status": "cancelled", "outcome": "not_applied", "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('tool_requests:thread-0:send:request-2','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','tool_requests','thread-0:send:request-2','{"id": "tool_requests:thread-0:send:request-2", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "tool_requests", "referenceId": "thread-0:send:request-2", "attemptId": null, "requestId": "request-2", "status": "cancelled", "outcome": "not_applied", "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('tool_requests:thread-0:review:request-3','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','tool_requests','thread-0:review:request-3','{"id": "tool_requests:thread-0:review:request-3", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "tool_requests", "referenceId": "thread-0:review:request-3", "attemptId": null, "requestId": "request-3", "status": "cancelled", "outcome": "not_applied", "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('tool_requests:thread-0:server:request-4','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','tool_requests','thread-0:server:request-4','{"id": "tool_requests:thread-0:server:request-4", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "tool_requests", "referenceId": "thread-0:server:request-4", "attemptId": null, "requestId": "request-4", "status": "cancelled", "outcome": "not_applied", "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('tool_requests:thread-0:move:request-5','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','tool_requests','thread-0:move:request-5','{"id": "tool_requests:thread-0:move:request-5", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "tool_requests", "referenceId": "thread-0:move:request-5", "attemptId": null, "requestId": "request-5", "status": "cancelled", "outcome": "not_applied", "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('tool_requests:thread-0:move:request-6','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','tool_requests','thread-0:move:request-6','{"id": "tool_requests:thread-0:move:request-6", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "tool_requests", "referenceId": "thread-0:move:request-6", "attemptId": null, "requestId": "request-6", "status": "cancelled", "outcome": "not_applied", "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('tool_requests:thread-0:warmup','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','tool_requests','thread-0:warmup','{"id": "tool_requests:thread-0:warmup", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "tool_requests", "referenceId": "thread-0:warmup", "attemptId": null, "requestId": "warmup", "status": "completed", "outcome": "applied", "resultReference": "thread-0:warmup"}');
INSERT INTO "runtime_execution_effects" VALUES('tool_requests:thread-0:dynamic-0','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','tool_requests','thread-0:dynamic-0','{"id": "tool_requests:thread-0:dynamic-0", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "tool_requests", "referenceId": "thread-0:dynamic-0", "attemptId": null, "requestId": "dynamic-0", "status": "completed", "outcome": "applied", "resultReference": "thread-0:dynamic-0"}');
INSERT INTO "runtime_execution_effects" VALUES('tool_requests:thread-0:dynamic-1','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','tool_requests','thread-0:dynamic-1','{"id": "tool_requests:thread-0:dynamic-1", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "tool_requests", "referenceId": "thread-0:dynamic-1", "attemptId": null, "requestId": "dynamic-1", "status": "completed", "outcome": "applied", "resultReference": "thread-0:dynamic-1"}');
INSERT INTO "runtime_execution_effects" VALUES('tool_requests:thread-0:dynamic-2','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','tool_requests','thread-0:dynamic-2','{"id": "tool_requests:thread-0:dynamic-2", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "tool_requests", "referenceId": "thread-0:dynamic-2", "attemptId": null, "requestId": "dynamic-2", "status": "completed", "outcome": "applied", "resultReference": "thread-0:dynamic-2"}');
INSERT INTO "runtime_execution_effects" VALUES('spawn:aaaaaaaa-1111-4111-8111-111111111111','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','spawn','aaaaaaaa-1111-4111-8111-111111111111','{"id": "spawn:aaaaaaaa-1111-4111-8111-111111111111", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "spawn", "referenceId": "aaaaaaaa-1111-4111-8111-111111111111", "attemptId": null, "status": "running"}');
INSERT INTO "runtime_execution_effects" VALUES('spawn:aaaaaaaa-2222-4222-8222-222222222222','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','spawn','aaaaaaaa-2222-4222-8222-222222222222','{"id": "spawn:aaaaaaaa-2222-4222-8222-222222222222", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "spawn", "referenceId": "aaaaaaaa-2222-4222-8222-222222222222", "attemptId": null, "status": "running"}');
INSERT INTO "runtime_execution_effects" VALUES('spawn:bbbbbbbb-2222-4222-8222-222222222222','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','spawn','bbbbbbbb-2222-4222-8222-222222222222','{"id": "spawn:bbbbbbbb-2222-4222-8222-222222222222", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "spawn", "referenceId": "bbbbbbbb-2222-4222-8222-222222222222", "attemptId": null, "status": "running"}');
INSERT INTO "runtime_execution_effects" VALUES('spawn:cccccccc-1111-4111-8111-111111111111','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','spawn','cccccccc-1111-4111-8111-111111111111','{"id": "spawn:cccccccc-1111-4111-8111-111111111111", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "spawn", "referenceId": "cccccccc-1111-4111-8111-111111111111", "attemptId": null, "status": "running"}');
INSERT INTO "runtime_execution_effects" VALUES('requests:8b359a1f-85cd-4d54-adfd-733c9c049f3c','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','8b359a1f-85cd-4d54-adfd-733c9c049f3c','{"id": "requests:8b359a1f-85cd-4d54-adfd-733c9c049f3c", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "8b359a1f-85cd-4d54-adfd-733c9c049f3c", "attemptId": null, "requestId": null, "status": "pending", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:resolve-card','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','resolve-card','{"id": "requests:resolve-card", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "resolve-card", "attemptId": null, "requestId": null, "status": "resolved", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:python-written-card','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','python-written-card','{"id": "requests:python-written-card", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "python-written-card", "attemptId": null, "requestId": null, "status": "pending", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('monitors:6a1cbd51-e035-54ce-af5c-7fd9850e3065','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','monitors','6a1cbd51-e035-54ce-af5c-7fd9850e3065','{"id": "monitors:6a1cbd51-e035-54ce-af5c-7fd9850e3065", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "monitors", "referenceId": "6a1cbd51-e035-54ce-af5c-7fd9850e3065", "attemptId": null, "requestId": "managed-monitor", "status": "lost", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:roundtrip-0','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','roundtrip-0','{"id": "requests:roundtrip-0", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "roundtrip-0", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:roundtrip-1','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','roundtrip-1','{"id": "requests:roundtrip-1", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "roundtrip-1", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:roundtrip-2','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','roundtrip-2','{"id": "requests:roundtrip-2", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "roundtrip-2", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:roundtrip-3','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','roundtrip-3','{"id": "requests:roundtrip-3", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "roundtrip-3", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:roundtrip-4','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','roundtrip-4','{"id": "requests:roundtrip-4", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "roundtrip-4", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:roundtrip-5','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','roundtrip-5','{"id": "requests:roundtrip-5", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "roundtrip-5", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:roundtrip-6','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','roundtrip-6','{"id": "requests:roundtrip-6", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "roundtrip-6", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:roundtrip-7','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','roundtrip-7','{"id": "requests:roundtrip-7", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "roundtrip-7", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:roundtrip-8','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','roundtrip-8','{"id": "requests:roundtrip-8", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "roundtrip-8", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:roundtrip-9','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','roundtrip-9','{"id": "requests:roundtrip-9", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "roundtrip-9", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:roundtrip-10','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','roundtrip-10','{"id": "requests:roundtrip-10", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "roundtrip-10", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:roundtrip-11','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','roundtrip-11','{"id": "requests:roundtrip-11", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "roundtrip-11", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:roundtrip-12','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','roundtrip-12','{"id": "requests:roundtrip-12", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "roundtrip-12", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:roundtrip-13','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','roundtrip-13','{"id": "requests:roundtrip-13", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "roundtrip-13", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:roundtrip-14','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','roundtrip-14','{"id": "requests:roundtrip-14", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "roundtrip-14", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:roundtrip-15','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','roundtrip-15','{"id": "requests:roundtrip-15", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "roundtrip-15", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:roundtrip-16','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','roundtrip-16','{"id": "requests:roundtrip-16", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "roundtrip-16", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:roundtrip-17','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','roundtrip-17','{"id": "requests:roundtrip-17", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "roundtrip-17", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:roundtrip-18','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','roundtrip-18','{"id": "requests:roundtrip-18", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "roundtrip-18", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:roundtrip-19','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','roundtrip-19','{"id": "requests:roundtrip-19", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "roundtrip-19", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:roundtrip-20','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','roundtrip-20','{"id": "requests:roundtrip-20", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "roundtrip-20", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:roundtrip-21','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','roundtrip-21','{"id": "requests:roundtrip-21", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "roundtrip-21", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:roundtrip-22','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','roundtrip-22','{"id": "requests:roundtrip-22", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "roundtrip-22", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:roundtrip-23','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','roundtrip-23','{"id": "requests:roundtrip-23", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "roundtrip-23", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:roundtrip-24','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','roundtrip-24','{"id": "requests:roundtrip-24", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "roundtrip-24", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:roundtrip-25','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','roundtrip-25','{"id": "requests:roundtrip-25", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "roundtrip-25", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:roundtrip-26','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','roundtrip-26','{"id": "requests:roundtrip-26", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "roundtrip-26", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:roundtrip-27','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','roundtrip-27','{"id": "requests:roundtrip-27", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "roundtrip-27", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:roundtrip-28','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','roundtrip-28','{"id": "requests:roundtrip-28", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "roundtrip-28", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:roundtrip-29','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','roundtrip-29','{"id": "requests:roundtrip-29", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "roundtrip-29", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:delete-0','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','delete-0','{"id": "requests:delete-0", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "delete-0", "attemptId": null, "requestId": null, "status": "deleted", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:delete-1','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','delete-1','{"id": "requests:delete-1", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "delete-1", "attemptId": null, "requestId": null, "status": "deleted", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:delete-2','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','delete-2','{"id": "requests:delete-2", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "delete-2", "attemptId": null, "requestId": null, "status": "deleted", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:delete-3','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','delete-3','{"id": "requests:delete-3", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "delete-3", "attemptId": null, "requestId": null, "status": "deleted", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:delete-4','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','delete-4','{"id": "requests:delete-4", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "delete-4", "attemptId": null, "requestId": null, "status": "deleted", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:delete-5','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','delete-5','{"id": "requests:delete-5", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "delete-5", "attemptId": null, "requestId": null, "status": "answering", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:delete-6','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','delete-6','{"id": "requests:delete-6", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "delete-6", "attemptId": null, "requestId": null, "status": "uncertain", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:delete-7','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','delete-7','{"id": "requests:delete-7", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "delete-7", "attemptId": null, "requestId": null, "status": "deleted", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:delete-8','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','delete-8','{"id": "requests:delete-8", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "delete-8", "attemptId": null, "requestId": null, "status": "deleted", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:delete-9','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','delete-9','{"id": "requests:delete-9", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "delete-9", "attemptId": null, "requestId": null, "status": "deleted", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:delete-10','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','delete-10','{"id": "requests:delete-10", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "delete-10", "attemptId": null, "requestId": null, "status": "deleted", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:delete-11','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','delete-11','{"id": "requests:delete-11", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "delete-11", "attemptId": null, "requestId": null, "status": "deleted", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:delete-12','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','delete-12','{"id": "requests:delete-12", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "delete-12", "attemptId": null, "requestId": null, "status": "answering", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:delete-13','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','delete-13','{"id": "requests:delete-13", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "delete-13", "attemptId": null, "requestId": null, "status": "uncertain", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:delete-14','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','delete-14','{"id": "requests:delete-14", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "delete-14", "attemptId": null, "requestId": null, "status": "deleted", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:delete-15','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','delete-15','{"id": "requests:delete-15", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "delete-15", "attemptId": null, "requestId": null, "status": "deleted", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:delete-16','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','delete-16','{"id": "requests:delete-16", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "delete-16", "attemptId": null, "requestId": null, "status": "deleted", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:delete-17','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','delete-17','{"id": "requests:delete-17", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "delete-17", "attemptId": null, "requestId": null, "status": "deleted", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:delete-18','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','delete-18','{"id": "requests:delete-18", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "delete-18", "attemptId": null, "requestId": null, "status": "deleted", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:delete-19','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','delete-19','{"id": "requests:delete-19", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "delete-19", "attemptId": null, "requestId": null, "status": "answering", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:delete-20','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','delete-20','{"id": "requests:delete-20", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "delete-20", "attemptId": null, "requestId": null, "status": "uncertain", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:failure-answer-notSent','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','failure-answer-notSent','{"id": "requests:failure-answer-notSent", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "failure-answer-notSent", "attemptId": null, "requestId": null, "status": "pending", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:failure-answer-unknown','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','failure-answer-unknown','{"id": "requests:failure-answer-unknown", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "failure-answer-unknown", "attemptId": null, "requestId": null, "status": "uncertain", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:failure-delete-notSent','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','failure-delete-notSent','{"id": "requests:failure-delete-notSent", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "failure-delete-notSent", "attemptId": null, "requestId": null, "status": "pending", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:failure-delete-unknown','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','failure-delete-unknown','{"id": "requests:failure-delete-unknown", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "failure-delete-unknown", "attemptId": null, "requestId": null, "status": "uncertain", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:invalid-0','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','invalid-0','{"id": "requests:invalid-0", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "invalid-0", "attemptId": null, "requestId": null, "status": "pending", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:invalid-1','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','invalid-1','{"id": "requests:invalid-1", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "invalid-1", "attemptId": null, "requestId": null, "status": "pending", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:invalid-2','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','invalid-2','{"id": "requests:invalid-2", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "invalid-2", "attemptId": null, "requestId": null, "status": "pending", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:invalid-3','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','invalid-3','{"id": "requests:invalid-3", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "invalid-3", "attemptId": null, "requestId": null, "status": "pending", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:invalid-4','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','invalid-4','{"id": "requests:invalid-4", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "invalid-4", "attemptId": null, "requestId": null, "status": "pending", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:invalid-5','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','invalid-5','{"id": "requests:invalid-5", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "invalid-5", "attemptId": null, "requestId": null, "status": "pending", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:invalid-6','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','invalid-6','{"id": "requests:invalid-6", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "invalid-6", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:invalid-7','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','invalid-7','{"id": "requests:invalid-7", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "invalid-7", "attemptId": null, "requestId": null, "status": "pending", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:invalid-8','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','invalid-8','{"id": "requests:invalid-8", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "invalid-8", "attemptId": null, "requestId": null, "status": "pending", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:invalid-9','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','invalid-9','{"id": "requests:invalid-9", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "invalid-9", "attemptId": null, "requestId": null, "status": "pending", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:invalid-10','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','invalid-10','{"id": "requests:invalid-10", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "invalid-10", "attemptId": null, "requestId": null, "status": "pending", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:invalid-11','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','invalid-11','{"id": "requests:invalid-11", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "invalid-11", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:race-completed','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','race-completed','{"id": "requests:race-completed", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "race-completed", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:race-failed','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','race-failed','{"id": "requests:race-failed", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "race-failed", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
INSERT INTO "runtime_execution_effects" VALUES('requests:race-resolved','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','requests','race-resolved','{"id": "requests:race-resolved", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "requests", "referenceId": "race-resolved", "attemptId": null, "requestId": null, "status": "answered", "outcome": null, "resultReference": null}');
CREATE TABLE runtime_execution_inputs (run TEXT NOT NULL, event TEXT NOT NULL, PRIMARY KEY(run,event));
CREATE TABLE runtime_execution_nodes (id TEXT PRIMARY KEY, run TEXT NOT NULL, record TEXT NOT NULL);
INSERT INTO "runtime_execution_nodes" VALUES('worker:aaaaaaaa-1111-4111-8111-111111111111','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','{"id": "worker:aaaaaaaa-1111-4111-8111-111111111111", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "managed_worker", "agentId": "aaaaaaaa-1111-4111-8111-111111111111", "status": "interrupted"}');
INSERT INTO "runtime_execution_nodes" VALUES('worker:aaaaaaaa-2222-4222-8222-222222222222','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','{"id": "worker:aaaaaaaa-2222-4222-8222-222222222222", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "managed_worker", "agentId": "aaaaaaaa-2222-4222-8222-222222222222", "status": "interrupted"}');
INSERT INTO "runtime_execution_nodes" VALUES('worker:bbbbbbbb-2222-4222-8222-222222222222','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','{"id": "worker:bbbbbbbb-2222-4222-8222-222222222222", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "managed_worker", "agentId": "bbbbbbbb-2222-4222-8222-222222222222", "status": "interrupted"}');
INSERT INTO "runtime_execution_nodes" VALUES('worker:cccccccc-1111-4111-8111-111111111111','run:18c363e5-2b4a-57a2-a086-76339fa02f4e','{"id": "worker:cccccccc-1111-4111-8111-111111111111", "runId": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "kind": "managed_worker", "agentId": "cccccccc-1111-4111-8111-111111111111", "status": "interrupted"}');
CREATE TABLE runtime_execution_runs (id TEXT PRIMARY KEY, agent TEXT NOT NULL, account TEXT NOT NULL, epoch INTEGER NOT NULL, thread TEXT, turn TEXT, created REAL NOT NULL, record TEXT NOT NULL);
INSERT INTO "runtime_execution_runs" VALUES('run:18c363e5-2b4a-57a2-a086-76339fa02f4e','abf56fbb-f8dc-4241-ad30-3960b0afa460','default',3,'thread-0','turn-0',1792684446.8000987,'{"id": "run:18c363e5-2b4a-57a2-a086-76339fa02f4e", "agent": "abf56fbb-f8dc-4241-ad30-3960b0afa460", "accountKey": "default", "epoch": 3, "threadId": "thread-0", "turnId": "turn-0", "created": 1792684446.8000987, "status": "running", "firstAttemptId": null, "rootAttemptId": null, "latestAttemptId": null, "reconciled": true, "restartRecovery": {"epoch": 3, "accountKey": "default", "threadId": "thread-0", "turnId": "turn-0", "autoWake": true, "at": 1792684447.7894883, "stage": "pending", "startAttempt": null}, "disconnectRecovery": {"epoch": 3, "accountKey": "default", "threadId": "thread-0", "turnId": "turn-0", "autoWake": true, "at": 1792684447.7894883, "source": "restart"}}');
INSERT INTO "runtime_execution_runs" VALUES('run:ca8cf778-ff24-526e-82db-707ddddaf9f8','3e73e736-e287-42b7-963c-6bef9312a291','default',3,'thread-1','turn-1',1792684446.8035574,'{"id": "run:ca8cf778-ff24-526e-82db-707ddddaf9f8", "agent": "3e73e736-e287-42b7-963c-6bef9312a291", "accountKey": "default", "epoch": 3, "threadId": "thread-1", "turnId": "turn-1", "created": 1792684446.8035574, "status": "running", "firstAttemptId": null, "rootAttemptId": null, "latestAttemptId": null, "reconciled": true, "restartRecovery": {"epoch": 3, "accountKey": "default", "threadId": "thread-1", "turnId": "turn-1", "autoWake": true, "at": 1792684447.7954564, "stage": "pending", "startAttempt": null}, "disconnectRecovery": {"epoch": 3, "accountKey": "default", "threadId": "thread-1", "turnId": "turn-1", "autoWake": true, "at": 1792684447.7954564, "source": "restart"}}');
INSERT INTO "runtime_execution_runs" VALUES('run:b334b0de-1363-5393-906f-3d49e1e9ab8b','7c2692e5-4303-49f4-8ba0-cdc5a6f1db4a','default',3,'thread-2','turn-2',1792684446.8077269,'{"id": "run:b334b0de-1363-5393-906f-3d49e1e9ab8b", "agent": "7c2692e5-4303-49f4-8ba0-cdc5a6f1db4a", "accountKey": "default", "epoch": 3, "threadId": "thread-2", "turnId": "turn-2", "created": 1792684446.8077269, "status": "running", "firstAttemptId": null, "rootAttemptId": null, "latestAttemptId": null, "reconciled": true, "restartRecovery": {"epoch": 3, "accountKey": "default", "threadId": "thread-2", "turnId": "turn-2", "autoWake": true, "at": 1792684447.7961004, "stage": "pending", "startAttempt": null}, "disconnectRecovery": {"epoch": 3, "accountKey": "default", "threadId": "thread-2", "turnId": "turn-2", "autoWake": true, "at": 1792684447.7961004, "source": "restart"}}');
INSERT INTO "runtime_execution_runs" VALUES('run:3d30ec42-08b4-5705-a9f1-265b1e23298c','aaaaaaaa-1111-4111-8111-111111111111','default',3,'thread-1','turn-1',1792684447.3010902,'{"id": "run:3d30ec42-08b4-5705-a9f1-265b1e23298c", "agent": "aaaaaaaa-1111-4111-8111-111111111111", "accountKey": "default", "epoch": 3, "threadId": "thread-1", "turnId": "turn-1", "created": 1792684447.3010902, "status": "running", "firstAttemptId": null, "rootAttemptId": null, "latestAttemptId": null, "reconciled": true, "restartRecovery": {"epoch": 3, "accountKey": "default", "threadId": "thread-1", "turnId": "turn-1", "autoWake": true, "at": 1792684447.7965105, "stage": "pending", "startAttempt": null}, "disconnectRecovery": {"epoch": 3, "accountKey": "default", "threadId": "thread-1", "turnId": "turn-1", "autoWake": true, "at": 1792684447.7965105, "source": "restart"}}');
INSERT INTO "runtime_execution_runs" VALUES('run:cd8957f8-27ab-5eef-a9c3-5a02a523ef8b','aaaaaaaa-2222-4222-8222-222222222222','default',3,'thread-1','turn-1',1792684447.3043177,'{"id": "run:cd8957f8-27ab-5eef-a9c3-5a02a523ef8b", "agent": "aaaaaaaa-2222-4222-8222-222222222222", "accountKey": "default", "epoch": 3, "threadId": "thread-1", "turnId": "turn-1", "created": 1792684447.3043177, "status": "running", "firstAttemptId": null, "rootAttemptId": null, "latestAttemptId": null, "reconciled": true, "restartRecovery": {"epoch": 3, "accountKey": "default", "threadId": "thread-1", "turnId": "turn-1", "autoWake": true, "at": 1792684447.7969077, "stage": "pending", "startAttempt": null}, "disconnectRecovery": {"epoch": 3, "accountKey": "default", "threadId": "thread-1", "turnId": "turn-1", "autoWake": true, "at": 1792684447.7969077, "source": "restart"}}');
INSERT INTO "runtime_execution_runs" VALUES('run:3c0493c4-e9bc-5726-b0e6-6232cb0a370b','bbbbbbbb-1111-4111-8111-111111111111','default',3,'thread-2','turn-2',1792684447.3070784,'{"id": "run:3c0493c4-e9bc-5726-b0e6-6232cb0a370b", "agent": "bbbbbbbb-1111-4111-8111-111111111111", "accountKey": "default", "epoch": 3, "threadId": "thread-2", "turnId": "turn-2", "created": 1792684447.3070784, "status": "running", "firstAttemptId": null, "rootAttemptId": null, "latestAttemptId": null, "reconciled": true, "restartRecovery": {"epoch": 3, "accountKey": "default", "threadId": "thread-2", "turnId": "turn-2", "autoWake": true, "at": 1792684447.7972267, "stage": "pending", "startAttempt": null}, "disconnectRecovery": {"epoch": 3, "accountKey": "default", "threadId": "thread-2", "turnId": "turn-2", "autoWake": true, "at": 1792684447.7972267, "source": "restart"}}');
INSERT INTO "runtime_execution_runs" VALUES('run:baaa4bf8-d7e3-5029-a6e4-ee9bead0baf9','bbbbbbbb-2222-4222-8222-222222222222','default',3,'thread-1','turn-1',1792684447.3093696,'{"id": "run:baaa4bf8-d7e3-5029-a6e4-ee9bead0baf9", "agent": "bbbbbbbb-2222-4222-8222-222222222222", "accountKey": "default", "epoch": 3, "threadId": "thread-1", "turnId": "turn-1", "created": 1792684447.3093696, "status": "running", "firstAttemptId": null, "rootAttemptId": null, "latestAttemptId": null, "reconciled": true, "restartRecovery": {"epoch": 3, "accountKey": "default", "threadId": "thread-1", "turnId": "turn-1", "autoWake": true, "at": 1792684447.797499, "stage": "pending", "startAttempt": null}, "disconnectRecovery": {"epoch": 3, "accountKey": "default", "threadId": "thread-1", "turnId": "turn-1", "autoWake": true, "at": 1792684447.797499, "source": "restart"}}');
INSERT INTO "runtime_execution_runs" VALUES('run:6cbe02fd-b7fd-5704-9198-1be59eb38284','cccccccc-1111-4111-8111-111111111111','default',3,'thread-1','turn-1',1792684447.311975,'{"id": "run:6cbe02fd-b7fd-5704-9198-1be59eb38284", "agent": "cccccccc-1111-4111-8111-111111111111", "accountKey": "default", "epoch": 3, "threadId": "thread-1", "turnId": "turn-1", "created": 1792684447.311975, "status": "running", "firstAttemptId": null, "rootAttemptId": null, "latestAttemptId": null, "reconciled": true}');
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
INSERT INTO "runtime_items" VALUES('abf56fbb-f8dc-4241-ad30-3960b0afa460:preview','abf56fbb-f8dc-4241-ad30-3960b0afa460','{"id": "abf56fbb-f8dc-4241-ad30-3960b0afa460:preview", "role": "tool", "title": "commandExecution", "text": "{\"command\": \"printf review\", \"cwd\": \"/fixture\", \"changes\": [{\"path\": \"file.txt\", \"kind\": \"update\"}]}", "truncated": false, "at": 1792684447.5432637}',1792684447.5432773);
CREATE TABLE runtime_lead_requests (id TEXT PRIMARY KEY, agent TEXT NOT NULL, signature TEXT NOT NULL);
CREATE TABLE runtime_model_modes (agent TEXT, request TEXT, version TEXT, record TEXT, PRIMARY KEY(agent,request,version));
INSERT INTO "runtime_model_modes" VALUES('abf56fbb-f8dc-4241-ad30-3960b0afa460','thread-0:warmup','864d87c67a5e2d49072e144a','{"epoch":["thread-0",0],"version":"cbaab587bdd3e5b2b44c85e8","revision":0,"text":"[Studio subagent concurrency, revision 0] Multi agent mode (subagent concurrency 32): delegate sensibly up to 32 simultaneous descendant turns across all providers and reviewer roles. The lead does not use a subagent slot. Excess worker turns queue. Do not poll unchanged status."}');
CREATE TABLE runtime_monitors (id TEXT PRIMARY KEY, record TEXT NOT NULL);
INSERT INTO "runtime_monitors" VALUES('6a1cbd51-e035-54ce-af5c-7fd9850e3065','{"activityAt": 19.0, "activityGeneration": 0, "agent": "abf56fbb-f8dc-4241-ad30-3960b0afa460", "bytes": 0, "command": "printf fixture", "created": 19.0, "cwd": "__CALLBACK_STATE__", "epoch": 3, "error": "Server restarted. Command outcome unknown; not rerun.", "exitCode": null, "id": "6a1cbd51-e035-54ce-af5c-7fd9850e3065", "interactive": false, "livenessCommand": "", "log": "__CALLBACK_STATE__/monitor-logs/6a1cbd51-e035-54ce-af5c-7fd9850e3065.log", "operation": {"accountKey": "default", "agent": "abf56fbb-f8dc-4241-ad30-3960b0afa460", "connectionId": "connection", "epoch": 3, "requestId": "managed-monitor"}, "outputEventCount": 0, "outputEventIds": [], "outputEventSequence": 0, "outputPatterns": [], "reattachRecovery": {"accountKey": "default", "epoch": 3, "error": null, "finished": null, "status": "running"}, "requestId": "managed-monitor", "ruleId": null, "stallTimeoutSeconds": 1800, "stallWakeGeneration": -1, "status": "lost", "successExitCodes": [0], "tail": "", "timeout_ms": 3600000, "turnId": "turn-0", "wakeOn": "exit"}');
CREATE TABLE runtime_native_sweeps (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_operation_receipts (id TEXT PRIMARY KEY, signature TEXT NOT NULL, result TEXT NOT NULL);
CREATE TABLE runtime_payload_refs (
            table_name TEXT NOT NULL, record_id TEXT NOT NULL, field TEXT NOT NULL,
            sha256 TEXT NOT NULL, PRIMARY KEY(table_name,record_id,field));
CREATE TABLE runtime_plans (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_profiles (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_projects (id TEXT PRIMARY KEY, record TEXT NOT NULL);
INSERT INTO "runtime_projects" VALUES('__CALLBACK_STATE__','{"id": "__CALLBACK_STATE__", "path": "__CALLBACK_STATE__", "name": "template", "created": 1792684446.784392, "accountKey": "default", "accountRevision": 1, "homeServerId": "c22626d9ddce41e5a3b269d2299f3e5e", "locations": [{"serverId": "c22626d9ddce41e5a3b269d2299f3e5e", "path": "__CALLBACK_STATE__", "projectId": "__CALLBACK_STATE__"}], "locationsRevision": 0}');
CREATE TABLE runtime_requests (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_rooms (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_rules (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_scheduler_agent_change_state (id INTEGER PRIMARY KEY CHECK(id=1),floor INTEGER NOT NULL,generation INTEGER NOT NULL,entries INTEGER NOT NULL,total INTEGER NOT NULL);
INSERT INTO "runtime_scheduler_agent_change_state" VALUES(1,0,140,8,489);
CREATE TABLE runtime_scheduler_agent_changes (id TEXT PRIMARY KEY,generation INTEGER NOT NULL CHECK(typeof(generation)='integer' AND generation>=0));
INSERT INTO "runtime_scheduler_agent_changes" VALUES('abf56fbb-f8dc-4241-ad30-3960b0afa460',140);
INSERT INTO "runtime_scheduler_agent_changes" VALUES('3e73e736-e287-42b7-963c-6bef9312a291',53);
INSERT INTO "runtime_scheduler_agent_changes" VALUES('7c2692e5-4303-49f4-8ba0-cdc5a6f1db4a',54);
INSERT INTO "runtime_scheduler_agent_changes" VALUES('aaaaaaaa-1111-4111-8111-111111111111',48);
INSERT INTO "runtime_scheduler_agent_changes" VALUES('aaaaaaaa-2222-4222-8222-222222222222',49);
INSERT INTO "runtime_scheduler_agent_changes" VALUES('bbbbbbbb-1111-4111-8111-111111111111',50);
INSERT INTO "runtime_scheduler_agent_changes" VALUES('bbbbbbbb-2222-4222-8222-222222222222',51);
INSERT INTO "runtime_scheduler_agent_changes" VALUES('cccccccc-1111-4111-8111-111111111111',44);
PRAGMA writable_schema=ON;
INSERT INTO sqlite_master(type,name,tbl_name,rootpage,sql)VALUES('table','runtime_search','runtime_search',0,'CREATE VIRTUAL TABLE runtime_search USING fts5(id UNINDEXED,agent UNINDEXED,kind UNINDEXED,body,tokenize=''unicode61'')');
CREATE TABLE 'runtime_search_config'(k PRIMARY KEY, v) WITHOUT ROWID;
INSERT INTO "runtime_search_config" VALUES('version',4);
CREATE TABLE 'runtime_search_content'(id INTEGER PRIMARY KEY, c0, c1, c2, c3);
CREATE TABLE 'runtime_search_data'(id INTEGER PRIMARY KEY, block BLOB);
INSERT INTO "runtime_search_data" VALUES(1,X'');
INSERT INTO "runtime_search_data" VALUES(10,X'00000000000000');
CREATE TABLE 'runtime_search_docsize'(id INTEGER PRIMARY KEY, sz BLOB);
CREATE TABLE 'runtime_search_idx'(segid, term, pgno, PRIMARY KEY(segid, term)) WITHOUT ROWID;
CREATE TABLE runtime_search_indexed (id TEXT PRIMARY KEY);
CREATE TABLE runtime_search_next_meta (
                id TEXT PRIMARY KEY, agent TEXT NOT NULL, kind TEXT NOT NULL,
                search_rowid INTEGER NOT NULL UNIQUE);
CREATE TABLE runtime_search_rollout (
                id INTEGER PRIMARY KEY CHECK(id=1), phase TEXT NOT NULL,
                cursor INTEGER NOT NULL DEFAULT 0, updated REAL NOT NULL);
INSERT INTO "runtime_search_rollout" VALUES(1,'building',0,1792684446.7317002);
CREATE TABLE runtime_search_rows (id TEXT PRIMARY KEY, search_rowid INTEGER NOT NULL UNIQUE);
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
CREATE TABLE runtime_tool_request_aliases
                (agent TEXT NOT NULL, alias TEXT NOT NULL, request TEXT NOT NULL, PRIMARY KEY(agent,alias,request));
INSERT INTO "runtime_tool_request_aliases" VALUES('abf56fbb-f8dc-4241-ad30-3960b0afa460','alias-0','thread-0:alias-0');
INSERT INTO "runtime_tool_request_aliases" VALUES('abf56fbb-f8dc-4241-ad30-3960b0afa460','thread-0:alias-0','thread-0:alias-0');
INSERT INTO "runtime_tool_request_aliases" VALUES('abf56fbb-f8dc-4241-ad30-3960b0afa460','alias-1','thread-0:spawn:request-1');
INSERT INTO "runtime_tool_request_aliases" VALUES('abf56fbb-f8dc-4241-ad30-3960b0afa460','thread-0:alias-1','thread-0:spawn:request-1');
INSERT INTO "runtime_tool_request_aliases" VALUES('abf56fbb-f8dc-4241-ad30-3960b0afa460','request-1','thread-0:spawn:request-1');
INSERT INTO "runtime_tool_request_aliases" VALUES('abf56fbb-f8dc-4241-ad30-3960b0afa460','alias-2','thread-0:send:request-2');
INSERT INTO "runtime_tool_request_aliases" VALUES('abf56fbb-f8dc-4241-ad30-3960b0afa460','thread-0:alias-2','thread-0:send:request-2');
INSERT INTO "runtime_tool_request_aliases" VALUES('abf56fbb-f8dc-4241-ad30-3960b0afa460','request-2','thread-0:send:request-2');
INSERT INTO "runtime_tool_request_aliases" VALUES('abf56fbb-f8dc-4241-ad30-3960b0afa460','alias-3','thread-0:review:request-3');
INSERT INTO "runtime_tool_request_aliases" VALUES('abf56fbb-f8dc-4241-ad30-3960b0afa460','thread-0:alias-3','thread-0:review:request-3');
INSERT INTO "runtime_tool_request_aliases" VALUES('abf56fbb-f8dc-4241-ad30-3960b0afa460','request-3','thread-0:review:request-3');
INSERT INTO "runtime_tool_request_aliases" VALUES('abf56fbb-f8dc-4241-ad30-3960b0afa460','alias-4','thread-0:server:request-4');
INSERT INTO "runtime_tool_request_aliases" VALUES('abf56fbb-f8dc-4241-ad30-3960b0afa460','thread-0:alias-4','thread-0:server:request-4');
INSERT INTO "runtime_tool_request_aliases" VALUES('abf56fbb-f8dc-4241-ad30-3960b0afa460','request-4','thread-0:server:request-4');
INSERT INTO "runtime_tool_request_aliases" VALUES('abf56fbb-f8dc-4241-ad30-3960b0afa460','alias-5','thread-0:move:request-5');
INSERT INTO "runtime_tool_request_aliases" VALUES('abf56fbb-f8dc-4241-ad30-3960b0afa460','thread-0:alias-5','thread-0:move:request-5');
INSERT INTO "runtime_tool_request_aliases" VALUES('abf56fbb-f8dc-4241-ad30-3960b0afa460','request-5','thread-0:move:request-5');
INSERT INTO "runtime_tool_request_aliases" VALUES('abf56fbb-f8dc-4241-ad30-3960b0afa460','alias-6','thread-0:move:request-6');
INSERT INTO "runtime_tool_request_aliases" VALUES('abf56fbb-f8dc-4241-ad30-3960b0afa460','thread-0:alias-6','thread-0:move:request-6');
INSERT INTO "runtime_tool_request_aliases" VALUES('abf56fbb-f8dc-4241-ad30-3960b0afa460','request-6','thread-0:move:request-6');
CREATE TABLE runtime_tool_requests (id TEXT PRIMARY KEY, record TEXT NOT NULL);
INSERT INTO "runtime_tool_requests" VALUES('thread-0:alias-0','{"id": "thread-0:alias-0", "agent": "abf56fbb-f8dc-4241-ad30-3960b0afa460", "threadId": "thread-0", "callId": "alias-0", "tool": "orchestration_task", "signature": "1dff160641e4d1fe77178179b43844c3c6d588f8f3741f77bb9b8655c3a5e579", "rpcId": 0, "turnId": "turn-0", "epoch": 3, "accountKey": "default", "connectionId": "connection", "created": 10.0, "updated": 1792684447.805279, "readOnly": true, "timing": {"reservationBeganAt": 223681162235562, "reservationLockedAt": 223681163731562, "reservationEndedAt": 223681164328340, "handlerStartedAt": 223681181514591}, "stage": "interrupted", "outcome": "unknown", "cancelRequested": false, "started": 1792684447.2115858, "executionQueueDelayMs": 1792684437211.5857, "error": "Server restarted before the execution receipt", "result": {"success": false, "contentItems": [{"type": "inputText", "text": "{\"requestId\": \"thread-0:alias-0\", \"stage\": \"interrupted\", \"outcome\": \"unknown\", \"message\": \"Server restarted before the execution receipt. Nothing was repeated. Check what the call did before you run it again.\"}"}]}}');
INSERT INTO "runtime_tool_requests" VALUES('thread-0:spawn:request-1','{"id": "thread-0:spawn:request-1", "agent": "abf56fbb-f8dc-4241-ad30-3960b0afa460", "threadId": "thread-0", "callId": "alias-1", "tool": "orchestration_spawn", "signature": "c91f8a50f288534226b47d6abda036631ba03b0ad5aae43ad0b8e1fe6431fc12", "rpcId": 1, "turnId": "turn-0", "epoch": 3, "accountKey": "default", "connectionId": "connection", "created": 10.0, "updated": 1792684447.8056414, "readOnly": false, "timing": {"reservationBeganAt": 223681165205452, "reservationLockedAt": 223681166531504, "reservationEndedAt": 223681166951360}, "request_id": "request-1", "stage": "cancelled", "outcome": "not_applied", "cancelRequested": false, "error": "Server restarted before execution", "result": {"success": false, "contentItems": [{"type": "inputText", "text": "{\"requestId\": \"thread-0:spawn:request-1\", \"stage\": \"cancelled\", \"outcome\": \"not_applied\", \"cancelRequested\": false, \"message\": \"Server restarted before execution\"}"}]}}');
INSERT INTO "runtime_tool_requests" VALUES('thread-0:send:request-2','{"id": "thread-0:send:request-2", "agent": "abf56fbb-f8dc-4241-ad30-3960b0afa460", "threadId": "thread-0", "callId": "alias-2", "tool": "orchestration_send", "signature": "abb0f8214ffec1e848f67f36b7b030d1f53121efbd2979fd75680fd3c7785749", "rpcId": 2, "turnId": "turn-0", "epoch": 3, "accountKey": "default", "connectionId": "connection", "created": 10.0, "updated": 1792684447.8057444, "readOnly": false, "timing": {"reservationBeganAt": 223681167627355, "reservationLockedAt": 223681168803998, "reservationEndedAt": 223681169180973}, "request_id": "request-2", "stage": "cancelled", "outcome": "not_applied", "cancelRequested": false, "error": "Server restarted before execution", "result": {"success": false, "contentItems": [{"type": "inputText", "text": "{\"requestId\": \"thread-0:send:request-2\", \"stage\": \"cancelled\", \"outcome\": \"not_applied\", \"cancelRequested\": false, \"message\": \"Server restarted before execution\"}"}]}}');
INSERT INTO "runtime_tool_requests" VALUES('thread-0:review:request-3','{"id": "thread-0:review:request-3", "agent": "abf56fbb-f8dc-4241-ad30-3960b0afa460", "threadId": "thread-0", "callId": "alias-3", "tool": "orchestration_review", "signature": "69b6d0789d8ba6854d46da3a2ec8e90d8871e7ce6895cd678acd23aaf86106ca", "rpcId": 3, "turnId": "turn-0", "epoch": 3, "accountKey": "default", "connectionId": "connection", "created": 10.0, "updated": 1792684447.8058205, "readOnly": false, "timing": {"reservationBeganAt": 223681169847842, "reservationLockedAt": 223681171343832, "reservationEndedAt": 223681171825163}, "request_id": "request-3", "stage": "cancelled", "outcome": "not_applied", "cancelRequested": false, "error": "Server restarted before execution", "result": {"success": false, "contentItems": [{"type": "inputText", "text": "{\"requestId\": \"thread-0:review:request-3\", \"stage\": \"cancelled\", \"outcome\": \"not_applied\", \"cancelRequested\": false, \"message\": \"Server restarted before execution\"}"}]}}');
INSERT INTO "runtime_tool_requests" VALUES('thread-0:server:request-4','{"id": "thread-0:server:request-4", "agent": "abf56fbb-f8dc-4241-ad30-3960b0afa460", "threadId": "thread-0", "callId": "alias-4", "tool": "orchestration_servers", "signature": "4b00aba39b4be2847cacf972191005ff1b73f15438c48c9b54e8507c26aeb478", "rpcId": 4, "turnId": "turn-0", "epoch": 3, "accountKey": "default", "connectionId": "connection", "created": 10.0, "updated": 1792684447.8058898, "readOnly": false, "timing": {"reservationBeganAt": 223681172689060, "reservationLockedAt": 223681174125018, "reservationEndedAt": 223681174577605}, "request_id": "request-4", "stage": "cancelled", "outcome": "not_applied", "cancelRequested": false, "error": "Server restarted before execution", "result": {"success": false, "contentItems": [{"type": "inputText", "text": "{\"requestId\": \"thread-0:server:request-4\", \"stage\": \"cancelled\", \"outcome\": \"not_applied\", \"cancelRequested\": false, \"message\": \"Server restarted before execution\"}"}]}}');
INSERT INTO "runtime_tool_requests" VALUES('thread-0:move:request-5','{"id": "thread-0:move:request-5", "agent": "abf56fbb-f8dc-4241-ad30-3960b0afa460", "threadId": "thread-0", "callId": "alias-5", "tool": "orchestration_move", "signature": "00cebdf9732e1609eecdd9a0f86b687817cd2a371ef83e312ac50706f822f661", "rpcId": 5, "turnId": "turn-0", "epoch": 3, "accountKey": "default", "connectionId": "connection", "created": 10.0, "updated": 1792684447.8059556, "readOnly": false, "timing": {"reservationBeganAt": 223681175384957, "reservationLockedAt": 223681176984230, "reservationEndedAt": 223681177367979}, "request_id": "request-5", "stage": "cancelled", "outcome": "not_applied", "cancelRequested": false, "error": "Server restarted before execution", "result": {"success": false, "contentItems": [{"type": "inputText", "text": "{\"requestId\": \"thread-0:move:request-5\", \"stage\": \"cancelled\", \"outcome\": \"not_applied\", \"cancelRequested\": false, \"message\": \"Server restarted before execution\"}"}]}}');
INSERT INTO "runtime_tool_requests" VALUES('thread-0:move:request-6','{"id": "thread-0:move:request-6", "agent": "abf56fbb-f8dc-4241-ad30-3960b0afa460", "threadId": "thread-0", "callId": "alias-6", "tool": "orchestration_teleport", "signature": "4d2a73cfcffe4df8b7075fa3edcc43dc0faab254b347892d55b3c227be583e82", "rpcId": 6, "turnId": "turn-0", "epoch": 3, "accountKey": "default", "connectionId": "connection", "created": 10.0, "updated": 1792684447.806042, "readOnly": false, "timing": {"reservationBeganAt": 223681177956620, "reservationLockedAt": 223681179373843, "reservationEndedAt": 223681179710313}, "request_id": "request-6", "stage": "cancelled", "outcome": "not_applied", "cancelRequested": false, "error": "Server restarted before execution", "result": {"success": false, "contentItems": [{"type": "inputText", "text": "{\"requestId\": \"thread-0:move:request-6\", \"stage\": \"cancelled\", \"outcome\": \"not_applied\", \"cancelRequested\": false, \"message\": \"Server restarted before execution\"}"}]}}');
CREATE TABLE runtime_tool_results (id TEXT PRIMARY KEY, result TEXT NOT NULL);
CREATE TABLE runtime_transcript_revisions(
                        agent TEXT PRIMARY KEY, revision INTEGER NOT NULL DEFAULT 0);
INSERT INTO "runtime_transcript_revisions" VALUES('abf56fbb-f8dc-4241-ad30-3960b0afa460',126);
CREATE TABLE runtime_turn_item_link_state_v1 (id INTEGER PRIMARY KEY CHECK(id=1),epoch TEXT NOT NULL,source_rootpage INTEGER NOT NULL,cursor INTEGER,target INTEGER,complete INTEGER NOT NULL CHECK(complete IN (0,1)),scanned_rows INTEGER NOT NULL,source_bytes INTEGER NOT NULL,updated REAL NOT NULL);
INSERT INTO "runtime_turn_item_link_state_v1" VALUES(1,'462a645137c24e6b9f0c50320981be6a',96,NULL,NULL,1,0,0,1792684446.6525176);
CREATE TABLE runtime_turn_item_links_v1 (id TEXT PRIMARY KEY,agent TEXT NOT NULL,turn_id,created REAL NOT NULL,epoch TEXT NOT NULL);
INSERT INTO "runtime_turn_item_links_v1" VALUES('abf56fbb-f8dc-4241-ad30-3960b0afa460:preview','abf56fbb-f8dc-4241-ad30-3960b0afa460',NULL,1792684447.5432773,'462a645137c24e6b9f0c50320981be6a');
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
INSERT INTO "sync_entities" VALUES('project','__CALLBACK_STATE__',1,'410fc08cbbbac2bcdcbf8eef2367b14c74b03d5f6a709773d1f309bf37f20930','{"collection":"project","id":"__CALLBACK_STATE__","value":{"accountKey":"default","accountRevision":1,"created":1792684446.784392,"homeServerId":"c22626d9ddce41e5a3b269d2299f3e5e","id":"__CALLBACK_STATE__","locations":[{"path":"__CALLBACK_STATE__","projectId":"__CALLBACK_STATE__","serverId":"c22626d9ddce41e5a3b269d2299f3e5e"}],"locationsRevision":0,"name":"template","path":"__CALLBACK_STATE__"}}',0);
INSERT INTO "sync_entities" VALUES('agent','abf56fbb-f8dc-4241-ad30-3960b0afa460',346,'55a50a583672e6e1a9ceb99e46be3924a7437e8eca1bd2a28bb5f0a666251d96','{"collection": "agent", "id": "abf56fbb-f8dc-4241-ad30-3960b0afa460", "value": {"accountKey": "default", "agentMode": "multi", "agentModeRevision": 0, "agentModeSupported": true, "autoWake": true, "canSend": true, "commandProgress": [], "compactions": 0, "concurrency": 32, "contextUsage": null, "created": 1792684446.7842586, "cwd": "__CALLBACK_STATE__", "cyberAccessProgram": "standard", "daybreakEnabled": false, "effort": "medium", "empty": false, "environment": "host", "epoch": 3, "error": null, "executionMode": "native", "fastMode": false, "hostProjectPath": "__CALLBACK_STATE__", "id": "abf56fbb-f8dc-4241-ad30-3960b0afa460", "inFlight": false, "isLead": true, "kind": "agent", "launcherAlive": true, "layrReady": false, "model": "gpt-6-astra", "name": "Callback lead", "nextTurnSettingsSupported": true, "parentId": null, "provider": "codex", "readStateSupported": true, "reviewDefaults": {"effort": null, "model": null}, "role": "orchestrator", "rootId": "abf56fbb-f8dc-4241-ad30-3960b0afa460", "serverId": "c22626d9ddce41e5a3b269d2299f3e5e", "source": "managed", "stall": null, "startWait": null, "status": "running", "subagentConcurrencyVersion": 2, "tail": "", "threadId": "thread-0", "tokensUsed": 0, "turnId": "turn-0", "workerDefaults": {"daybreakEnabled": false, "delegationDepth": 1, "effort": "high", "fastMode": false, "model": "gpt-6-luna"}, "workspaceBackend": null, "workspaceMode": "worktree", "worktree": false, "yoloMode": true}}',0);
INSERT INTO "sync_entities" VALUES('agent','3e73e736-e287-42b7-963c-6bef9312a291',136,'9445eeaa88debfef2b1d0de4c2751a0663ea7c5bfa838c92032067da602f8316','{"collection": "agent", "id": "3e73e736-e287-42b7-963c-6bef9312a291", "value": {"accountKey": "default", "agentMode": "multi", "agentModeRevision": 0, "agentModeSupported": true, "autoWake": true, "canSend": true, "commandProgress": [], "compactions": 0, "concurrency": 32, "contextUsage": null, "created": 1792684446.7901473, "cwd": "__CALLBACK_STATE__", "cyberAccessProgram": "standard", "daybreakEnabled": false, "effort": "medium", "environment": "host", "epoch": 3, "error": null, "executionMode": "native", "fastMode": false, "hostProjectPath": "__CALLBACK_STATE__", "id": "3e73e736-e287-42b7-963c-6bef9312a291", "inFlight": false, "isLead": false, "kind": "agent", "launcherAlive": true, "layrReady": false, "model": "gpt-6-astra", "name": "Callback worker", "nextTurnSettingsSupported": true, "overview": {"result": "", "resultFile": null, "resultTruncated": false, "resultTurnId": null, "task": "fixture", "taskTruncated": false}, "parentId": "abf56fbb-f8dc-4241-ad30-3960b0afa460", "provider": "codex", "readStateSupported": true, "reviewDefaults": {"effort": null, "model": null}, "role": "orchestrator", "rootId": "abf56fbb-f8dc-4241-ad30-3960b0afa460", "serverId": "c22626d9ddce41e5a3b269d2299f3e5e", "source": "managed", "stall": null, "startWait": null, "status": "running", "subagentConcurrencyVersion": 2, "tail": "", "threadId": "thread-1", "tokensUsed": 0, "turnId": "turn-1", "workerDefaults": {"daybreakEnabled": false, "delegationDepth": 1, "effort": "high", "fastMode": false, "model": "gpt-6-luna"}, "workspaceBackend": null, "workspaceMode": "worktree", "worktree": false, "yoloMode": true}}',0);
INSERT INTO "sync_entities" VALUES('agent','7c2692e5-4303-49f4-8ba0-cdc5a6f1db4a',137,'a7d0113df7ad39da48ae6c28291a5b0f23e0606a6a537b4554a79c243a66f7fe','{"collection": "agent", "id": "7c2692e5-4303-49f4-8ba0-cdc5a6f1db4a", "value": {"accountKey": "default", "agentMode": "multi", "agentModeRevision": 0, "agentModeSupported": true, "autoWake": true, "canSend": true, "commandProgress": [], "compactions": 0, "concurrency": 32, "contextUsage": null, "created": 1792684446.7960482, "cwd": "__CALLBACK_STATE__", "cyberAccessProgram": "standard", "daybreakEnabled": false, "effort": "medium", "empty": false, "environment": "host", "epoch": 3, "error": null, "executionMode": "native", "fastMode": false, "hostProjectPath": "__CALLBACK_STATE__", "id": "7c2692e5-4303-49f4-8ba0-cdc5a6f1db4a", "inFlight": false, "isLead": true, "kind": "agent", "launcherAlive": true, "layrReady": false, "model": "gpt-6-astra", "name": "Other team", "nextTurnSettingsSupported": true, "parentId": null, "provider": "codex", "readStateSupported": true, "reviewDefaults": {"effort": null, "model": null}, "role": "orchestrator", "rootId": "7c2692e5-4303-49f4-8ba0-cdc5a6f1db4a", "serverId": "c22626d9ddce41e5a3b269d2299f3e5e", "source": "managed", "stall": null, "startWait": null, "status": "running", "subagentConcurrencyVersion": 2, "tail": "", "threadId": "thread-2", "tokensUsed": 0, "turnId": "turn-2", "workerDefaults": {"daybreakEnabled": false, "delegationDepth": 1, "effort": "high", "fastMode": false, "model": "gpt-6-luna"}, "workspaceBackend": null, "workspaceMode": "worktree", "worktree": false, "yoloMode": true}}',0);
INSERT INTO "sync_entities" VALUES('request','answer-0',9,'ca780824cd43ffc0e5eef158a5ab34ae0e9ec57bbc342f18efea62e55be2efe7','{"collection":"request","id":"answer-0","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','answer-1',11,'600ec1576085c7345df51a1d24d1c04d2836a7a625adb13de0ee2bd4d514f64e','{"collection":"request","id":"answer-1","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','answer-2',13,'b6ddd9f61d40f13509a0e91850aae6a39332031fb6c3aad43f47d69c66ba736d','{"collection":"request","id":"answer-2","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','answer-3',15,'0097b9b99d887aa31a64948aaf35a9cc5284b29933c8531a0b3e4ba2adecea5a','{"collection":"request","id":"answer-3","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','answer-4',17,'78d304379b02c07bff7788800c50147b093790b9b3cae7975befc4dd1d267c8b','{"collection":"request","id":"answer-4","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','answer-5',19,'bb5f7a79ae1b4730ca02265bfdbbf86507eec5f166c7115963e95b8a62085b57','{"collection":"request","id":"answer-5","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','answer-6',21,'4af940c68e6ce2dc9acf18d7a8593d304eff5c7978fbff00c71daf97ae2426e2','{"collection":"request","id":"answer-6","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','answer-7',23,'71248c4ae2759d0257bb131c5b434d64c745abdf4e801c2fabe6c2cee6043490','{"collection":"request","id":"answer-7","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','answer-8',25,'7991f8ce68ae78287e0794d10e38666769f5d4859d2309ef7e40767d0518773b','{"collection":"request","id":"answer-8","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','answer-9',27,'67d06bd9b0b8507a29b26084c2916fbd751945a440cdeca5b61ed6c1c22f1791','{"collection":"request","id":"answer-9","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','answer-10',29,'f3a4368c277a2d8be1ea88c4f19382233d5ba7503f223596cea4fb9b2f8d2829','{"collection":"request","id":"answer-10","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','answer-11',31,'3659d9dca58ea38273a7933a72c4eb42350a24e38a468cbebd703a3b32b70149','{"collection":"request","id":"answer-11","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','answer-12',33,'ff1b0099378833648c9a1cb497891c8b1ea91f81b5f85df4d027eff3e8c98212','{"collection":"request","id":"answer-12","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','answer-13',35,'53adca3f2506a6ecc4e2c3deb57cb7c781218c018cf1388a99a9840c4e2d44de','{"collection":"request","id":"answer-13","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','answer-14',37,'9cede43c415fc336299b87173df9d7972d46e1d8d67773ddde4bfc95d8bbc0b4','{"collection":"request","id":"answer-14","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','answer-15',39,'289a2c77d5ec9176f1965efad5737448dc0234807388ff1ebfe3e035dd1ed66f','{"collection":"request","id":"answer-15","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','answer-16',41,'f362717c0703093fc9778272136959fee9958e8064705d16f7c50f3d5968b4e3','{"collection":"request","id":"answer-16","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','answer-17',43,'9dadbf231395b5d03b3240c24797ee2186eca4e6422ad52debc30c7f95fb5fa8','{"collection":"request","id":"answer-17","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','answer-18',45,'a6b4216be987bff145b27ed8b5f17b3b44c939c4930d6d5eae446c655e3320a2','{"collection":"request","id":"answer-18","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','answer-19',47,'34111e5d7135cabf313dfe1c69e7d0ca3a67887a752d1c1548ebceb77e624378','{"collection":"request","id":"answer-19","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','answer-20',49,'d4c793c9c991018070b19a52c6a92a5fad975edb71e151f7f8ad463e2f43bb35','{"collection":"request","id":"answer-20","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','answer-21',51,'f69b92a6e8dc6d23887503d931d0107df8bf2e6d6f54dead32281ee7b7ac4528','{"collection":"request","id":"answer-21","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','answer-22',53,'2c98bb62b49953b5300f884ccd554cc2d755becde757004f10e3d0c2dfec5e2e','{"collection":"request","id":"answer-22","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','answer-23',55,'17fe27955b4d421eff9ff0ccb66541b981b4fe9022811b544240f44e50265642','{"collection":"request","id":"answer-23","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','answer-24',57,'37d87bb32563438dabe2dc0b040680e4c9986d7e6fc9d0398b8a8f94af1c5acb','{"collection":"request","id":"answer-24","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','answer-25',59,'10cb4a8f05ebfc2294341f83c9321339d2a210e901686276f5cd8ada4a363252','{"collection":"request","id":"answer-25","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','answer-26',61,'2a244dca467341146a1c33b33ceaf68ea3d54f9cb2cb14a3545c7acdeb931174','{"collection":"request","id":"answer-26","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','answer-27',63,'3675bf7bd8a97ddfdb7ad128348f1628576d6f4fdc48524dfb4df3d46a3089c0','{"collection":"request","id":"answer-27","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','answer-28',65,'330077b156dd9c69b9cf9a60b7eaf4f67a735367c057e0b8ee10cb846a510df2','{"collection":"request","id":"answer-28","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','answer-29',67,'466f9c6cd01898afd34501a47f8e7950886eee8cf0727b9a9f80cc4d48d9ddc8','{"collection":"request","id":"answer-29","value":{}}',1);
INSERT INTO "sync_entities" VALUES('agent','aaaaaaaa-1111-4111-8111-111111111111',127,'43088c10788f7f7304e91449020c91e0b36a92a50b0880088ff153f4578916d1','{"collection": "agent", "id": "aaaaaaaa-1111-4111-8111-111111111111", "value": {"accountKey": "default", "agentMode": "multi", "agentModeRevision": 0, "agentModeSupported": true, "autoWake": false, "canSend": true, "commandProgress": [], "compactions": 0, "contextUsage": null, "created": 1792684446.7901473, "cwd": "__CALLBACK_STATE__", "cyberAccessProgram": "standard", "daybreakEnabled": false, "deletedAt": null, "effort": "medium", "environment": "host", "epoch": 3, "error": "Server restarted during a turn. Review history, then send a new instruction.", "executionMode": "native", "fastMode": false, "hostProjectPath": "__CALLBACK_STATE__", "id": "aaaaaaaa-1111-4111-8111-111111111111", "inFlight": false, "isLead": false, "kind": "agent", "launcherAlive": true, "layrReady": false, "model": "gpt-6-astra", "name": "Callback worker", "nextTurnSettingsSupported": true, "overview": {"result": "", "resultFile": null, "resultTruncated": false, "resultTurnId": null, "task": "fixture", "taskTruncated": false}, "parentId": "abf56fbb-f8dc-4241-ad30-3960b0afa460", "provider": "codex", "readStateSupported": true, "reviewDefaults": {"effort": null, "model": null}, "role": "orchestrator", "rootId": "abf56fbb-f8dc-4241-ad30-3960b0afa460", "serverId": "c22626d9ddce41e5a3b269d2299f3e5e", "source": "managed", "stall": null, "startWait": null, "status": "interrupted", "subagentConcurrencyVersion": 2, "tail": "", "threadId": "thread-1", "tokensUsed": 0, "turnId": "turn-1", "workerDefaults": {"daybreakEnabled": false, "delegationDepth": 1, "effort": "high", "fastMode": false, "model": "gpt-6-luna"}, "workspaceBackend": null, "workspaceMode": "worktree", "worktree": false, "yoloMode": true}}',0);
INSERT INTO "sync_entities" VALUES('agent','aaaaaaaa-2222-4222-8222-222222222222',128,'dc4dbe89f7d044ff395b32142f143a6e204ea18c5cccaaf9671d1959c5611300','{"collection": "agent", "id": "aaaaaaaa-2222-4222-8222-222222222222", "value": {"accountKey": "default", "agentMode": "multi", "agentModeRevision": 0, "agentModeSupported": true, "autoWake": false, "canSend": true, "commandProgress": [], "compactions": 0, "contextUsage": null, "created": 1792684446.7901473, "cwd": "__CALLBACK_STATE__", "cyberAccessProgram": "standard", "daybreakEnabled": false, "deletedAt": null, "effort": "medium", "environment": "host", "epoch": 3, "error": "Server restarted during a turn. Review history, then send a new instruction.", "executionMode": "native", "fastMode": false, "hostProjectPath": "__CALLBACK_STATE__", "id": "aaaaaaaa-2222-4222-8222-222222222222", "inFlight": false, "isLead": false, "kind": "agent", "launcherAlive": true, "layrReady": false, "model": "gpt-6-astra", "name": "Callback worker", "nextTurnSettingsSupported": true, "overview": {"result": "", "resultFile": null, "resultTruncated": false, "resultTurnId": null, "task": "fixture", "taskTruncated": false}, "parentId": "abf56fbb-f8dc-4241-ad30-3960b0afa460", "provider": "codex", "readStateSupported": true, "reviewDefaults": {"effort": null, "model": null}, "role": "orchestrator", "rootId": "abf56fbb-f8dc-4241-ad30-3960b0afa460", "serverId": "c22626d9ddce41e5a3b269d2299f3e5e", "source": "managed", "stall": null, "startWait": null, "status": "interrupted", "subagentConcurrencyVersion": 2, "tail": "", "threadId": "thread-1", "tokensUsed": 0, "turnId": "turn-1", "workerDefaults": {"daybreakEnabled": false, "delegationDepth": 1, "effort": "high", "fastMode": false, "model": "gpt-6-luna"}, "workspaceBackend": null, "workspaceMode": "worktree", "worktree": false, "yoloMode": true}}',0);
INSERT INTO "sync_entities" VALUES('agent','bbbbbbbb-1111-4111-8111-111111111111',129,'a60c20f5e0a9e3978b9e42fb97cbf8ed199a3c2f063a13090fe351b1fc6c6045','{"collection": "agent", "id": "bbbbbbbb-1111-4111-8111-111111111111", "value": {"accountKey": "default", "agentMode": "multi", "agentModeRevision": 0, "agentModeSupported": true, "autoWake": false, "canSend": true, "commandProgress": [], "compactions": 0, "concurrency": 32, "contextUsage": null, "created": 1792684446.7960482, "cwd": "__CALLBACK_STATE__", "cyberAccessProgram": "standard", "daybreakEnabled": false, "deletedAt": null, "effort": "medium", "empty": false, "environment": "host", "epoch": 3, "error": "Server restarted during a turn. Review history, then send a new instruction.", "executionMode": "native", "fastMode": false, "hostProjectPath": "__CALLBACK_STATE__", "id": "bbbbbbbb-1111-4111-8111-111111111111", "inFlight": false, "isLead": true, "kind": "agent", "launcherAlive": true, "layrReady": false, "model": "gpt-6-astra", "name": "Other team", "nextTurnSettingsSupported": true, "parentId": null, "provider": "codex", "readStateSupported": true, "reviewDefaults": {"effort": null, "model": null}, "role": "orchestrator", "rootId": "bbbbbbbb-1111-4111-8111-111111111111", "serverId": "c22626d9ddce41e5a3b269d2299f3e5e", "source": "managed", "stall": null, "startWait": null, "status": "interrupted", "subagentConcurrencyVersion": 2, "tail": "", "threadId": "thread-2", "tokensUsed": 0, "turnId": "turn-2", "workerDefaults": {"daybreakEnabled": false, "delegationDepth": 1, "effort": "high", "fastMode": false, "model": "gpt-6-luna"}, "workspaceBackend": null, "workspaceMode": "worktree", "worktree": false, "yoloMode": true}}',0);
INSERT INTO "sync_entities" VALUES('agent','bbbbbbbb-2222-4222-8222-222222222222',130,'ae3676d3e799d3a700eee04b6dc77ddd95c55213dc9f97f742ff95b951a31e0c','{"collection": "agent", "id": "bbbbbbbb-2222-4222-8222-222222222222", "value": {"accountKey": "default", "agentMode": "multi", "agentModeRevision": 0, "agentModeSupported": true, "autoWake": false, "canSend": true, "commandProgress": [], "compactions": 0, "contextUsage": null, "created": 1792684446.7901473, "cwd": "__CALLBACK_STATE__", "cyberAccessProgram": "standard", "daybreakEnabled": false, "deletedAt": null, "effort": "medium", "environment": "host", "epoch": 3, "error": "Server restarted during a turn. Review history, then send a new instruction.", "executionMode": "native", "fastMode": false, "hostProjectPath": "__CALLBACK_STATE__", "id": "bbbbbbbb-2222-4222-8222-222222222222", "inFlight": false, "isLead": false, "kind": "agent", "launcherAlive": true, "layrReady": false, "model": "gpt-6-astra", "name": "Callback worker", "nextTurnSettingsSupported": true, "overview": {"result": "", "resultFile": null, "resultTruncated": false, "resultTurnId": null, "task": "fixture", "taskTruncated": false}, "parentId": "abf56fbb-f8dc-4241-ad30-3960b0afa460", "provider": "codex", "readStateSupported": true, "reviewDefaults": {"effort": null, "model": null}, "role": "orchestrator", "rootId": "abf56fbb-f8dc-4241-ad30-3960b0afa460", "serverId": "c22626d9ddce41e5a3b269d2299f3e5e", "source": "managed", "stall": null, "startWait": null, "status": "interrupted", "subagentConcurrencyVersion": 2, "tail": "", "threadId": "thread-1", "tokensUsed": 0, "turnId": "turn-1", "workerDefaults": {"daybreakEnabled": false, "delegationDepth": 1, "effort": "high", "fastMode": false, "model": "gpt-6-luna"}, "workspaceBackend": null, "workspaceMode": "worktree", "worktree": false, "yoloMode": true}}',0);
INSERT INTO "sync_entities" VALUES('agent','cccccccc-1111-4111-8111-111111111111',73,'cdad54a0bd7b1bd7dd271f09bfe9cf42fc1986c5a17f8342cb8e97b3a21d35aa','{"collection":"agent","id":"cccccccc-1111-4111-8111-111111111111","value":{}}',1);
INSERT INTO "sync_entities" VALUES('work','5d133257-ae7a-4370-aa83-1a34dd5b7e09',91,'5bc197a8cbb7d7de3f46fb225252095fa8ac9b595653dfeb5df4cd02e81a6896','{"collection":"work","id":"5d133257-ae7a-4370-aa83-1a34dd5b7e09","value":{"id":"5d133257-ae7a-4370-aa83-1a34dd5b7e09","rootId":"abf56fbb-f8dc-4241-ad30-3960b0afa460","status":"ready","title":"Valid title"}}',0);
INSERT INTO "sync_entities" VALUES('work','5c30f2eb-316e-4fb4-aa09-276856bf7385',77,'18a02031c746e55428e08a68ed064402c91f26ca3ec80e31c76bac0b67fe8958','{"collection":"work","id":"5c30f2eb-316e-4fb4-aa09-276856bf7385","value":{"id":"5c30f2eb-316e-4fb4-aa09-276856bf7385","rootId":"abf56fbb-f8dc-4241-ad30-3960b0afa460","status":"ready","title":"Valid title"}}',0);
INSERT INTO "sync_entities" VALUES('work','378e58ab-a1ce-428c-925f-b40b8e08143a',84,'389c1b6c492481cf3ff63bbe1b97db045d466117a1e65b5c3ccd80f5293af13d','{"collection":"work","id":"378e58ab-a1ce-428c-925f-b40b8e08143a","value":{"id":"378e58ab-a1ce-428c-925f-b40b8e08143a","rootId":"abf56fbb-f8dc-4241-ad30-3960b0afa460","status":"ready","title":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"}}',0);
INSERT INTO "sync_entities" VALUES('work','cdd039e5-e272-4855-8eff-fa22c71b841a',85,'aefc34a69e07a9845852092d9d7a91de9edffd89ce980566944db9f104dd3cf3','{"collection":"work","id":"cdd039e5-e272-4855-8eff-fa22c71b841a","value":{"id":"cdd039e5-e272-4855-8eff-fa22c71b841a","rootId":"abf56fbb-f8dc-4241-ad30-3960b0afa460","status":"blocked","title":"Valid title"}}',0);
INSERT INTO "sync_entities" VALUES('work','6cc71248-30bf-4811-b473-1aafcab6f5a8',86,'aac8f540448c2033867281cd98503e6b6d39848f2349deaa2a02f2e4a59e314c','{"collection":"work","id":"6cc71248-30bf-4811-b473-1aafcab6f5a8","value":{"id":"6cc71248-30bf-4811-b473-1aafcab6f5a8","rootId":"abf56fbb-f8dc-4241-ad30-3960b0afa460","status":"ready","title":"Valid title"}}',0);
INSERT INTO "sync_entities" VALUES('request','8b359a1f-85cd-4d54-adfd-733c9c049f3c',92,'e25115772397b38ee391b517e9292054f7cfd674dcd651140180d31527f61442','{"collection":"request","id":"8b359a1f-85cd-4d54-adfd-733c9c049f3c","value":{"accountKey":"default","agent":"abf56fbb-f8dc-4241-ad30-3960b0afa460","connectionId":"connection","createdAt":20.0,"epoch":3,"id":"8b359a1f-85cd-4d54-adfd-733c9c049f3c","method":"item/commandExecution/requestApproval","params":{"itemId":"preview","threadId":"thread-0","turnId":"turn-0"},"preview":{"changes":[{"kind":"update","path":"file.txt"}],"command":"printf review","cwd":"/fixture"},"status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('request','resolve-card',332,'b56fe424da372c2f0cee9848ee1276a9abb0095b37933d74f95c978c94ba06d0','{"collection":"request","id":"resolve-card","value":{}}',1);
INSERT INTO "sync_entities" VALUES('room','private-review',139,'389df7f888ea04f82fc1246c33d5e1cfffaebb460578b60737edeafde043c409','{"collection":"room","id":"private-review","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','python-written-card',138,'7c3aa5e1db8164aa80c7c540e2fd4c2cf05383fd12dee2ff6451c17fd75195ef','{"collection":"request","id":"python-written-card","value":{"accountKey":"default","agent":"abf56fbb-f8dc-4241-ad30-3960b0afa460","connectionId":"connection","createdAt":10.0,"epoch":3,"id":"python-written-card","method":"item/commandExecution/requestApproval","params":{"threadId":"thread-0","turnId":"turn-0"},"status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('monitor','6a1cbd51-e035-54ce-af5c-7fd9850e3065',131,'cb93790aad7c0f1615f856e7bd53a918c2832d8b46f3f7769137c5146bfa8cfe','{"collection":"monitor","id":"6a1cbd51-e035-54ce-af5c-7fd9850e3065","value":{"agent":"abf56fbb-f8dc-4241-ad30-3960b0afa460","bytes":0,"command":"printf fixture","created":19.0,"cwd":"__CALLBACK_STATE__","error":"Server restarted. Command outcome unknown; not rerun.","exitCode":null,"id":"6a1cbd51-e035-54ce-af5c-7fd9850e3065","interactive":false,"log":"__CALLBACK_STATE__/monitor-logs/6a1cbd51-e035-54ce-af5c-7fd9850e3065.log","ruleId":null,"status":"lost","tail":"","timeout_ms":3600000}}',0);
INSERT INTO "sync_entities" VALUES('event','monitor:6a1cbd51-e035-54ce-af5c-7fd9850e3065',132,'c83d84e17f36ac227468d22802128d966dcc7b8f60817af8bc74f556b678fd5c','{"collection":"event","id":"monitor:6a1cbd51-e035-54ce-af5c-7fd9850e3065","value":{"agent":"abf56fbb-f8dc-4241-ad30-3960b0afa460","created":1792684447.798625,"error":null,"id":"monitor:6a1cbd51-e035-54ce-af5c-7fd9850e3065","kind":"monitor_exit","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('workspace','current',140,'28eb8b5d3db8e1f6253b1de1b18245fbc844bf4badf6c1bb45c525f26f7b807d','{"collection":"workspace","id":"current","value":{"connected":false,"nativeNotices":[],"peerTeamsVersion":1,"projectOrganizationVersion":1,"rateLimits":{"accountKey":"default","at":null,"data":null,"error":null},"rateLimitsByAccount":{},"sidebarOrder":{"groups":null,"revision":0},"stateDir":"__CALLBACK_STATE__","tasksHistoryLimit":100}}',0);
INSERT INTO "sync_entities" VALUES('request','roundtrip-0',143,'e5f99ef1a17519404a853ab3446797a9d3573b65255e1282c7793d8ecd76db52','{"collection":"request","id":"roundtrip-0","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','roundtrip-1',147,'ba4e0cf2a0dd76483aa802509c7cc19cd88f6b943f26d115b5d4e80741bbf677','{"collection":"request","id":"roundtrip-1","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','roundtrip-2',151,'e6f35819a7f32d00fc34b92a0ebbf3aff7945a25cc50451833b1210b1f67a02b','{"collection":"request","id":"roundtrip-2","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','roundtrip-3',155,'f24fc7b5a63a80f6d76a53b23c381345cf060e7a1807c731044c2f2012d6daae','{"collection":"request","id":"roundtrip-3","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','roundtrip-4',159,'e8cb8e7bd99f1518d2aeca1f6323ca9e354d77533dbf4572abab3e3ff4f4eadc','{"collection":"request","id":"roundtrip-4","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','roundtrip-5',163,'db4372257c17f77d6509a1b76ee59ba73a5b5b9da0c1b6886579570c582d0f10','{"collection":"request","id":"roundtrip-5","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','roundtrip-6',167,'f5c338dac04e38798669550ff7df4c317ab8918d72e0da96aaf3828dd27db1cd','{"collection":"request","id":"roundtrip-6","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','roundtrip-7',171,'dd8f160de7c898970358d0fba998015e7ccee63b1ef45e328945ee8a51f2d019','{"collection":"request","id":"roundtrip-7","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','roundtrip-8',175,'94339f9d5d6819cd698a1f1009a48bb190c01e3ec9ad2f411258596848b1e370','{"collection":"request","id":"roundtrip-8","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','roundtrip-9',179,'6a05945ca71ffaa5d4d82b74f8c35709c9ba71f494ecee308946cbd69580494d','{"collection":"request","id":"roundtrip-9","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','roundtrip-10',183,'c93e985c47b4851d9767d57bc6d8fc9408072180f772fcbe419c848a733a425e','{"collection":"request","id":"roundtrip-10","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','roundtrip-11',187,'da9e2420e3d81dedf05d8f55d19ecde1b2d16d80a3c5d04b8e3465525ed5f5ac','{"collection":"request","id":"roundtrip-11","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','roundtrip-12',191,'dccc301cfee87cdd7eb6a07b293555fa63bf96679199d8fe03d8bf36db3e0ae8','{"collection":"request","id":"roundtrip-12","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','roundtrip-13',195,'63b6a88de11e4c292fc5faa1b4a7fcd8eea1bea784569b02d3b5bf17500d06b2','{"collection":"request","id":"roundtrip-13","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','roundtrip-14',199,'1b6b860e2657531cf4cffe6f1381183eb4fc2df5d64ad0e2aefb3c470c8e3721','{"collection":"request","id":"roundtrip-14","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','roundtrip-15',203,'15706097b9ec83248a6928f6057874d0bb5c7616a0010792e55cccbe8ee0a1d9','{"collection":"request","id":"roundtrip-15","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','roundtrip-16',207,'069c86194173996773c7a047a7c5375ba30f8bc7d364c0561abac1991eb023d3','{"collection":"request","id":"roundtrip-16","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','roundtrip-17',211,'84d4940524927b1e7e4e746427a2fe98d5606e281a4468bd4bb929e8b108d6f9','{"collection":"request","id":"roundtrip-17","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','roundtrip-18',215,'4e4b9f076dce4bd2283dc12a09a8ca6115f32433325a7c69bc3053c8c138a28c','{"collection":"request","id":"roundtrip-18","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','roundtrip-19',219,'ee0381965eef1b6ee3c0ed8e24e33cfef1437352a821382f139b2f30e459346d','{"collection":"request","id":"roundtrip-19","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','roundtrip-20',223,'3b30f90432bc5daec8f0adf229dd3f1e3220ddeeea0f9d54da838e7996c28f43','{"collection":"request","id":"roundtrip-20","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','roundtrip-21',227,'2dd0e5a2b4b35cab8d104f2966f3b5c7a3558d1833dfe494fb213c32c3780129','{"collection":"request","id":"roundtrip-21","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','roundtrip-22',231,'44a40432b3bd084739d2aaa2bd141129fd4f333b214d5bb90cf67179be1d05be','{"collection":"request","id":"roundtrip-22","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','roundtrip-23',235,'69c3fd0357595ecda5675926e33d0d7d3007723eae5912b0b74ca305f61a1681','{"collection":"request","id":"roundtrip-23","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','roundtrip-24',239,'c17ee8cee3676dbeb4c9974c2687af6cc9bfd35ad2a486027877b46503fee4f8','{"collection":"request","id":"roundtrip-24","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','roundtrip-25',243,'1b1575151bca69a27ce49e25d4a601ec77bba8f9bc4b7abe771a813a46c9a298','{"collection":"request","id":"roundtrip-25","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','roundtrip-26',247,'144e1ac11ebdcc05fe671d81bb805dab96a24b7a4c9c5f1415df853fb9b5bf7b','{"collection":"request","id":"roundtrip-26","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','roundtrip-27',251,'a8f1d046dd7995d43dfe5b2eae2e589acb11c36784e00e5f7054a42acf5dff4f','{"collection":"request","id":"roundtrip-27","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','roundtrip-28',255,'3f8fb0757890f94eb5ecff4fed80522dd051ca44968e14655888913183c598cf','{"collection":"request","id":"roundtrip-28","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','roundtrip-29',259,'f4a222db69732fd86a56ef0dc9079c0c46aa2bf954c0fcfe080686522d96fe4a','{"collection":"request","id":"roundtrip-29","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','delete-0',263,'52b1d5de365561d74eae300c8aad5b466c93030326cb7a9a80d9bf771b9fa878','{"collection":"request","id":"delete-0","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','delete-1',264,'6e84191aebe6f2cce5305e159f1065ac7e73f5b857a0a8d6dffa9cda818e0129','{"collection":"request","id":"delete-1","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','delete-2',265,'21eb4b6cb0991ad4e9d0c580b04fe8d31f535297d287eecb15a5de21944fb1c5','{"collection":"request","id":"delete-2","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','delete-3',266,'0c8f017f809b5cdc90e549ef4f6522d1a4c66e611d970b25410c13691dd01db6','{"collection":"request","id":"delete-3","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','delete-4',267,'494af05404917313f0f11db8016531c71139637eec5fe16c918b899cf5882e0c','{"collection":"request","id":"delete-4","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','delete-5',268,'ba506f68c2cb4551631fe1869b91fc8da46c4cbaaa24da690999e17e36f4133d','{"collection":"request","id":"delete-5","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','delete-6',269,'472396bf0e7a502f7f633b6ace6c5f2a7189e781dfc82bee78dfee0ddb8f0627','{"collection":"request","id":"delete-6","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','delete-7',271,'32d923fc16f0bee09633cae5821a870f8bac2fa12e474a76b4c9a89a73c0b524','{"collection":"request","id":"delete-7","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','delete-8',274,'5a08bd2cb14f610fbdc6aa9622281b2929b6e2b91280ecb2ee37f2843ce2fbae','{"collection":"request","id":"delete-8","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','delete-9',275,'751815c30ec2cd6c5b5f72ee43a9c79b10967563ce2c4f48af4b59e6cffdb16e','{"collection":"request","id":"delete-9","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','delete-10',276,'3572f1f60b4c9567964bef8eadafcb8623ea1158058877fa88982bed55873af8','{"collection":"request","id":"delete-10","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','delete-11',277,'8f2fd6cc8575b8550dff5802e778798627c984359d148e1d87694ddaef46d8dd','{"collection":"request","id":"delete-11","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','delete-12',278,'4b587e1528f028a527c78912a77ea45688d2fa01caa4f2bbf76d4d4664cd780a','{"collection":"request","id":"delete-12","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','delete-13',279,'a45d52bb6c12aff6ae32a0707c94835cbbdc691d862c2b365b7a41747268c6a4','{"collection":"request","id":"delete-13","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','delete-14',281,'837cf366f6efc48fbaedb3b3e4275677af105e42eee7b554adcfae996b03c892','{"collection":"request","id":"delete-14","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','delete-15',284,'4dbce2ce0b3e5fae0772da06a3b090254d78c96a7c69a45802149ba4ce8a785c','{"collection":"request","id":"delete-15","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','delete-16',285,'5209cecc500cdadd801c3c38f9988a81a4a7041da53cfa0e2d61006d078f30db','{"collection":"request","id":"delete-16","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','delete-17',286,'97ee5d175d3fdbcc4399726b4949a57219d440659189c7d45fdba7d8c43abc19','{"collection":"request","id":"delete-17","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','delete-18',287,'2276d762cf2ede987a6e1985ce623fdaca5257c0157200126eab40127d864ecf','{"collection":"request","id":"delete-18","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','delete-19',288,'ff0291203091490b4fa5e4e709aee3efc3581455fc7ede59e5fb46f1e11741f5','{"collection":"request","id":"delete-19","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','delete-20',289,'61dcfe43d3196f73e6020ba9e80923d950d9dabcbc18936735855414d8cbad63','{"collection":"request","id":"delete-20","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','failure-answer-notSent',292,'25bd9046685c6831f2164245ee418dd698c7ce7cd9e38066e7ec3b64137ff75c','{"collection":"request","id":"failure-answer-notSent","value":{"accountKey":"default","agent":"abf56fbb-f8dc-4241-ad30-3960b0afa460","connectionId":"connection","createdAt":10.0,"epoch":3,"id":"failure-answer-notSent","method":"item/tool/requestUserInput","params":{"questions":[],"threadId":"thread-0","turnId":"turn-0"},"status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('request','failure-answer-unknown',294,'1ed647a88d4bdbca68cc9d3d716083d9fc75675ba447e5fa5581848451801b68','{"collection":"request","id":"failure-answer-unknown","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','failure-delete-notSent',297,'b2e9c27c2f827c8acd7661d5abc3c7e4920e044bb6573d665294d621cc354e03','{"collection":"request","id":"failure-delete-notSent","value":{"accountKey":"default","agent":"abf56fbb-f8dc-4241-ad30-3960b0afa460","connectionId":"connection","createdAt":10.0,"epoch":3,"id":"failure-delete-notSent","method":"item/tool/requestUserInput","params":{"questions":[],"threadId":"thread-0","turnId":"turn-0"},"status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('request','failure-delete-unknown',299,'22332fdf971cce5670cbf0af80ac0edd66c290601aff3e7bb2b4ea05e678e0b3','{"collection":"request","id":"failure-delete-unknown","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','invalid-0',300,'05d6cb87c4ebb8e149b5224eadafda809f348fb341e89200a07173d1704c61dd','{"collection":"request","id":"invalid-0","value":{"accountKey":"default","agent":"abf56fbb-f8dc-4241-ad30-3960b0afa460","connectionId":"connection","createdAt":10.0,"epoch":3,"id":"invalid-0","method":"item/commandExecution/requestApproval","params":{"threadId":"thread-0","turnId":"turn-0"},"status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('request','invalid-1',301,'cf930ac01b7a68c1ba992b02fc37fc13188d5963a7097ab8ad1083ecc380a10e','{"collection":"request","id":"invalid-1","value":{"accountKey":"default","agent":"abf56fbb-f8dc-4241-ad30-3960b0afa460","connectionId":"connection","createdAt":10.0,"epoch":3,"id":"invalid-1","method":"item/commandExecution/requestApproval","params":{"threadId":"thread-0","turnId":"turn-0"},"status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('request','invalid-2',302,'0d6e8d71e4f0af466b8f05e9a17b2ff560409f4d511e5e17c6fb9ad98e971c28','{"collection":"request","id":"invalid-2","value":{"accountKey":"default","agent":"abf56fbb-f8dc-4241-ad30-3960b0afa460","connectionId":"connection","createdAt":10.0,"epoch":3,"id":"invalid-2","method":"item/commandExecution/requestApproval","params":{"threadId":"thread-0","turnId":"turn-0"},"status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('request','invalid-3',303,'1f6604d38254bee37b2f86548254d7703af265bc6436f39d04786da9850673a9','{"collection":"request","id":"invalid-3","value":{"accountKey":"default","agent":"abf56fbb-f8dc-4241-ad30-3960b0afa460","connectionId":"connection","createdAt":10.0,"epoch":3,"id":"invalid-3","method":"item/fileChange/requestApproval","params":{"threadId":"thread-0","turnId":"turn-0"},"status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('request','invalid-4',304,'6c03cb29f15e79e43bde42695ad147a70c81487fa3474801c810d0520c36ffe8','{"collection":"request","id":"invalid-4","value":{"accountKey":"default","agent":"abf56fbb-f8dc-4241-ad30-3960b0afa460","connectionId":"connection","createdAt":10.0,"epoch":3,"id":"invalid-4","method":"item/fileChange/requestApproval","params":{"threadId":"thread-0","turnId":"turn-0"},"status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('request','invalid-5',305,'a74d5cba144e138d20dbe7f092aec823b2908202e2224516afb23cca333082a6','{"collection":"request","id":"invalid-5","value":{"accountKey":"default","agent":"abf56fbb-f8dc-4241-ad30-3960b0afa460","connectionId":"connection","createdAt":10.0,"epoch":3,"id":"invalid-5","method":"item/fileChange/requestApproval","params":{"threadId":"thread-0","turnId":"turn-0"},"status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('request','invalid-6',307,'0b0a7ad6adabb0179fb323909e2f440f9a86646995c13fc48dc3503a02c77903','{"collection":"request","id":"invalid-6","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','invalid-7',310,'06ae7dcddd9ebc0a4b601dace4550f7d5f6e2e2510f695ab9ea3dfcca81ad9eb','{"collection":"request","id":"invalid-7","value":{"accountKey":"default","agent":"abf56fbb-f8dc-4241-ad30-3960b0afa460","connectionId":"connection","createdAt":10.0,"epoch":3,"id":"invalid-7","method":"mcpServer/elicitation/request","params":{"threadId":"thread-0","turnId":"turn-0"},"status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('request','invalid-8',311,'793a73b6ac4c1199699cc4776b116fb5b2bc51d4c79eeabfb49255794d72d48e','{"collection":"request","id":"invalid-8","value":{"accountKey":"default","agent":"abf56fbb-f8dc-4241-ad30-3960b0afa460","connectionId":"connection","createdAt":10.0,"epoch":3,"id":"invalid-8","method":"mcpServer/elicitation/request","params":{"threadId":"thread-0","turnId":"turn-0"},"status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('request','invalid-9',312,'219a279c94e1b17e8ccd06eb6efa1bd17c161e8646c0e2826b8928306f010a9b','{"collection":"request","id":"invalid-9","value":{"accountKey":"default","agent":"abf56fbb-f8dc-4241-ad30-3960b0afa460","connectionId":"connection","createdAt":10.0,"epoch":3,"id":"invalid-9","method":"item/tool/requestUserInput","params":{"threadId":"thread-0","turnId":"turn-0"},"status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('request','invalid-10',313,'6f6d0be9e0466596d28ecf4935332ba9c060a65b7c6d024c5c3f060f23794609','{"collection":"request","id":"invalid-10","value":{"accountKey":"default","agent":"abf56fbb-f8dc-4241-ad30-3960b0afa460","connectionId":"connection","createdAt":10.0,"epoch":3,"id":"invalid-10","method":"item/tool/requestUserInput","params":{"threadId":"thread-0","turnId":"turn-0"},"status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('request','invalid-11',315,'d2746eb775e3cfd0c343de1ee142d9afcb2285d1b1b0bad61888849abb4a2a96','{"collection":"request","id":"invalid-11","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','race-completed',335,'f4b7fcf436dea35407957546e329c1859bf736c98d54ac9a1e4fd6d8202ca621','{"collection":"request","id":"race-completed","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','race-failed',340,'5f99c703789614fbb4796c7b7f32b6b7002cb2271a2114b0d83256db33ab4f13','{"collection":"request","id":"race-failed","value":{}}',1);
INSERT INTO "sync_entities" VALUES('request','race-resolved',345,'aeeef4714265a62cd15508d6e99cb36f817fb6361a552d5bac83b94d46f64667','{"collection":"request","id":"race-resolved","value":{}}',1);
CREATE TABLE sync_entity_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
INSERT INTO "sync_entity_meta" VALUES('entity_tombstone_count','91');
INSERT INTO "sync_entity_meta" VALUES('event_window_seq','132');
INSERT INTO "sync_entity_meta" VALUES('task_window_migrated','1');
INSERT INTO "sync_entity_meta" VALUES('seeded','1');
INSERT INTO "sync_entity_meta" VALUES('agent_organization_fields','2');
CREATE TABLE sync_generation (id INTEGER PRIMARY KEY, value INTEGER NOT NULL);
INSERT INTO "sync_generation" VALUES(1,0);
CREATE TABLE sync_identity (id TEXT PRIMARY KEY);
INSERT INTO "sync_identity" VALUES('0257042456d74b1d98d79602ce870d31');
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
CREATE INDEX runtime_access_nonce_age ON runtime_access_nonces(created);
CREATE INDEX runtime_access_receipt_age
          ON runtime_access_receipts(json_extract(record,'$.completed')) WHERE status='complete';
CREATE INDEX runtime_access_outbound_age
          ON runtime_access_outbound(json_extract(record,'$.completed'))
          WHERE json_extract(record,'$.outcome')='complete';
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
PRAGMA journal_mode=WAL;
PRAGMA synchronous=FULL;
