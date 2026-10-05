"""Create the reviewed turn index once, within a bounded server transaction."""
import hashlib
import json
import os
from pathlib import Path
import re
import sys
import time

from codex_source import signature, source_function

SOURCES = ('7e2f7a03acdf9c6a1c8876f022b70c14821ef37a7119f33ed910845e18ba7450',
           '9b7fcf91543d2e469bc8581ec8d4c1cd868afe64a4b035a15fe0f5fd1beac7e7')
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


def _pending_error(message, attempt):
    keys = ('status', 'error', 'message', 'sqlite_errorname', 'sqlite_errorcode',
            'startedAt', 'failedAt', 'elapsedSeconds')
    details = {key: attempt[key] for key in keys if key in attempt}
    if ('elapsedSeconds' not in details and 'startedAt' in details and 'failedAt' in details):
        details['elapsedSeconds'] = max(0, details['failedAt'] - details['startedAt'])
    return RuntimeError(message + ': ' + json.dumps(details, sort_keys=True))


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
        wrapped = getattr(module.Runtime.db, '__wrapped__', None)
        closure = getattr(module.Runtime.db, '__closure__', None) or ()
        if ('db' in vars(runtime) or getattr(current, '__func__', None) is not module.Runtime.db
                or signature(module.Runtime.db) != DB_WRAPPER
                or len(closure) != 1 or closure[0].cell_contents is not wrapped
                or signature(wrapped) != DB_FUNCTION
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
                    raise _pending_error('The turn-scope index remains pending manual review',
                                         vars(runtime)[ATTEMPT_KEY])
                started_clock = _clock()
                attempt = {'status': 'started', 'database': str(database), 'startedAt': time.time()}
                runtime.__dict__[ATTEMPT_KEY] = attempt
                deadline = started_clock + BUDGET_SECONDS
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
                attempt.update(status='failed', error=type(error).__name__, message=str(error),
                               failedAt=time.time(), elapsedSeconds=max(0, _clock() - started_clock))
                for key in ('sqlite_errorname', 'sqlite_errorcode'):
                    if hasattr(error, key):
                        attempt[key] = getattr(error, key)
                raise _pending_error('The turn-scope index attempt failed; pending manual review',
                                     attempt) from error
            raise
