#!/usr/bin/env python3
"""Read-only, indexed, bounded analytics collection. Emit aggregate metadata only."""
import argparse
from collections import Counter, defaultdict
from datetime import datetime, timezone
import hashlib
import json
import math
from pathlib import Path
import sqlite3
import statistics
import subprocess
import time

FIELDS = ('inputTokens', 'cachedInputTokens', 'cacheWriteInputTokens', 'outputTokens', 'reasoningOutputTokens', 'totalTokens')

def total(values):
    values = [v for v in values if isinstance(v, (int, float)) and not isinstance(v, bool)]
    return sum(values) if values else None

def distribution(values):
    values = sorted(v for v in values if isinstance(v, (int, float)))
    return {'count': len(values), 'sum': total(values), 'p50': statistics.median(values) if values else None,
            'p95': values[math.ceil(len(values) * .95) - 1] if values else None, 'max': max(values) if values else None}

def usage_summary(rows):
    tokens = {f: total(r['delta'].get(f) for r in rows) for f in FIELDS}
    valid = [r for r in rows if isinstance(r['delta'].get('inputTokens'), (int, float)) and isinstance(r['delta'].get('cachedInputTokens'), (int, float)) and 0 <= r['delta']['cachedInputTokens'] <= r['delta']['inputTokens']]
    return {'samples': len(rows), 'tokens': tokens,
            'fieldObservations': {f: sum(r['delta'].get(f) is not None for r in rows) for f in FIELDS},
            'cachePairSamples': len(valid), 'cacheInputShare': tokens['cachedInputTokens'] / tokens['inputTokens'] if len(valid) == len(rows) and tokens['inputTokens'] else None,
            'uncachedInputTokens': total(r['delta']['inputTokens'] - r['delta']['cachedInputTokens'] for r in valid),
            'perSampleInputTokens': distribution(r['delta'].get('inputTokens') for r in rows),
            'baselineMissingSamples': sum(bool(r['baselineMissing']) for r in rows)}

def collect(database, root, end):
    start = end - 86400
    c = sqlite3.connect(f'file:{database}?mode=ro', uri=True, timeout=2)
    c.row_factory = sqlite3.Row
    c.execute('PRAGMA query_only=ON')
    c.execute('PRAGMA busy_timeout=2000')
    deadline = [time.monotonic() + 20]
    c.set_progress_handler(lambda: time.monotonic() > deadline[0], 10000)
    def query(sql, args=()):
        deadline[0] = time.monotonic() + 20
        return c.execute(sql, args).fetchall()
    began = time.time()
    c.execute('BEGIN')
    agents = [r[0] for r in query('SELECT id FROM analytics_agents')]
    usage, items, turns, events = [], [], [], []
    for agent in agents:
        for r in query("SELECT agent,thread,turn,at,json_extract(record,'$.responseId') responseId,json_extract(record,'$.source') source,json_extract(record,'$.counterDomain') counterDomain,json_extract(record,'$.model') model,json_extract(record,'$.delta') delta,json_extract(record,'$.baselineMissing') baselineMissing FROM analytics_usage INDEXED BY analytics_usage_scope WHERE agent=? AND at>=? AND at<?", (agent, start, end)):
            r = dict(r); r['delta'] = json.loads(r['delta']); usage.append(r)
        for r in query("SELECT agent,thread,turn,type,name,is_tool,json_extract(record,'$.payloadBoundary') boundary,json_extract(record,'$.status') status,json_extract(record,'$.input.bytes') inputBytes,json_extract(record,'$.output.bytes') outputBytes,json_extract(record,'$.durationMs') durationMs,json_extract(record,'$.finishedAt') finishedAt,json_extract(record,'$.payloadTruncated') truncated,json_extract(record,'$.coverage') coverage FROM analytics_items INDEXED BY analytics_items_scope WHERE agent=? AND at>=? AND at<?", (agent, start, end)):
            items.append(dict(r))
        turns += [dict(r) for r in query("SELECT agent,json_extract(record,'$.status') status,json_extract(record,'$.durationMs') durationMs FROM analytics_turns INDEXED BY analytics_turns_scope WHERE agent=? AND at>=? AND at<?", (agent, start, end))]
    authoritative = set()
    for agent, thread in {(r['agent'],r['thread']) for r in usage}:
        for r in query("SELECT DISTINCT turn FROM analytics_usage INDEXED BY analytics_usage_response WHERE agent=? AND thread IS ? AND json_extract(record,'$.responseId') IS NOT NULL", (agent,thread)):
            authoritative.add((agent,thread,r[0]))
    # Event groups concern events created within the window, not every event in a turn's lifetime.
    event_agents = set(agents) | {r[0] for r in query('SELECT id FROM runtime_agents')}
    for agent in event_agents:
        rows = query("SELECT agent,turn_id,kind,length(CAST(text AS BLOB)) bytes,CASE WHEN json_valid(text) THEN json_extract(text,'$.importance') END importance,CASE WHEN json_valid(text) THEN json_extract(text,'$.progress_key') IS NOT NULL AND json_type(text,'$.progress_version')='integer' END versionedProgress,CASE WHEN json_valid(text) THEN json_extract(text,'$.exitCode') END exitCode,CASE WHEN json_valid(text) THEN length(CAST(json_extract(text,'$.tail') AS BLOB)) END tailBytes FROM runtime_events INDEXED BY runtime_event_queue WHERE status='delivered' AND agent=? AND created>=? AND created<?", (agent,start,end))
        events += [dict(r) for r in rows]
    meta = {r[0]: json.loads(r[1]) for r in query('SELECT key,value FROM analytics_meta')}
    history = Counter()
    if query("SELECT 1 FROM sqlite_master WHERE name='analytics_history'"):
        history = Counter(r[0] for r in query("SELECT json_extract(record,'$.status') FROM analytics_history"))
    c.rollback(); c.close()
    selected = [r for r in usage if r['responseId'] or (r['agent'],r['thread'],r['turn']) not in authoritative]
    exact = [r for r in selected if r['responseId']]
    legacy = [r for r in selected if not r['responseId']]
    tools = defaultdict(list)
    for r in items:
        if r['is_tool']: tools[(r['boundary'] or 'protocol',r['name'],r['type'])].append(r)
    tool_groups = []
    for (boundary,name,kind), rows in tools.items():
        tool_groups.append({'boundary':boundary,'name':name,'type':kind,'calls':len(rows),'failed':sum(r['status']=='failed' for r in rows),'inputBytes':total(r['inputBytes'] for r in rows),'outputBytes':total(r['outputBytes'] for r in rows),'outputDistribution':distribution(r['outputBytes'] for r in rows),'durationMs':distribution(r['durationMs'] for r in rows),'truncated':sum(bool(r['truncated']) for r in rows),'above64kCount':sum((r['outputBytes'] or 0)>64000 for r in rows),'above64kBytes':sum(r['outputBytes'] for r in rows if (r['outputBytes'] or 0)>64000),'above16KiBCount':sum((r['outputBytes'] or 0)>16384 for r in rows),'above16KiBBytes':sum(r['outputBytes'] for r in rows if (r['outputBytes'] or 0)>16384),'above64KiBCount':sum((r['outputBytes'] or 0)>65536 for r in rows),'above64KiBBytes':sum(r['outputBytes'] for r in rows if (r['outputBytes'] or 0)>65536)})
    tool_groups.sort(key=lambda r:r['outputBytes'] or 0,reverse=True)
    event_turns = defaultdict(list)
    for r in events:
        if r['turn_id']: event_turns[(r['agent'],r['turn_id'])].append(r)
    event_sets = Counter(','.join(sorted({r['kind'] for r in rows})) for rows in event_turns.values())
    monitor_only = [rows for rows in event_turns.values() if {r['kind'] for r in rows} == {'monitor_exit'}]
    successful_only = [rows for rows in monitor_only if all(r['exitCode'] == 0 for r in rows)]
    successful_keys = {(rows[0]['agent'],rows[0]['turn_id']) for rows in successful_only}
    successful_usage = [r for r in selected if (r['agent'],r['turn']) in successful_keys]
    event_usage = defaultdict(list)
    for r in selected:
        key = (r['agent'],r['turn'])
        if key in event_turns:
            event_usage[','.join(sorted({e['kind'] for e in event_turns[key]}))].append(r)
    commands = [r['outputBytes'] for r in items if r['name']=='commandExecution' and r['boundary'] != 'model' and r['outputBytes'] is not None]
    native_compactions = {(r['agent'],r['turn']) for r in items if r['type']=='contextCompaction' and r['finishedAt'] is not None}
    snapshots = [r for r in items if r['type']=='compactionSnapshot' and (r['agent'],r['turn']) not in native_compactions]
    models = defaultdict(list)
    for r in selected: models[r['model']].append(r)
    paths = ['scripts/codex_analytics.py','scripts/codex_analytics_history.py','scripts/codex_efficiency.py','scripts/codex_runtime.py','scripts/codex_work.py','docs/research/2026-09-12-harness-metrics.py']
    hashes = {p:hashlib.sha256((root/p).read_bytes()).hexdigest() for p in paths if (root/p).exists()}
    installed = Path.home()/'Applications/Codex Studio.app/Contents/Resources/workspace'
    return {'window':{'from':start,'to':end,'fromUtc':datetime.fromtimestamp(start,timezone.utc).isoformat(),'toUtc':datetime.fromtimestamp(end,timezone.utc).isoformat(),'durationSeconds':86400,'interval':'[from,to)'},
            'collection':{'snapshotStartedAt':began,'elapsedSeconds':time.time()-began,'sqliteMode':'ro; query_only=ON; single read transaction','queryDeadlineSeconds':20,'registryAgents':len(agents),'head':subprocess.check_output(['git','rev-parse','HEAD'],cwd=root,text=True).strip(),'sha256':hashes,'installedSha256':{p:hashlib.sha256((installed/p).read_bytes()).hexdigest() for p in paths if (installed/p).exists()}},
            'summary':{'payloadBoundaries':{boundary:{'calls':sum(r['calls'] for r in tool_groups if r['boundary']==boundary),'outputBytes':total(r['outputBytes'] for r in tool_groups if r['boundary']==boundary)} for boundary in ('model','protocol')},'activeAgents':len({r['agent'] for r in usage+items+turns}),'turns':len(turns),'turnStatus':dict(Counter(r['status'] for r in turns)),'turnDurationMs':distribution(r['durationMs'] for r in turns),'tokens':usage_summary(selected),'compactions':sum(r['type']=='contextCompaction' and r['finishedAt'] is not None for r in items)+len(snapshots)},
            'coverage':{'rawUsageRows':len(usage),'exactResponseSamples':len(exact),'legacyUsageSamples':len(legacy),'provisionalExcludedSamples':len(usage)-len(selected),'usageSource':dict(Counter(r['source'] for r in selected)),'counterDomain':dict(Counter(r['counterDomain'] for r in selected)),'payloadCoverage':dict(Counter(r['coverage'] for r in items)),'analyticsMeta':meta,'historyStatus':dict(history)},
            'exactResponseUsage':usage_summary(exact),'legacyUsage':usage_summary(legacy),'byModel':{str(k):usage_summary(v) for k,v in models.items()},'tools':tool_groups,
            'commands':{**distribution(commands),'above16kCount':sum(n>16000 for n in commands),'above64kCount':sum(n>64000 for n in commands),'above64kBytes':sum(n for n in commands if n>64000)},
            'events':{'deliveredByKind':dict(Counter(r['kind'] for r in events)),'bytesByKind':{k:sum(r['bytes'] for r in events if r['kind']==k) for k in {r['kind'] for r in events}},'missingTurnId':sum(not r['turn_id'] for r in events),'messageImportance':dict(Counter(r['importance'] or 'missing' for r in events if r['kind']=='agent_message')),'versionedProgress':sum(bool(r['versionedProgress']) and r['importance']=='progress' for r in events),'turnsByEventSet':dict(event_sets),'successMonitorOnlyTurns':len(successful_only),'successMonitorOnlyUsage':{'all':usage_summary(successful_usage),'exact':usage_summary([r for r in successful_usage if r['responseId']]),'legacy':usage_summary([r for r in successful_usage if not r['responseId']])},'successMonitorOnlyTurnsWithUsage':len({(r['agent'],r['turn']) for r in successful_usage}),'successMonitorEvents':sum(r['kind']=='monitor_exit' and r['exitCode']==0 for r in events),'monitorTailBytes':total(r['tailBytes'] for r in events if r['kind']=='monitor_exit'),'usageByEventSet':{k:usage_summary(v) for k,v in event_usage.items()}}}

if __name__ == '__main__':
    parser=argparse.ArgumentParser()
    parser.add_argument('--database', type=Path, default=Path.home()/'.local/state/codex-agents/canvas.sqlite3')
    parser.add_argument('--end',type=float,default=time.time())
    parser.add_argument('--output',type=Path,required=True)
    args=parser.parse_args()
    result=collect(args.database,Path(__file__).resolve().parents[2],args.end)
    args.output.write_text(json.dumps(result,indent=2,sort_keys=True)+'\n')
    print(json.dumps({'output':str(args.output),'window':result['window'],'elapsedSeconds':result['collection']['elapsedSeconds'],'usageRows':result['coverage']['rawUsageRows'],'exactResponses':result['coverage']['exactResponseSamples'],'toolGroups':len(result['tools'])}))
