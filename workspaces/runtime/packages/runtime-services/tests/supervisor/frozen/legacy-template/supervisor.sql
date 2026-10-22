-- Frozen legacy SQLite schema and rows; see the generating commit for provenance.
PRAGMA journal_mode=WAL;
PRAGMA synchronous=FULL;
BEGIN TRANSACTION;
CREATE TABLE child_identities(
                    handle TEXT PRIMARY KEY, pid INTEGER NOT NULL, pgid INTEGER NOT NULL,
                    start_time TEXT NOT NULL);
CREATE TABLE degraded_handles(
                    handle TEXT PRIMARY KEY, prior_pid INTEGER NOT NULL,
                    start_time TEXT NOT NULL, recovered_at REAL NOT NULL);
CREATE TABLE events(
                    handle TEXT NOT NULL, sequence INTEGER NOT NULL, kind TEXT NOT NULL,
                    payload TEXT NOT NULL, size INTEGER NOT NULL,
                    PRIMARY KEY(handle,sequence));
CREATE TABLE handles(
                    id TEXT PRIMARY KEY, signature TEXT NOT NULL, pid INTEGER NOT NULL,
                    sequence INTEGER NOT NULL DEFAULT 0, acknowledged INTEGER NOT NULL DEFAULT 0,
                    rpc_sequence INTEGER NOT NULL DEFAULT 0, init_result TEXT, created REAL NOT NULL);
CREATE TABLE operations(
                    handle TEXT NOT NULL, operation_id TEXT NOT NULL, digest TEXT NOT NULL,
                    native_id INTEGER, accepted REAL NOT NULL,
                    PRIMARY KEY(handle,operation_id));
CREATE TABLE recovery_events(
                    id INTEGER PRIMARY KEY AUTOINCREMENT, at REAL NOT NULL,
                    handle TEXT NOT NULL, pid INTEGER NOT NULL, start_time TEXT NOT NULL,
                    outcome TEXT NOT NULL, detail TEXT);
CREATE TABLE supervisor_state(
                    key TEXT PRIMARY KEY, value TEXT NOT NULL);
INSERT INTO "child_identities" VALUES('fixture',123,123,'fixture-start');
INSERT INTO "events" VALUES('fixture',1,'stdout','null',4);
INSERT INTO "handles" VALUES('fixture','signature',123,1,0,0,NULL,10.0);
INSERT INTO "operations" VALUES('fixture','monitor:legacy','digest',1,11.0);
INSERT INTO "supervisor_state" VALUES('journalId','3301198e6b9444a89072cb2f372b5533');
COMMIT;
