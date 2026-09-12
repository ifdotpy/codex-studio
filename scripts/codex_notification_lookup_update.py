"""Apply the indexed native notification lookup without stopping live work."""
from pathlib import Path
import sys
from types import MethodType

from codex_active_task_update import compile_function, signature

BASE_COMMIT = '516ab7a'
EXPECTED = (
    '794ee6a4dcbd920bff1fe41bf4fd77b06b0b7a95fbb87ee5dc1caa29a291ae73',
    '89450ccbe330e21529ca2459c6000279b794206ac410351f85f0ec6d4d0072e4',
)
INDEX_NAME = 'runtime_agent_native_scope'
INDEX_SQL = """CREATE INDEX runtime_agent_native_scope ON runtime_agents(
                    json_extract(record,'$.threadId'),
                    CASE WHEN json_type(record,'$.accountKey') IS NULL THEN 'default'
                         ELSE json_extract(record,'$.accountKey') END)"""


def apply(runtime):
    import codex_runtime
    if sys.version_info[:2] != (3, 14) or type(runtime) is not codex_runtime.Runtime:
        raise RuntimeError('Unknown runtime for the notification lookup update')
    source = Path(__file__).resolve().with_name('codex_runtime.py')
    if Path(codex_runtime.__file__).resolve() != source:
        raise RuntimeError('Unexpected live notification module location')
    desired = compile_function(source.read_text(), 'Runtime', 'notification', vars(codex_runtime), str(source))
    if signature(desired) != EXPECTED[1]:
        raise RuntimeError('Unreviewed notification lookup replacement')
    if not runtime.lock.acquire(timeout=10):
        raise RuntimeError('Runtime remains busy; no notification lookup update applied')
    try:
        if runtime.closed:
            raise RuntimeError('Runtime is closed; no notification lookup update applied')
        live = vars(codex_runtime.Runtime).get('notification')
        bound = getattr(runtime, 'notification', None)
        if ('notification' in vars(runtime) or not isinstance(bound, MethodType)
                or bound.__self__ is not runtime or bound.__func__ is not live):
            raise RuntimeError('Unexpected notification method override')
        if (signature(live) not in EXPECTED or live.__globals__ is not vars(codex_runtime)
                or live.__code__.co_freevars != desired.__code__.co_freevars):
            raise RuntimeError('Unknown live notification method')
        previous = live.__code__
        already = signature(live) == EXPECTED[1]
        try:
            with runtime.db() as db:
                db.execute('BEGIN IMMEDIATE')
                row = db.execute('SELECT type,tbl_name,sql FROM sqlite_master WHERE name=?',
                                 (INDEX_NAME,)).fetchone()
                if row is not None and (row[0] != 'index' or row[1] != 'runtime_agents'
                        or ' '.join((row[2] or '').split()) != ' '.join(INDEX_SQL.split())):
                    raise RuntimeError('Unknown existing native notification index')
                if row is None:
                    db.execute(INDEX_SQL)
                if not already:
                    live.__code__ = desired.__code__
        except BaseException:
            if live.__code__ is not previous:
                live.__code__ = previous
            raise
        return {'status': 'already_applied' if already and row is not None else 'applied',
                'baseCommit': BASE_COMMIT, 'methods': ['notification'],
                'indexCreated': row is None}
    finally:
        runtime.lock.release()
