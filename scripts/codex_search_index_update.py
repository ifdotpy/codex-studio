"""Apply only the search row-address fix to a compatible running Studio runtime.

This operation runs inside the existing process, under its normal writer lock.
It does not reload modules, replace account connections, or replay requests.
"""
import ast
from pathlib import Path
from types import MethodType

_LEGACY = '''def index_item(self, db, key, agent, kind, body):
    db.execute("DELETE FROM runtime_search WHERE id=?", (key,))
    db.execute(
        "INSERT INTO runtime_search(id,agent,kind,body) VALUES (?,?,?,?)",
        (key, agent, kind, body),
    )
    db.execute("INSERT OR IGNORE INTO runtime_search_indexed VALUES (?)", (key,))
'''


def signature(function):
    code = function.__code__
    return code.co_code, code.co_consts, code.co_names, code.co_varnames


def apply(runtime):
    source = Path(__file__).with_name('codex_work.py')
    tree = ast.parse(source.read_text())
    cls = next(node for node in tree.body if isinstance(node, ast.ClassDef) and node.name == 'WorkMixin')
    methods = [node for node in cls.body if isinstance(node, ast.FunctionDef)
               and node.name in {'setup_search_rows', 'index_item'}]
    assert len(methods) == 2, 'Expected exactly two search methods'
    scope = {}
    exec(compile(ast.Module(body=methods, type_ignores=[]), str(source), 'exec'), scope)
    legacy = {}
    exec(_LEGACY, legacy)
    if not runtime.lock.acquire(timeout=10):
        raise RuntimeError('Runtime remains busy; no search update applied')
    try:
        current = runtime.index_item.__func__
        if signature(current) == signature(scope['index_item']):
            return {'status': 'already_applied'}
        if signature(current) != signature(legacy['index_item']):
            raise RuntimeError('Unknown live search implementation; no update applied')
        with runtime.db() as db:
            scope['setup_search_rows'](runtime, db)
            rows = db.execute('SELECT count(*) FROM runtime_search_rows').fetchone()[0]
        # All normal index writers hold this same lock. The table is committed
        # before the replacement can execute, and accepted operations stay queued.
        runtime.index_item = MethodType(scope['index_item'], runtime)
        return {'status': 'applied', 'mappedRows': rows}
    finally:
        runtime.lock.release()
