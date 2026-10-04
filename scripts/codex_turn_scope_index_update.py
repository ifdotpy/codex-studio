"""Create the reviewed turn index once, within a bounded server transaction."""
import hashlib
import os
from pathlib import Path
import re
import sys
import time

from codex_source import signature, source_function

SOURCES = ('ec05e1d6b43d80a36a0548db1cd6baf185af8ca4a8f048998425f6c3d4312d51',
           '0855e2e41b6652a4a7c3f8e472855d4b171c920b0fea3ad91cc961b3dfea51d2')
DB_FUNCTION = '2a635207f8e7fe8ad50829d3c3157afda018eac930820e5ef3642f3fc565113c'
DB_WRAPPER = 'aabffbe653bec8c5208b6b45cc81fb4a8077f9fbd383da54f5214aa6674c02c2'
INDEX = 'runtime_item_turn_scope'
SQL = "CREATE INDEX runtime_item_turn_scope ON runtime_items(agent, json_extract(record,'$.turnId'), created)"
ATTEMPT_KEY = '_turn_scope_index_attempt'
BUDGET_SECONDS = 12
_clock = time.monotonic


def _sql_identity(sql):
    # Preserve quoted paths and identifier case while ignoring layout changes.
    return tuple(re.findall(r"'(?:''|[^'])*'|\"(?:\"\"|[^\"])*\"|\S", sql or ''))


def _existing_index(db):
    row = db.execute('SELECT type,tbl_name,sql FROM sqlite_master WHERE name=?', (INDEX,)).fetchone()
    if row is None:
        return False
    if (row[0] != 'index' or row[1] != 'runtime_items'
            or _sql_identity(row[2]) != _sql_identity(SQL)):
        raise RuntimeError('The turn-scope index schema differs')
    return True


def apply(runtime):
    scripts = Path(__file__).resolve().parent
    module = sys.modules.get('codex_runtime')
    path = scripts / 'codex_runtime.py'
    if (module is None or not isinstance(runtime, module.Runtime)
            or Path(module.__file__).resolve() != path):
        raise RuntimeError('The running backend source identity differs')
    with runtime.lock:
        if runtime.closed:
            raise RuntimeError('The running backend is closed')
        raw = path.read_bytes()
        if hashlib.sha256(raw).hexdigest() not in SOURCES:
            raise RuntimeError('The reviewed runtime source differs')
        reviewed, _ = source_function(raw, ['Runtime', 'db'], vars(module), str(path),
                                     allow_contextmanager=True)
        current = runtime.db
        if ('db' in vars(runtime) or getattr(current, '__func__', None) is not module.Runtime.db
                or signature(module.Runtime.db) != DB_WRAPPER
                or signature(getattr(module.Runtime.db, '__wrapped__', None)) != DB_FUNCTION
                or signature(reviewed) != DB_FUNCTION):
            raise RuntimeError('The running runtime database guard differs')
        root = Path(runtime.root).resolve()
        database = Path(runtime.db_path).resolve()
        lease = getattr(runtime, 'lease', None)
        if (database != root / 'canvas.sqlite3' or lease is None or lease.closed
                or Path(lease.name).resolve() != root / 'runtime.lock'):
            raise RuntimeError('The running state identity differs')
        held = os.fstat(lease.fileno())
        owned = (root / 'runtime.lock').stat()
        if (held.st_dev, held.st_ino) != (owned.st_dev, owned.st_ino):
            raise RuntimeError('The running state lease identity differs')
        attempt = None
        try:
            with runtime.db(busy_timeout=50) as db:
                main = next((row[2] for row in db.execute('PRAGMA database_list')
                             if row[1] == 'main'), None)
                if not main or Path(main).resolve() != database:
                    raise RuntimeError('The running database identity differs')
                if _existing_index(db):
                    return {'status': 'already_applied'}
                if ATTEMPT_KEY in vars(runtime):
                    raise RuntimeError('The turn-scope index remains pending manual review')
                attempt = {'status': 'started', 'database': str(database), 'startedAt': time.time()}
                runtime.__dict__[ATTEMPT_KEY] = attempt
                deadline = _clock() + BUDGET_SECONDS
                db.set_progress_handler(lambda: int(runtime.closed or _clock() >= deadline), 1000)
                try:
                    db.execute('BEGIN IMMEDIATE')
                    existing = _existing_index(db)
                    if not existing:
                        db.execute(SQL)
                    if runtime.closed or _clock() >= deadline:
                        raise TimeoutError('The turn-scope index deadline expired')
                    if not _existing_index(db):
                        raise RuntimeError('The turn-scope index was not created')
                    db.commit()
                finally:
                    db.set_progress_handler(None, 0)
            attempt.update(status='applied', appliedAt=time.time())
            return {'status': 'already_applied' if existing else 'applied'}
        except Exception as error:
            if attempt is not None:
                attempt.update(status='failed', error=type(error).__name__, failedAt=time.time())
                raise RuntimeError('The turn-scope index attempt failed; pending manual review') from error
            raise
