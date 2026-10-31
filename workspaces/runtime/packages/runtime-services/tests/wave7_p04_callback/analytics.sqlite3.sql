PRAGMA foreign_keys=OFF;
BEGIN TRANSACTION;
CREATE TABLE analytics_agents (id TEXT PRIMARY KEY, record TEXT NOT NULL);
INSERT INTO analytics_agents VALUES('abf56fbb-f8dc-4241-ad30-3960b0afa460','{"id": "abf56fbb-f8dc-4241-ad30-3960b0afa460", "name": "Callback lead", "rootId": "abf56fbb-f8dc-4241-ad30-3960b0afa460", "parentId": null, "accountKey": "default", "accountHistory": [], "provider": "codex", "threadId": "thread-0", "model": "gpt-6-astra", "effort": "medium", "fastMode": false, "daybreakEnabled": false, "cyberAccessProgram": "standard", "cwd": "__CALLBACK_STATE__", "deletedAt": null}');
CREATE TABLE analytics_history (id TEXT PRIMARY KEY, agent TEXT NOT NULL, record TEXT NOT NULL);
CREATE TABLE analytics_items (
            id TEXT PRIMARY KEY, agent TEXT NOT NULL, root TEXT, thread TEXT, turn TEXT,
            at REAL NOT NULL, type TEXT, name TEXT, is_tool INTEGER NOT NULL, record TEXT NOT NULL);
INSERT INTO analytics_items VALUES('abf56fbb-f8dc-4241-ad30-3960b0afa460:thread-0:warmup','abf56fbb-f8dc-4241-ad30-3960b0afa460','abf56fbb-f8dc-4241-ad30-3960b0afa460','thread-0','turn-0',1792684447.7596304,'dynamicToolCall','orchestration_task',1,'{"agentId": "abf56fbb-f8dc-4241-ad30-3960b0afa460", "agentName": "Callback lead", "rootId": "abf56fbb-f8dc-4241-ad30-3960b0afa460", "accountKey": "default", "accountHistory": [], "provider": "codex", "threadId": "thread-0", "model": "gpt-6-astra", "effort": "medium", "fastMode": false, "daybreakEnabled": false, "cyberAccessProgram": "standard", "turnId": "turn-0", "id": "abf56fbb-f8dc-4241-ad30-3960b0afa460:thread-0:warmup", "itemId": "warmup", "startedAt": null, "at": 1792684447.7596304, "source": "live", "input": {"bytes": 17, "chars": 17, "lines": 1, "imageCount": 0, "imageBytes": 0, "images": [], "format": "json"}, "output": {"bytes": 553, "chars": 553, "lines": 1, "imageCount": 0, "imageBytes": 0, "images": [], "format": "json"}, "payloadBoundary": "protocol", "type": "dynamicToolCall", "name": "orchestration_task", "recordedAt": 1792684447.7599607, "isTool": true, "timestampSource": "observed", "finishedAt": 1792684447.7596304, "status": "completed", "durationMs": null, "durationSource": null, "coverage": "protocol_payload", "payloadTruncated": null, "firstRecordedAt": 1792684447.7599607, "firstSourceAt": 1792684447.7596304}');
INSERT INTO analytics_items VALUES('abf56fbb-f8dc-4241-ad30-3960b0afa460:thread-0:dynamic-0','abf56fbb-f8dc-4241-ad30-3960b0afa460','abf56fbb-f8dc-4241-ad30-3960b0afa460','thread-0','turn-0',1792684447.763507,'dynamicToolCall','orchestration_task',1,'{"agentId": "abf56fbb-f8dc-4241-ad30-3960b0afa460", "agentName": "Callback lead", "rootId": "abf56fbb-f8dc-4241-ad30-3960b0afa460", "accountKey": "default", "accountHistory": [], "provider": "codex", "threadId": "thread-0", "model": "gpt-6-astra", "effort": "medium", "fastMode": false, "daybreakEnabled": false, "cyberAccessProgram": "standard", "turnId": "turn-0", "id": "abf56fbb-f8dc-4241-ad30-3960b0afa460:thread-0:dynamic-0", "itemId": "dynamic-0", "startedAt": null, "at": 1792684447.7635074, "source": "live", "input": {"bytes": 17, "chars": 17, "lines": 1, "imageCount": 0, "imageBytes": 0, "images": [], "format": "json"}, "output": {"bytes": 243, "chars": 243, "lines": 1, "imageCount": 0, "imageBytes": 0, "images": [], "format": "json"}, "payloadBoundary": "protocol", "type": "dynamicToolCall", "name": "orchestration_task", "recordedAt": 1792684447.7638032, "isTool": true, "timestampSource": "observed", "finishedAt": 1792684447.7635074, "status": "completed", "durationMs": null, "durationSource": null, "coverage": "protocol_payload", "payloadTruncated": null, "firstRecordedAt": 1792684447.7638032, "firstSourceAt": 1792684447.7635074}');
INSERT INTO analytics_items VALUES('abf56fbb-f8dc-4241-ad30-3960b0afa460:thread-0:dynamic-1','abf56fbb-f8dc-4241-ad30-3960b0afa460','abf56fbb-f8dc-4241-ad30-3960b0afa460','thread-0','turn-0',1792684447.7668204,'dynamicToolCall','orchestration_context',1,'{"agentId": "abf56fbb-f8dc-4241-ad30-3960b0afa460", "agentName": "Callback lead", "rootId": "abf56fbb-f8dc-4241-ad30-3960b0afa460", "accountKey": "default", "accountHistory": [], "provider": "codex", "threadId": "thread-0", "model": "gpt-6-astra", "effort": "medium", "fastMode": false, "daybreakEnabled": false, "cyberAccessProgram": "standard", "turnId": "turn-0", "id": "abf56fbb-f8dc-4241-ad30-3960b0afa460:thread-0:dynamic-1", "itemId": "dynamic-1", "startedAt": null, "at": 1792684447.7668204, "source": "live", "input": {"bytes": 16, "chars": 16, "lines": 1, "imageCount": 0, "imageBytes": 0, "images": [], "format": "json"}, "output": {"bytes": 212, "chars": 212, "lines": 1, "imageCount": 0, "imageBytes": 0, "images": [], "format": "json"}, "payloadBoundary": "protocol", "type": "dynamicToolCall", "name": "orchestration_context", "recordedAt": 1792684447.767103, "isTool": true, "timestampSource": "observed", "finishedAt": 1792684447.7668204, "status": "completed", "durationMs": null, "durationSource": null, "coverage": "protocol_payload", "payloadTruncated": null, "firstRecordedAt": 1792684447.767103, "firstSourceAt": 1792684447.7668204}');
INSERT INTO analytics_items VALUES('abf56fbb-f8dc-4241-ad30-3960b0afa460:thread-0:dynamic-2','abf56fbb-f8dc-4241-ad30-3960b0afa460','abf56fbb-f8dc-4241-ad30-3960b0afa460','thread-0','turn-0',1792684447.769569,'dynamicToolCall','orchestration_title',1,'{"agentId": "abf56fbb-f8dc-4241-ad30-3960b0afa460", "agentName": "Callback lead", "rootId": "abf56fbb-f8dc-4241-ad30-3960b0afa460", "accountKey": "default", "accountHistory": [], "provider": "codex", "threadId": "thread-0", "model": "gpt-6-astra", "effort": "medium", "fastMode": false, "daybreakEnabled": false, "cyberAccessProgram": "standard", "turnId": "turn-0", "id": "abf56fbb-f8dc-4241-ad30-3960b0afa460:thread-0:dynamic-2", "itemId": "dynamic-2", "startedAt": null, "at": 1792684447.769569, "source": "live", "input": {"bytes": 30, "chars": 27, "lines": 1, "imageCount": 0, "imageBytes": 0, "images": [], "format": "json"}, "output": {"bytes": 164, "chars": 161, "lines": 1, "imageCount": 0, "imageBytes": 0, "images": [], "format": "json"}, "payloadBoundary": "protocol", "type": "dynamicToolCall", "name": "orchestration_title", "recordedAt": 1792684447.7698326, "isTool": true, "timestampSource": "observed", "finishedAt": 1792684447.769569, "status": "completed", "durationMs": null, "durationSource": null, "coverage": "protocol_payload", "payloadTruncated": null, "firstRecordedAt": 1792684447.7698326, "firstSourceAt": 1792684447.769569}');
CREATE TABLE analytics_limits (
            id INTEGER PRIMARY KEY AUTOINCREMENT, account TEXT NOT NULL, at REAL NOT NULL, record TEXT NOT NULL);
CREATE TABLE analytics_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
INSERT INTO analytics_meta VALUES('trackingSince','1792684446.7271178');
INSERT INTO analytics_meta VALUES('providerIdentityFormat','2026-10-26');
INSERT INTO analytics_meta VALUES('terminalErrorRepairV1','{"ambiguous":0,"cursor":1,"end":1,"repaired":0}');
CREATE TABLE analytics_notifications (
            id TEXT PRIMARY KEY, agent TEXT, root TEXT, method TEXT, hour INTEGER,
            count INTEGER NOT NULL, bytes INTEGER NOT NULL);
INSERT INTO analytics_notifications VALUES('["abf56fbb-f8dc-4241-ad30-3960b0afa460","item/completed",1792681200]','abf56fbb-f8dc-4241-ad30-3960b0afa460','abf56fbb-f8dc-4241-ad30-3960b0afa460','item/completed',1792681200,4,2238);
CREATE TABLE analytics_turns (
            id TEXT PRIMARY KEY, agent TEXT NOT NULL, root TEXT, at REAL NOT NULL, record TEXT NOT NULL);
INSERT INTO analytics_turns VALUES('abf56fbb-f8dc-4241-ad30-3960b0afa460:thread-0:turn-0','abf56fbb-f8dc-4241-ad30-3960b0afa460','abf56fbb-f8dc-4241-ad30-3960b0afa460',1792684447.7596304,'{"agentId": "abf56fbb-f8dc-4241-ad30-3960b0afa460", "agentName": "Callback lead", "rootId": "abf56fbb-f8dc-4241-ad30-3960b0afa460", "accountKey": "default", "accountHistory": [], "provider": "codex", "threadId": "thread-0", "model": "gpt-6-astra", "effort": "medium", "fastMode": false, "daybreakEnabled": false, "cyberAccessProgram": "standard", "turnId": "turn-0", "id": "abf56fbb-f8dc-4241-ad30-3960b0afa460:thread-0:turn-0", "at": 1792684447.7596304, "startedAt": null, "finishedAt": null, "firstOutputAt": null, "durationMs": null, "firstOutputDelayMs": null, "status": "unknown", "source": "live"}');
CREATE TABLE analytics_usage (
            seq INTEGER PRIMARY KEY AUTOINCREMENT, id TEXT UNIQUE NOT NULL,
            agent TEXT NOT NULL, root TEXT, thread TEXT, turn TEXT, at REAL NOT NULL, record TEXT NOT NULL);
CREATE TABLE analytics_usage_roots (root TEXT PRIMARY KEY, generation INTEGER NOT NULL);
CREATE TABLE analytics_cost_rows (
    id TEXT PRIMARY KEY, root TEXT NOT NULL, agent TEXT NOT NULL, thread TEXT, turn TEXT,
    seq INTEGER NOT NULL, at REAL NOT NULL, model TEXT, account_key TEXT, response TEXT,
    input REAL, cached REAL, cache_write REAL, output REAL, uncached INTEGER NOT NULL,
    counted INTEGER NOT NULL, counted_model TEXT, eligible INTEGER, final_evidence INTEGER NOT NULL, cache_write_1h REAL);
CREATE TABLE analytics_cost_groups (
    root TEXT NOT NULL, model TEXT, account TEXT NOT NULL, input REAL, cached REAL,
    cache_write REAL, output REAL, uncached INTEGER NOT NULL, count INTEGER NOT NULL, cache_write_1h REAL);
CREATE TABLE analytics_usage_revision_clock (revision INTEGER NOT NULL);
INSERT INTO analytics_usage_revision_clock VALUES(0);
CREATE TABLE analytics_usage_revisions (id TEXT PRIMARY KEY, revision INTEGER NOT NULL);
CREATE TABLE analytics_cost_group_revisions (id INTEGER PRIMARY KEY, revision INTEGER NOT NULL);
CREATE TABLE analytics_stored_text_dictionaries (
        id INTEGER PRIMARY KEY, name TEXT NOT NULL, created REAL NOT NULL,
        sha256 TEXT NOT NULL, dictionary BLOB NOT NULL);
CREATE TABLE runtime_payload_migrations (name TEXT PRIMARY KEY,cursor INTEGER NOT NULL DEFAULT 0,complete INTEGER NOT NULL DEFAULT 0,status TEXT NOT NULL DEFAULT 'pending',updated REAL,error TEXT);
CREATE TABLE runtime_stored_text_batches (
        name TEXT NOT NULL, batch INTEGER NOT NULL, first_rowid INTEGER NOT NULL,
        last_rowid INTEGER NOT NULL, rows INTEGER NOT NULL, plain_bytes INTEGER NOT NULL,
        stored_bytes INTEGER NOT NULL, sha256 TEXT NOT NULL, created REAL NOT NULL,
        PRIMARY KEY(name, batch));
CREATE TRIGGER analytics_usage_revision_insert AFTER INSERT ON analytics_usage BEGIN
           UPDATE analytics_usage_revision_clock SET revision=revision+1;
           INSERT INTO analytics_usage_revisions VALUES(NEW.id,(SELECT revision FROM analytics_usage_revision_clock))
             ON CONFLICT(id) DO UPDATE SET revision=excluded.revision;
         END;
CREATE TRIGGER analytics_usage_revision_update AFTER UPDATE ON analytics_usage BEGIN
           UPDATE analytics_usage_revision_clock SET revision=revision+1;
           INSERT INTO analytics_usage_revisions VALUES(OLD.id,(SELECT revision FROM analytics_usage_revision_clock))
             ON CONFLICT(id) DO UPDATE SET revision=excluded.revision;
           INSERT INTO analytics_usage_revisions VALUES(NEW.id,(SELECT revision FROM analytics_usage_revision_clock))
             ON CONFLICT(id) DO UPDATE SET revision=excluded.revision;
         END;
CREATE TRIGGER analytics_usage_revision_delete AFTER DELETE ON analytics_usage BEGIN
           UPDATE analytics_usage_revision_clock SET revision=revision+1;
           INSERT INTO analytics_usage_revisions VALUES(OLD.id,(SELECT revision FROM analytics_usage_revision_clock))
             ON CONFLICT(id) DO UPDATE SET revision=excluded.revision;
         END;
CREATE TRIGGER analytics_cost_group_revision_insert AFTER INSERT ON analytics_cost_groups BEGIN
            UPDATE analytics_usage_revision_clock SET revision=revision+1;
            INSERT INTO analytics_cost_group_revisions VALUES(NEW.rowid,(SELECT revision FROM analytics_usage_revision_clock))
                ON CONFLICT(id) DO UPDATE SET revision=excluded.revision;
        END;
CREATE TRIGGER analytics_cost_group_revision_update AFTER UPDATE ON analytics_cost_groups BEGIN
            UPDATE analytics_usage_revision_clock SET revision=revision+1;
            INSERT INTO analytics_cost_group_revisions VALUES(OLD.rowid,(SELECT revision FROM analytics_usage_revision_clock))
                ON CONFLICT(id) DO UPDATE SET revision=excluded.revision;
            INSERT INTO analytics_cost_group_revisions VALUES(NEW.rowid,(SELECT revision FROM analytics_usage_revision_clock))
                ON CONFLICT(id) DO UPDATE SET revision=excluded.revision;
        END;
CREATE TRIGGER analytics_cost_group_revision_delete AFTER DELETE ON analytics_cost_groups BEGIN
            UPDATE analytics_usage_revision_clock SET revision=revision+1;
            INSERT INTO analytics_cost_group_revisions VALUES(OLD.rowid,(SELECT revision FROM analytics_usage_revision_clock))
                ON CONFLICT(id) DO UPDATE SET revision=excluded.revision;
        END;
CREATE TRIGGER analytics_stored_text_dictionaries_no_update
        BEFORE UPDATE ON analytics_stored_text_dictionaries BEGIN
        SELECT RAISE(ABORT, 'stored text dictionaries are immutable'); END;
CREATE TRIGGER analytics_stored_text_dictionaries_no_delete
        BEFORE DELETE ON analytics_stored_text_dictionaries BEGIN
        SELECT RAISE(ABORT, 'stored text dictionaries are immutable'); END;
CREATE TRIGGER analytics_stored_text_dictionaries_no_replace
        BEFORE INSERT ON analytics_stored_text_dictionaries
        WHEN EXISTS(SELECT 1 FROM analytics_stored_text_dictionaries WHERE id=NEW.id) BEGIN
        SELECT RAISE(ABORT, 'stored text dictionaries are immutable'); END;
CREATE INDEX analytics_turns_scope ON analytics_turns(agent,at);
CREATE INDEX analytics_limits_account_at ON analytics_limits(account, at);
CREATE INDEX analytics_usage_scope ON analytics_usage(agent,at);
CREATE INDEX analytics_usage_team ON analytics_usage(root,at);
CREATE INDEX analytics_usage_root_seq ON analytics_usage(root,seq);
CREATE INDEX analytics_usage_response ON analytics_usage(agent,thread,json_extract(record,'$.responseId'));
CREATE INDEX analytics_usage_thread ON analytics_usage(agent,thread,seq);
CREATE INDEX analytics_usage_migration ON analytics_usage(agent,seq);
CREATE INDEX analytics_items_scope ON analytics_items(agent,at);
CREATE INDEX analytics_items_team ON analytics_items(root,at);
CREATE INDEX analytics_items_tool ON analytics_items(name,at);
CREATE INDEX analytics_agents_root ON analytics_agents(json_extract(record,'$.rootId'));
CREATE INDEX analytics_cost_rows_scope ON analytics_cost_rows(root, agent, thread, turn);
CREATE INDEX analytics_cost_rows_response ON analytics_cost_rows(root, thread, response)
    WHERE response IS NOT NULL;
CREATE INDEX analytics_cost_rows_agent_response
    ON analytics_cost_rows(root,agent,response) WHERE response IS NOT NULL;
CREATE INDEX analytics_cost_rows_eligible_unmodeled
    ON analytics_cost_rows(root,agent) WHERE model IS NULL AND eligible IS NOT 0;
CREATE INDEX analytics_cost_groups_cover
    ON analytics_cost_groups(root,model,account,input,cached,cache_write,output,uncached,cache_write_1h,count);
CREATE INDEX analytics_usage_revisions_revision ON analytics_usage_revisions(revision);
CREATE INDEX analytics_cost_group_revisions_revision ON analytics_cost_group_revisions(revision);
CREATE INDEX analytics_stored_text_dictionaries_name
        ON analytics_stored_text_dictionaries(name, created, id);
COMMIT;
PRAGMA journal_mode=WAL;
PRAGMA synchronous=NORMAL;
