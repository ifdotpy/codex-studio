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
INSERT INTO "handles" VALUES('fixture','signature',123,0,0,0,NULL,10.0,NULL,NULL,1);
INSERT INTO "supervisor_state" VALUES('journalId','f439f61019264ac994281b3e560aed6e');
CREATE INDEX operations_monitor_native ON operations(handle,generation,native_id) WHERE substr(operation_id,1,8)='monitor:';
COMMIT;
