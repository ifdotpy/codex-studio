CREATE TABLE IF NOT EXISTS handles(
                    id TEXT PRIMARY KEY, signature TEXT NOT NULL, pid INTEGER NOT NULL,
                    sequence INTEGER NOT NULL DEFAULT 0, acknowledged INTEGER NOT NULL DEFAULT 0,
                    rpc_sequence INTEGER NOT NULL DEFAULT 0, init_result TEXT, created REAL NOT NULL,
                    closed_at REAL, closed_reason TEXT, generation INTEGER NOT NULL DEFAULT 0);
                CREATE TABLE IF NOT EXISTS operations(
                    handle TEXT NOT NULL, operation_id TEXT NOT NULL, digest TEXT NOT NULL,
                    native_id INTEGER, accepted REAL NOT NULL, generation INTEGER,
                    response TEXT, response_sequence INTEGER,
                    PRIMARY KEY(handle,operation_id));
                CREATE TABLE IF NOT EXISTS events(
                    handle TEXT NOT NULL, sequence INTEGER NOT NULL, kind TEXT NOT NULL,
                    payload TEXT NOT NULL, size INTEGER NOT NULL, generation INTEGER NOT NULL DEFAULT 1,
                    PRIMARY KEY(handle,sequence));
                CREATE TABLE IF NOT EXISTS child_identities(
                    handle TEXT PRIMARY KEY, pid INTEGER NOT NULL, pgid INTEGER NOT NULL,
                    start_time TEXT NOT NULL, job_name TEXT NOT NULL DEFAULT '',
                    job_kill_on_close INTEGER NOT NULL DEFAULT 0);
                CREATE TABLE IF NOT EXISTS recovery_events(
                    id INTEGER PRIMARY KEY AUTOINCREMENT, at REAL NOT NULL,
                    handle TEXT NOT NULL, pid INTEGER NOT NULL, start_time TEXT NOT NULL,
                    outcome TEXT NOT NULL, detail TEXT);
                CREATE TABLE IF NOT EXISTS supervisor_state(
                    key TEXT PRIMARY KEY, value TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS degraded_handles(
                    handle TEXT PRIMARY KEY, prior_pid INTEGER NOT NULL,
                    start_time TEXT NOT NULL, recovered_at REAL NOT NULL);
