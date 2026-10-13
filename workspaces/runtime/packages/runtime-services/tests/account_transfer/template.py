"""Prepare one disposable Python-written schema template without a Runtime."""
import json
from pathlib import Path
import sqlite3
import sys
sys.path.insert(0,str(Path(__file__).resolve().parents[1]/'turn_lifecycle'))
from oracle_guard import install_guard
install_guard(block_processes=True)
ROOT=Path(__file__).resolve().parents[6]
sys.path.insert(0,str(ROOT/'workspaces/runtime/apps/server/src'))
from codex_account_transfer import AccountTransfers
from threading import RLock

root=Path(sys.argv[1]); root.mkdir(parents=True,exist_ok=True)
schema=json.loads((ROOT/'workspaces/runtime/packages/runtime-store/tests/fixtures/live-state-schema.json').read_text())
for database in ('canvas.sqlite3','analytics.sqlite3'):
    db=sqlite3.connect(root/database)
    db.execute('PRAGMA journal_mode=WAL')
    db.execute('PRAGMA synchronous=FULL')
    db.execute('BEGIN')
    objects=[o for o in schema[database]['sqlite_master'] if o.get('sql') and o['name'] not in {
        'runtime_search_next_config','runtime_search_next_data','runtime_search_next_docsize','runtime_search_next_idx'}]
    objects.sort(key=lambda o:({'table':0,'index':1,'trigger':2,'view':3}.get(o['type'],4),o['name']))
    for obj in objects:
        if obj['type']!='trigger': db.execute(obj['sql'])
    if database=='canvas.sqlite3':
        agent=dict(id='lead',rootId='lead',epoch=3,status='idle',isLead=True,autoWake=False,accountKey='source',threadId='source-thread',cwd=str(root),name='Lead',model='gpt-6-astra',accountTransferId='41e0b552-0b0c-4c5c-9c37-2e7483cd05c9')
        operation=dict(id=agent['accountTransferId'],leadId='lead',targetAccountKey='target',status='pending',scope='team',created=1.0,members={'lead':dict(phase='waiting',sourceAccountKey='source',sourceThreadId='source-thread',name='Lead',provider='codex',lazy=False)},requests={})
        class Ports:
            lock=RLock()
            def db(self): return db
            def agent(self,key,db): return json.loads(db.execute('SELECT record FROM runtime_agents WHERE id=?',(key,)).fetchone()[0])
            def put(self,db,table,record): db.execute('INSERT OR REPLACE INTO runtime_'+table+'(id,record) VALUES(?,?)',(record['id'],json.dumps(record)))
        ports=Ports(); ports.put(db,'agents',agent)
        transfers=object.__new__(AccountTransfers); transfers.rt=ports
        transfers.save(db,operation)
    for obj in objects:
        if obj['type']=='trigger': db.execute(obj['sql'])
    db.commit(); db.close()
print('Python transfer template prepared')
