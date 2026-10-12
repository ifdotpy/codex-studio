-- section: common
CREATE TABLE IF NOT EXISTS sync_identity (id TEXT PRIMARY KEY);
CREATE TABLE IF NOT EXISTS sync_generation (id INTEGER PRIMARY KEY, value INTEGER NOT NULL);
INSERT OR IGNORE INTO sync_generation VALUES (1, 0);
CREATE TABLE IF NOT EXISTS sync_documents (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    scope TEXT NOT NULL,
    id TEXT NOT NULL,
    payload TEXT NOT NULL,
    deleted INTEGER NOT NULL DEFAULT 0,
    UNIQUE(scope, id)
);
CREATE INDEX IF NOT EXISTS sync_scope_seq ON sync_documents(scope,seq);
CREATE TABLE IF NOT EXISTS sync_entities (
    collection TEXT NOT NULL,
    id TEXT NOT NULL,
    seq INTEGER NOT NULL,
    hash TEXT NOT NULL,
    payload TEXT,
    deleted INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY(collection, id)
);
CREATE INDEX IF NOT EXISTS sync_entities_seq ON sync_entities(seq);
CREATE INDEX IF NOT EXISTS sync_entities_collection_seq ON sync_entities(collection,seq);
CREATE INDEX IF NOT EXISTS sync_entities_non_transcript_seq ON sync_entities(seq)
        WHERE collection NOT LIKE 'transcript:%';
CREATE INDEX IF NOT EXISTS sync_entities_collection_deleted
        ON sync_entities(collection,deleted);
CREATE INDEX IF NOT EXISTS sync_entities_tombstone_order
        ON sync_entities(seq,collection,id)
        WHERE deleted=1 AND collection NOT LIKE 'transcript:%';
CREATE TABLE IF NOT EXISTS sync_entity_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS sync_versions (
    seq INTEGER PRIMARY KEY,
    scope TEXT NOT NULL UNIQUE,
    hash TEXT NOT NULL,
    deleted INTEGER NOT NULL,
    updated REAL NOT NULL
);
CREATE TRIGGER IF NOT EXISTS sync_entity_tombstone_count_insert
      AFTER INSERT ON sync_entities
      WHEN NEW.deleted=1 AND NEW.collection NOT LIKE 'transcript:%' BEGIN
        UPDATE sync_entity_meta SET value=CAST(value AS INTEGER)+1
        WHERE key='entity_tombstone_count';
      END;
CREATE TRIGGER IF NOT EXISTS sync_entity_tombstone_count_update
      AFTER UPDATE OF deleted,collection ON sync_entities
      WHEN (OLD.deleted=1 AND OLD.collection NOT LIKE 'transcript:%') !=
           (NEW.deleted=1 AND NEW.collection NOT LIKE 'transcript:%') BEGIN
        UPDATE sync_entity_meta SET value=CAST(value AS INTEGER)+
          CASE WHEN NEW.deleted=1 AND NEW.collection NOT LIKE 'transcript:%' THEN 1 ELSE -1 END
        WHERE key='entity_tombstone_count';
      END;
CREATE TRIGGER IF NOT EXISTS sync_entity_tombstone_count_delete
      AFTER DELETE ON sync_entities
      WHEN OLD.deleted=1 AND OLD.collection NOT LIKE 'transcript:%' BEGIN
        UPDATE sync_entity_meta SET value=CAST(value AS INTEGER)-1
        WHERE key='entity_tombstone_count';
      END;
INSERT INTO sync_entity_meta(key, value)
SELECT 'entity_tombstone_count', (
    SELECT CAST(COUNT(*) AS TEXT) FROM sync_entities
    WHERE deleted=1 AND collection NOT LIKE 'transcript:%'
)
WHERE NOT EXISTS (
    SELECT 1 FROM sync_entity_meta WHERE key='entity_tombstone_count'
);
INSERT INTO sync_identity(id)
SELECT lower(hex(randomblob(16)))
WHERE NOT EXISTS (SELECT 1 FROM sync_identity);

-- section: runtime_events
CREATE INDEX IF NOT EXISTS runtime_event_created_id ON runtime_events(created DESC,id);
CREATE TRIGGER IF NOT EXISTS sync_entity_event_INSERT
          AFTER INSERT ON runtime_events BEGIN
          INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted)
          VALUES ('event',NEW.id,(SELECT max(seq)+1 FROM (
                    SELECT COALESCE(MAX(seq),0) seq FROM sync_entities UNION ALL
                    SELECT COALESCE(MAX(seq),0) FROM sync_documents UNION ALL
                    SELECT COALESCE(MAX(seq),0) FROM sync_versions)),
                  sync_entity_hash(sync_entity_payload('event',NEW.id,json_object('id',NEW.id,'agent',NEW.agent,'kind',NEW.kind,'status',NEW.status,'created',NEW.created,'error',NEW.error),0)),sync_entity_payload('event',NEW.id,json_object('id',NEW.id,'agent',NEW.agent,'kind',NEW.kind,'status',NEW.status,'created',NEW.created,'error',NEW.error),0),0)
          ON CONFLICT(collection,id) DO UPDATE SET
            seq=excluded.seq,hash=excluded.hash,payload=excluded.payload,deleted=excluded.deleted
          WHERE sync_entities.hash!=excluded.hash OR sync_entities.deleted!=excluded.deleted;
        END;
CREATE TRIGGER IF NOT EXISTS sync_entity_event_UPDATE
          AFTER UPDATE ON runtime_events BEGIN
          INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted)
          VALUES ('event',NEW.id,(SELECT max(seq)+1 FROM (
                    SELECT COALESCE(MAX(seq),0) seq FROM sync_entities UNION ALL
                    SELECT COALESCE(MAX(seq),0) FROM sync_documents UNION ALL
                    SELECT COALESCE(MAX(seq),0) FROM sync_versions)),
                  sync_entity_hash(sync_entity_payload('event',NEW.id,json_object('id',NEW.id,'agent',NEW.agent,'kind',NEW.kind,'status',NEW.status,'created',NEW.created,'error',NEW.error),0)),sync_entity_payload('event',NEW.id,json_object('id',NEW.id,'agent',NEW.agent,'kind',NEW.kind,'status',NEW.status,'created',NEW.created,'error',NEW.error),0),0)
          ON CONFLICT(collection,id) DO UPDATE SET
            seq=excluded.seq,hash=excluded.hash,payload=excluded.payload,deleted=excluded.deleted
          WHERE sync_entities.hash!=excluded.hash OR sync_entities.deleted!=excluded.deleted;
        END;
CREATE TRIGGER IF NOT EXISTS sync_entity_event_DELETE
          AFTER DELETE ON runtime_events BEGIN
          INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted)
          VALUES ('event',OLD.id,(SELECT max(seq)+1 FROM (
                    SELECT COALESCE(MAX(seq),0) seq FROM sync_entities UNION ALL
                    SELECT COALESCE(MAX(seq),0) FROM sync_documents UNION ALL
                    SELECT COALESCE(MAX(seq),0) FROM sync_versions)),
                  sync_entity_hash(sync_entity_payload('event',OLD.id,'{}',1)),sync_entity_payload('event',OLD.id,'{}',1),1)
          ON CONFLICT(collection,id) DO UPDATE SET
            seq=excluded.seq,hash=excluded.hash,payload=excluded.payload,deleted=excluded.deleted
          WHERE sync_entities.hash!=excluded.hash OR sync_entities.deleted!=excluded.deleted;
        END;

-- section: runtime_agents
CREATE TABLE IF NOT EXISTS runtime_transcript_revisions(
    agent TEXT PRIMARY KEY,
    revision INTEGER NOT NULL DEFAULT 0
);
CREATE TRIGGER IF NOT EXISTS sync_transcript_revision_runtime_agents_insert AFTER INSERT ON runtime_agents BEGIN INSERT INTO runtime_transcript_revisions(agent,revision)
                                    VALUES (NEW.id,1) ON CONFLICT(agent) DO UPDATE SET revision=revision+1; END;
CREATE TRIGGER IF NOT EXISTS sync_transcript_revision_runtime_agents_update AFTER UPDATE ON runtime_agents BEGIN INSERT INTO runtime_transcript_revisions(agent,revision)
                                    VALUES (NEW.id,1) ON CONFLICT(agent) DO UPDATE SET revision=revision+1;
                                    INSERT INTO runtime_transcript_revisions(agent,revision)
                                    SELECT OLD.id,1 WHERE OLD.id!=NEW.id
                                    ON CONFLICT(agent) DO UPDATE SET revision=revision+1; END;
CREATE TRIGGER IF NOT EXISTS sync_transcript_revision_runtime_agents_delete AFTER DELETE ON runtime_agents BEGIN INSERT INTO runtime_transcript_revisions(agent,revision)
                                    VALUES (OLD.id,1) ON CONFLICT(agent) DO UPDATE SET revision=revision+1; END;

-- section: runtime_items
CREATE TRIGGER IF NOT EXISTS sync_transcript_revision_runtime_items_insert AFTER INSERT ON runtime_items BEGIN INSERT INTO runtime_transcript_revisions(agent,revision)
                                    VALUES (NEW.agent,1) ON CONFLICT(agent) DO UPDATE SET revision=revision+1; END;
CREATE TRIGGER IF NOT EXISTS sync_transcript_revision_runtime_items_update AFTER UPDATE ON runtime_items BEGIN INSERT INTO runtime_transcript_revisions(agent,revision)
                                    VALUES (NEW.agent,1) ON CONFLICT(agent) DO UPDATE SET revision=revision+1;
                                    INSERT INTO runtime_transcript_revisions(agent,revision)
                                    SELECT OLD.agent,1 WHERE OLD.agent!=NEW.agent
                                    ON CONFLICT(agent) DO UPDATE SET revision=revision+1; END;
CREATE TRIGGER IF NOT EXISTS sync_transcript_revision_runtime_items_delete AFTER DELETE ON runtime_items BEGIN INSERT INTO runtime_transcript_revisions(agent,revision)
                                    VALUES (OLD.agent,1) ON CONFLICT(agent) DO UPDATE SET revision=revision+1; END;
