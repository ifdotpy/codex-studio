PRAGMA foreign_keys=OFF;
BEGIN TRANSACTION;
CREATE TABLE analytics_agents (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE analytics_history (id TEXT PRIMARY KEY, agent TEXT NOT NULL, record TEXT NOT NULL);
CREATE TABLE analytics_items (
            id TEXT PRIMARY KEY, agent TEXT NOT NULL, root TEXT, thread TEXT, turn TEXT,
            at REAL NOT NULL, type TEXT, name TEXT, is_tool INTEGER NOT NULL, record TEXT NOT NULL);
INSERT INTO analytics_items VALUES('measured','agent','agent','thread','turn',201,'reasoning','reasoning',0,'{"id": "measured", "agentId": "agent", "rootId": "agent", "threadId": "thread", "turnId": "turn", "at": 201.0, "startedAt": "wrong", "finishedAt": 203.0, "type": "reasoning", "name": "reasoning", "isTool": false, "firstRecordedAt": 1792689640.5177119, "firstSourceAt": 201.0}');
INSERT INTO analytics_items VALUES('overlap','agent','agent','thread','turn',202,'reasoning','reasoning',0,'{"id": "overlap", "agentId": "agent", "rootId": "agent", "threadId": "thread", "turnId": "turn", "at": 202.0, "startedAt": 202.0, "finishedAt": 204.0, "type": "reasoning", "name": "reasoning", "isTool": false, "firstRecordedAt": 1792689640.5178192, "firstSourceAt": 202.0}');
INSERT INTO analytics_items VALUES('observed','agent','agent','thread','turn',205,'reasoning','reasoning',0,'{"id": "observed", "agentId": "agent", "rootId": "agent", "threadId": "thread", "turnId": "turn", "at": 205.0, "startedAt": 205.0, "finishedAt": null, "type": "reasoning", "name": "reasoning", "isTool": false, "firstRecordedAt": 1792689640.5178344, "firstSourceAt": 205.0}');
CREATE TABLE analytics_limits (
            id INTEGER PRIMARY KEY AUTOINCREMENT, account TEXT NOT NULL, at REAL NOT NULL, record TEXT NOT NULL);
CREATE TABLE analytics_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
INSERT INTO analytics_meta VALUES('trackingSince','1792689637.8667583');
INSERT INTO analytics_meta VALUES('providerIdentityFormat','2026-10-26');
INSERT INTO analytics_meta VALUES('terminalErrorRepairV1','{"ambiguous":0,"cursor":0,"end":0,"repaired":0}');
CREATE TABLE analytics_notifications (
            id TEXT PRIMARY KEY, agent TEXT, root TEXT, method TEXT, hour INTEGER,
            count INTEGER NOT NULL, bytes INTEGER NOT NULL);
CREATE TABLE analytics_turns (
            id TEXT PRIMARY KEY, agent TEXT NOT NULL, root TEXT, at REAL NOT NULL, record TEXT NOT NULL);
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
