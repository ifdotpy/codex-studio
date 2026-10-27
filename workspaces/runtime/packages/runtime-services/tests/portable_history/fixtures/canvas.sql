BEGIN TRANSACTION;
CREATE TABLE graph_agents (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE graph_edges (
                  id TEXT PRIMARY KEY, source TEXT NOT NULL, target TEXT NOT NULL,
                  kind TEXT NOT NULL, UNIQUE(source, target, kind));
CREATE TABLE groups (
                  id TEXT PRIMARY KEY, name TEXT NOT NULL, members TEXT NOT NULL);
CREATE TABLE messages (
                  id TEXT PRIMARY KEY, room TEXT NOT NULL, author TEXT NOT NULL,
                  text TEXT NOT NULL, at REAL NOT NULL, deliveries TEXT NOT NULL);
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
INSERT INTO "runtime_agent_record_generation" VALUES(1,2);
CREATE TABLE runtime_agents (id TEXT PRIMARY KEY, record TEXT NOT NULL);
INSERT INTO "runtime_agents" VALUES('29935609-1946-4400-894e-402340cce1a3','{"id": "29935609-1946-4400-894e-402340cce1a3", "threadId": "native-thread", "accountKey": "default", "executionSettingsAccountKey": "default", "provider": "claude", "yoloMode": true, "name": "portable history fixture", "prompt": "synthetic", "cwd": "<fixture-root>", "role": "orchestrator", "isLead": true, "needsTitle": false, "parentId": null, "rootId": "29935609-1946-4400-894e-402340cce1a3", "subagentConcurrencyVersion": 2, "maxAgentsExplicit": false, "model": "gpt-6-astra", "effort": "medium", "fastMode": false, "daybreakEnabled": false, "cyberAccessProgram": "standard", "concurrency": 32, "maxAgents": 64, "tokenBudget": null, "status": "paused", "autoWake": false, "epoch": 0, "turnId": null, "inFlight": false, "turnEpoch": 0, "usageResumeEnabled": true, "tokensUsed": 0, "compactions": 0, "compactionsObservedOnly": false, "contextUsage": null, "events": 0, "created": 1792684458.5905414, "error": null, "profileId": null, "profileInstructions": "", "tail": "", "worktree": false, "workspaceMode": "worktree", "executionMode": "native", "hostProjectPath": "<fixture-root>", "layrReady": false, "workspaceBackend": null, "workspaceModeExplicit": false, "workspaceInGit": false, "worktreeReady": false, "environment": "host", "worktreeWarning": null, "workerBaseRef": null, "workerBaseCommit": null, "workerBaseBehindMain": null, "workerBaseMainRef": null, "workerDefaults": {"model": "fixture-model"}, "reviewDefaults": {"model": null, "effort": null}, "agentMode": "multi", "agentModeRevision": 0, "agentModeSupported": true, "claudeOptions": {"thinking": "enabled"}}');
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
INSERT INTO "runtime_events" VALUES('fixture-event','29935609-1946-4400-894e-402340cce1a3','user','Python-written original input','delivered',2.5,0,NULL,NULL);
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
INSERT INTO "runtime_item_fulltext" VALUES('fixture-item','Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
Python-written full transcript 😀
');
CREATE TABLE runtime_items (
                  id TEXT PRIMARY KEY, agent TEXT NOT NULL, record TEXT NOT NULL, created REAL NOT NULL);
INSERT INTO "runtime_items" VALUES('fixture-item','29935609-1946-4400-894e-402340cce1a3','{"id": "fixture-item", "role": "assistant", "text": "short stub", "truncated": true, "inputs": [{"id": "fixture-event", "text": "omitted", "truncated": true}], "assets": [{"path": "/tmp/archive-fixture \"日本語.png"}]}',1.25);
INSERT INTO "runtime_items" VALUES('fixture-restored-item','29935609-1946-4400-894e-402340cce1a3','{"id": "fixture-restored-item", "role": "assistant", "text": "restored content must stay out of context", "afterRestore": true}',1.5);
INSERT INTO "runtime_items" VALUES('fixture-restored-empty-1','29935609-1946-4400-894e-402340cce1a3','{"id": "fixture-restored-empty-1", "role": "assistant", "text": "empty afterRestore fixture 1", "afterRestore": []}',2.5);
INSERT INTO "runtime_items" VALUES('fixture-restored-empty-2','29935609-1946-4400-894e-402340cce1a3','{"id": "fixture-restored-empty-2", "role": "assistant", "text": "empty afterRestore fixture 2", "afterRestore": {}}',3.5);
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
INSERT INTO "runtime_projects" VALUES('<fixture-root>','{"id": "<fixture-root>", "path": "<fixture-root>", "name": "store", "created": 1792684458.5907943, "accountKey": "default", "accountRevision": 1, "homeServerId": "4b602f0803ab4d22aedc35593b29ba97", "locations": [{"serverId": "4b602f0803ab4d22aedc35593b29ba97", "path": "<fixture-root>", "projectId": "<fixture-root>"}], "locationsRevision": 0}');
CREATE TABLE runtime_requests (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_rooms (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_rules (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_scheduler_agent_change_state (id INTEGER PRIMARY KEY CHECK(id=1),floor INTEGER NOT NULL,generation INTEGER NOT NULL,entries INTEGER NOT NULL,total INTEGER NOT NULL);
INSERT INTO "runtime_scheduler_agent_change_state" VALUES(1,0,2,1,2);
CREATE TABLE runtime_scheduler_agent_changes (id TEXT PRIMARY KEY,generation INTEGER NOT NULL CHECK(typeof(generation)='integer' AND generation>=0));
INSERT INTO "runtime_scheduler_agent_changes" VALUES('29935609-1946-4400-894e-402340cce1a3',2);
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
INSERT INTO "runtime_search_rollout" VALUES(1,'building',0,1792684458.5333519);
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
CREATE TABLE runtime_tool_requests (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_tool_results (id TEXT PRIMARY KEY, result TEXT NOT NULL);
CREATE TABLE runtime_turn_item_link_state_v1 (id INTEGER PRIMARY KEY CHECK(id=1),epoch TEXT NOT NULL,source_rootpage INTEGER NOT NULL,cursor INTEGER,target INTEGER,complete INTEGER NOT NULL CHECK(complete IN (0,1)),scanned_rows INTEGER NOT NULL,source_bytes INTEGER NOT NULL,updated REAL NOT NULL);
INSERT INTO "runtime_turn_item_link_state_v1" VALUES(1,'3d39e467cb0b422fa6674add8a63d83d',117,NULL,NULL,1,0,0,1792684458.4766257);
CREATE TABLE runtime_turn_item_links_v1 (id TEXT PRIMARY KEY,agent TEXT NOT NULL,turn_id,created REAL NOT NULL,epoch TEXT NOT NULL);
INSERT INTO "runtime_turn_item_links_v1" VALUES('fixture-item','29935609-1946-4400-894e-402340cce1a3',NULL,1.25,'3d39e467cb0b422fa6674add8a63d83d');
INSERT INTO "runtime_turn_item_links_v1" VALUES('fixture-restored-item','29935609-1946-4400-894e-402340cce1a3',NULL,1.5,'3d39e467cb0b422fa6674add8a63d83d');
INSERT INTO "runtime_turn_item_links_v1" VALUES('fixture-restored-empty-1','29935609-1946-4400-894e-402340cce1a3',NULL,2.5,'3d39e467cb0b422fa6674add8a63d83d');
INSERT INTO "runtime_turn_item_links_v1" VALUES('fixture-restored-empty-2','29935609-1946-4400-894e-402340cce1a3',NULL,3.5,'3d39e467cb0b422fa6674add8a63d83d');
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
INSERT INTO "sync_entities" VALUES('project','<fixture-root>',1,'d7dbf1bcddceb2070efa7869c9198f156dc2639211ca469cfa4b487cd45d287c','{"collection":"project","id":"<fixture-root>","value":{"accountKey":"default","accountRevision":1,"created":1792684458.5907943,"homeServerId":"4b602f0803ab4d22aedc35593b29ba97","id":"<fixture-root>","locations":[{"path":"<fixture-root>","projectId":"<fixture-root>","serverId":"4b602f0803ab4d22aedc35593b29ba97"}],"locationsRevision":0,"name":"store","path":"<fixture-root>"}}',0);
INSERT INTO "sync_entities" VALUES('agent','29935609-1946-4400-894e-402340cce1a3',3,'d501f985c8d8a1a40c3d6a6024a4e7e159eac38f4e7d779e7a9174bde4e8e3fd','{"collection": "agent", "id": "29935609-1946-4400-894e-402340cce1a3", "value": {"accountKey": "default", "agentMode": "multi", "agentModeRevision": 0, "agentModeSupported": true, "autoWake": false, "canSend": true, "compactions": 0, "concurrency": 32, "contextUsage": null, "created": 1792684458.5905414, "cwd": "<fixture-root>", "cyberAccessProgram": "standard", "daybreakEnabled": false, "effort": "medium", "empty": false, "environment": "host", "epoch": 0, "error": null, "executionMode": "native", "fastMode": false, "hostProjectPath": "<fixture-root>", "id": "29935609-1946-4400-894e-402340cce1a3", "inFlight": false, "isLead": true, "kind": "agent", "launcherAlive": true, "layrReady": false, "model": "gpt-6-astra", "name": "portable history fixture", "nextTurnSettingsSupported": true, "parentId": null, "provider": "claude", "readStateSupported": true, "reviewDefaults": {"effort": null, "model": null}, "role": "orchestrator", "rootId": "29935609-1946-4400-894e-402340cce1a3", "serverId": "4b602f0803ab4d22aedc35593b29ba97", "source": "managed", "status": "paused", "subagentConcurrencyVersion": 2, "tail": "", "threadId": "native-thread", "tokensUsed": 0, "turnId": null, "workspaceBackend": null, "workspaceMode": "worktree", "worktree": false, "yoloMode": true}}',0);
INSERT INTO "sync_entities" VALUES('event','fixture-event',4,'097afdb8bcde0141fa9924f4061888ac344b81844ca485419d68045938ab3476','{"collection":"event","id":"fixture-event","value":{"agent":"29935609-1946-4400-894e-402340cce1a3","created":2.5,"error":null,"id":"fixture-event","kind":"user","status":"delivered"}}',0);
CREATE TABLE sync_entity_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
INSERT INTO "sync_entity_meta" VALUES('entity_tombstone_count','0');
INSERT INTO "sync_entity_meta" VALUES('event_window_seq','4');
CREATE TABLE sync_versions (
        seq INTEGER PRIMARY KEY, scope TEXT NOT NULL UNIQUE,
        hash TEXT NOT NULL, deleted INTEGER NOT NULL, updated REAL NOT NULL);
CREATE TABLE user_terminal_decoder_state (terminal TEXT PRIMARY KEY, pending BLOB NOT NULL);
CREATE TABLE user_terminal_event_cursor (terminal TEXT PRIMARY KEY, sequence INTEGER NOT NULL);
CREATE TABLE user_terminal_output (terminal TEXT NOT NULL, start INTEGER NOT NULL, end INTEGER NOT NULL, text TEXT NOT NULL, PRIMARY KEY (terminal,start));
CREATE TABLE user_terminal_receipts (id TEXT PRIMARY KEY, signature TEXT NOT NULL, result TEXT NOT NULL);
CREATE TABLE user_terminals (id TEXT PRIMARY KEY, record TEXT NOT NULL, output TEXT NOT NULL, offset INTEGER NOT NULL);
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
CREATE INDEX messages_room ON messages(room, at);
CREATE INDEX user_terminal_output_end ON user_terminal_output (terminal,end);
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
PRAGMA writable_schema=OFF;
DELETE FROM "sqlite_sequence";
COMMIT;
