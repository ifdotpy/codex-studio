-- Frozen legacy SQLite schema and rows; see the generating commit for provenance.
PRAGMA journal_mode=WAL;
PRAGMA synchronous=FULL;
BEGIN TRANSACTION;
CREATE TABLE child_identities(
                    handle TEXT PRIMARY KEY, pid INTEGER NOT NULL, pgid INTEGER NOT NULL,
                    start_time TEXT NOT NULL, job_name TEXT NOT NULL DEFAULT '',
                    job_kill_on_close INTEGER NOT NULL DEFAULT 0);
CREATE TABLE degraded_handles(
                    handle TEXT PRIMARY KEY, prior_pid INTEGER NOT NULL,
                    start_time TEXT NOT NULL, recovered_at REAL NOT NULL);
CREATE TABLE events(
                    handle TEXT NOT NULL, sequence INTEGER NOT NULL, kind TEXT NOT NULL,
                    payload TEXT NOT NULL, size INTEGER NOT NULL, generation INTEGER NOT NULL DEFAULT 1,
                    PRIMARY KEY(handle,sequence));
CREATE TABLE handles(
                    id TEXT PRIMARY KEY, signature TEXT NOT NULL, pid INTEGER NOT NULL,
                    sequence INTEGER NOT NULL DEFAULT 0, acknowledged INTEGER NOT NULL DEFAULT 0,
                    rpc_sequence INTEGER NOT NULL DEFAULT 0, init_result TEXT, created REAL NOT NULL,
                    closed_at REAL, closed_reason TEXT, generation INTEGER NOT NULL DEFAULT 0);
CREATE TABLE operations(
                    handle TEXT NOT NULL, operation_id TEXT NOT NULL, digest TEXT NOT NULL,
                    native_id INTEGER, accepted REAL NOT NULL, generation INTEGER,
                    response TEXT, response_sequence INTEGER,
                    PRIMARY KEY(handle,operation_id));
CREATE TABLE recovery_events(
                    id INTEGER PRIMARY KEY AUTOINCREMENT, at REAL NOT NULL,
                    handle TEXT NOT NULL, pid INTEGER NOT NULL, start_time TEXT NOT NULL,
                    outcome TEXT NOT NULL, detail TEXT);
CREATE TABLE supervisor_state(
                    key TEXT PRIMARY KEY, value TEXT NOT NULL);
INSERT INTO "child_identities" VALUES('fixture',123,123,'fixture-start','',0);
INSERT INTO "handles" VALUES('fixture','signature',123,0,0,1,NULL,10.0,NULL,NULL,1);
INSERT INTO "operations" VALUES('fixture','reply:claude','a262b1622e93d31ec190b60b7d4f4ed613501ab3715767cb66bc4d6ab8f1e1b5','claude:803e6306-e6a7-4116-bcd3-bb677560a582',1792684372.8929284,1,NULL,NULL);
INSERT INTO "operations" VALUES('fixture','monitor:text','ed0bb768c8bd86d37836aa36e74143e9de2ee0efbe7dd1b825b2ff9d15dac229','claude:monitor',1792684372.8933487,1,NULL,NULL);
INSERT INTO "operations" VALUES('fixture','monitor:integer','ed0bb768c8bd86d37836aa36e74143e9de2ee0efbe7dd1b825b2ff9d15dac229',1,1792684372.8937233,1,NULL,NULL);
INSERT INTO "operations" VALUES('fixture','notification','aba8c91241d7fab24e31ec3a6740ba9d9b3dd12695edbc8f14b78eaf23c9ba55',NULL,1792684372.8940604,1,NULL,NULL);
INSERT INTO "operations" VALUES('fixture','reply:integer','1c9d64ce7719748bf26cd81580ea58245e2a130baf32cd5fc92e08602626c26c',7,1792684372.8943844,1,NULL,NULL);
INSERT INTO "supervisor_state" VALUES('journalId','e7a44b076fa546eaac09fa6f4d3dc226');
CREATE INDEX operations_monitor_native ON operations(handle,generation,native_id) WHERE substr(operation_id,1,8)='monitor:';
COMMIT;
