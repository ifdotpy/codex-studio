"""Small model views over durable Studio records. UI storage remains complete."""
import base64
import hashlib
import json
import time


def packed(value):
    return json.dumps(value, ensure_ascii=False, separators=(',', ':'))


def clip(text, size):
    return str(text).encode('utf-8')[:size].decode('utf-8', errors='ignore')


def digest(value):
    return hashlib.sha256(packed(value).encode()).hexdigest()[:24]


def efficiency_tools(tool, text):
    return [tool('orchestration_read',
        'Read a full saved tool response by output_ref. Offsets are Unicode characters; pages are bounded. '
        'Use contains to locate relevant output. Same-agent records only. Never rerun a mutation to recover its result.',
        {'output_ref': text, 'offset': {'type': 'integer', 'minimum': 0},
         'contains': text}, ['output_ref']),
        tool('orchestration_context',
        'Read versioned Studio guidance or team context on demand. Panel set still requires visual validation. '
        'Use background guidance for script-driven status with no model polling.',
        {'topic': {'type': 'string', 'enum': ['panel', 'background', 'tools', 'plan', 'complaints', 'profiles', 'monitor']},
         'id': text}, ['topic'])]


class EfficiencyMixin:
    def model_page(self, rows, args, scope):
        limit = args.get('limit', 20)
        if type(limit) is not int or not 1 <= limit <= 50:
            raise ValueError('limit must be 1 to 50')
        revision = digest([scope, rows])
        offset = 0
        if args.get('cursor'):
            try:
                cursor = json.loads(base64.urlsafe_b64decode(args['cursor']))
                offset = cursor['offset']
                if type(offset) is not int or offset < 0 or cursor['revision'] != revision:
                    raise ValueError()
            except (ValueError, TypeError, KeyError):
                raise ValueError('List changed or cursor is invalid. Read the first page again.') from None
        page = rows[offset:offset + limit]
        next_offset = offset + len(page)
        cursor = base64.urlsafe_b64encode(packed({'offset': next_offset, 'revision': revision}).encode()).decode() if next_offset < len(rows) else None
        return {'apiVersion': 2, 'items': page, 'total': len(rows), 'revision': revision, 'nextCursor': cursor}

    def model_work(self, actor_id, args, key=None, epoch=None):
        action = args.get('action', 'list')
        if action not in {'list', 'get', 'history'}:
            full = self.work_action(actor_id, args, key, actor=actor_id, epoch=epoch)
            return {**self.task_brief(full), 'requestId': key,
                    'detail': {'tool': 'orchestration_task', 'action': 'get', 'task_id': full['id']}}
        with self.lock, self.db() as db:
            actor = self.checked_actor(db, actor_id, actor_id)
            works = [w for w in self.records(db, 'work') if w['rootId'] == actor['rootId']]
            if action == 'list':
                rows = [self.task_brief(self.work_view(w, works)) for w in works
                        if (not args.get('owner') or w.get('owner') == args['owner'])
                        and (not args.get('state') or w.get('status') == args['state'])]
                rows.sort(key=lambda w: w['id'])
                return self.model_page(rows, args, [actor['rootId'], args.get('owner'), args.get('state')])
            task = next((w for w in works if w['id'] == args.get('task_id')), None)
            if task is None:
                raise ValueError('Unknown task in this team')
            if action == 'history':
                rows = sorted([{'kind': 'result', **r} for r in task['results']]
                              + [{'kind': 'decision', **r} for r in task['decisions']], key=lambda r: r.get('created', 0))
                return self.model_page(rows, args, [actor['rootId'], task['id'], 'history'])
            return {**self.task_brief(self.work_view(task, works)), 'description': task['description'],
                    'dependencies': task['dependencies'], 'latestResult': task['results'][-1] if task['results'] else None,
                    'latestDecision': task['decisions'][-1] if task['decisions'] else None,
                    'history': {'results': len(task['results']), 'decisions': len(task['decisions'])}}

    @staticmethod
    def task_brief(task):
        return {k: task.get(k) for k in ('id', 'title', 'owner', 'status', 'version', 'blockedBy')}

    def model_directory(self, actor_id, name, args):
        with self.lock, self.db() as db:
            actor = self.checked_actor(db, actor_id, actor_id)
            agents = [a for a in self.records(db, 'agents') if not a.get('deletedAt')]
            if name == 'orchestration_peers':
                scope = args.get('scope', 'team')
                if scope not in {'team', 'all'}:
                    raise ValueError('Choose team or all')
                rows = [{k: a.get(k) for k in ('id', 'name', 'role', 'rootId', 'parentId', 'status')}
                        for a in agents if scope == 'all' or a['rootId'] == actor['rootId']]
                rows.sort(key=lambda a: a['id'])
                result = self.model_page(rows, args, [actor_id, scope])
                # No message bodies, monitor tails, profiles, or schemas here.
                result.update(self=actor_id, lead=actor['rootId'], parent=actor.get('parentId'),
                              rooms=[{k: r.get(k) for k in ('id', 'kind', 'members', 'rootId')}
                                     for r in self.chat_rooms(db, actor_id)])
                return result
            team = [a for a in agents if a['rootId'] == actor['rootId']]
            ids = {a['id'] for a in team}
            records = [{**{k: a.get(k) for k in ('id', 'name', 'status', 'inFlight', 'parentId')}, 'kind': 'agent',
                        'error': clip(a.get('error') or '', 600)} for a in team]
            records += [{**{k: m.get(k) for k in ('id', 'agent', 'status', 'exitCode')}, 'kind': 'monitor',
                         'error': clip(m.get('error') or '', 600)} for m in self.recent_monitors(db, actor["rootId"]) if m['agent'] in ids]
            records.sort(key=lambda r: (r['kind'], r['id']))
            defaults = self.worker_defaults(self.agent(actor['rootId'], db))
            revision = digest([records, defaults])
            db.execute('CREATE TABLE IF NOT EXISTS runtime_model_status (agent TEXT, revision TEXT, at REAL, record TEXT, PRIMARY KEY(agent,revision))')
            previous = db.execute('SELECT record FROM runtime_model_status WHERE agent=? AND revision=?',
                                  (actor_id, args.get('since_revision'))).fetchone()
            old = {r['kind'] + ':' + r['id']: r for r in json.loads(previous[0])} if previous else {}
            now = {r['kind'] + ':' + r['id']: r for r in records}
            changes = [r for k, r in now.items() if old.get(k) != r]
            db.execute('INSERT OR REPLACE INTO runtime_model_status VALUES (?,?,?,?)', (actor_id, revision, time.time(), packed(records)))
            db.execute('DELETE FROM runtime_model_status WHERE agent=? AND revision NOT IN (SELECT revision FROM runtime_model_status WHERE agent=? ORDER BY at DESC LIMIT 8)', (actor_id, actor_id))
            return {'apiVersion': 2, 'revision': revision, 'reset': previous is None, 'unchanged': previous is not None and args.get('since_revision') == revision,
                    'counts': {state: sum(a['status'] == state for a in team) for state in sorted({a['status'] for a in team})},
                    'changes': changes, 'removed': sorted(old.keys() - now.keys()),
                    'workerDefaults': defaults,
                    'help': 'Use orchestration_context for tools, profiles or monitor details. Use orchestration_peers for rooms.'}

    def model_tool_result(self, actor, key, result):
        content = result.get('contentItems', [])
        texts = [c for c in content if c.get('type') == 'inputText' and not c.get('text', '').startswith('[Time awareness]')]
        if len(packed(texts).encode()) <= 16000:
            return result
        # The full result was committed before this projection. Images and
        # immutable clock metadata stay intact, including panel set screenshots.
        raw = '\n'.join(c.get('text', '') for c in texts)
        preview = {'requestId': key, 'outputRef': key, 'success': result.get('success'),
                   'truncated': True, 'textBytes': len(raw.encode()), 'excerpt': clip(raw, 4000),
                   'read': {'tool': 'orchestration_read', 'output_ref': key},
                   'legacyRead': {'agent_id': 'workspace', 'text': packed({'tool': 'orchestration_read', 'arguments': {'output_ref': key}})}}
        with self.db() as db:
            receipt = self.tool_request(key, db)
            preview['outcome'] = receipt.get('outcome', 'unknown') if receipt else 'unknown'
        # Keep operation identities outside an excerpt, even for large spawn batches.
        try:
            value = json.loads(raw)
            if isinstance(value, dict):
                preview['summary'] = {k: value[k] for k in
                    ('id', 'requestId', 'stage', 'outcome', 'status', 'exitCode', 'version', 'nextCursor', 'revision', 'total', 'nextOffset', 'agentIds', 'agent_ids') if k in value}
                if isinstance(value.get('agents'), list):
                    preview['summary']['agents'] = [{k: a[k] for k in ('id', 'status') if k in a}
                                                    for a in value['agents'] if isinstance(a, dict)]
                if value.get('error'):
                    preview['summary']['error'] = clip(value['error'], 600)
                if value.get('status') == 'failed' or value.get('exitCode') not in (None, 0):
                    preview['failureTail'] = raw.encode()[-1200:].decode('utf-8', errors='ignore')
        except (ValueError, TypeError):
            pass
        if result.get('success') is False:
            preview['failureTail'] = raw.encode()[-1200:].decode('utf-8', errors='ignore')
        return {**result, 'contentItems': [{'type': 'inputText', 'text': packed(preview)}]
                + [c for c in content if c not in texts]}

    def model_read(self, actor_id, args):
        with self.lock, self.db() as db:
            actor = self.checked_actor(db, actor_id, actor_id)
            key = args.get('output_ref')
            receipt = self.tool_request(key, db)
            if receipt is None or receipt.get('agent') != actor['id']:
                raise ValueError('Output reference is not owned by this agent')
            row = db.execute('SELECT result FROM runtime_tool_results WHERE id=?', (key,)).fetchone()
            if row is None:
                raise ValueError('Result is not available yet. Do not repeat the operation.')
            result = json.loads(row[0])
            text = '\n'.join(c.get('text', '') for c in result.get('contentItems', []) if c.get('type') == 'inputText')
            offset = args.get('offset', 0)
            if type(offset) is not int or offset < 0 or offset > len(text):
                raise ValueError('offset must be within the saved text')
            if args.get('contains'):
                found = text.find(args['contains'], offset)
                if found < 0:
                    return {'outputRef': key, 'found': False, 'success': result.get('success'), 'outcome': receipt['outcome']}
                offset = max(offset, found - 200)
            excerpt = clip(text[offset:], 3000)
            end = offset + len(excerpt)
            return {'outputRef': key, 'success': result.get('success'), 'outcome': receipt['outcome'],
                    'offset': offset, 'nextOffset': end if end < len(text) else None,
                    'totalChars': len(text), 'text': excerpt}

    def reported_plan(self, db, root_id):
        row = db.execute('SELECT record FROM runtime_plans WHERE id=?', (root_id,)).fetchone()
        plan = json.loads(row[0]) if row else None
        if not plan or not isinstance(plan.get('native'), dict):
            return None
        return {'id': plan['id'], 'rootId': plan['rootId'],
                'steps': plan['native'].get('plan', []),
                'explanation': plan['native'].get('explanation') or ''}

    def model_context(self, actor_id, args):
        topic = args.get('topic')
        if topic == 'agent_manage':
            # Compatibility for existing threads whose native tool list is fixed.
            from codex_agent_management import manage_agent
            return manage_agent(self, actor_id, args)
        with self.lock, self.db() as db:
            actor = self.checked_actor(db, actor_id, actor_id)
            if topic == 'plan':
                value = self.reported_plan(db, actor['rootId'])
            elif topic == 'complaints':
                value = self.unanswered_complaints(db, actor['rootId'])
            elif topic == 'profiles':
                value = self.profiles()
            elif topic == 'monitor':
                row = db.execute('SELECT record FROM runtime_monitors WHERE id=?', (args.get('id'),)).fetchone()
                value = json.loads(row[0]) if row else None
                if value is None or self.agent(value['agent'], db)['rootId'] != actor['rootId']:
                    raise ValueError('Unknown monitor in this team')
            elif topic == 'panel':
                value = self.panel_guidance()
            elif topic == 'background':
                value = {'workflow': 'Set a structured panel once, start orchestration_panel_feed with an NDJSON-producing script, then finish the turn. Feed updates and exit status do not wake a model.',
                         'monitor': 'Use orchestration_monitor for commands needing a result. wake_on=failure suppresses a successful exit notification; errors still wake you.',
                         'polling': 'Do not poll status, read logs, or get unchanged panel screenshots. Use the event or feed.',
                         'frames': 'Each complete NDJSON line is the next value at state_path. Write a newline. Keep frames below 65536 bytes.'}
            elif topic == 'tools':
                value = self.tool_definitions(actor)
            else:
                raise ValueError('Unknown context topic')
        return {'topic': topic, 'version': digest(value), 'content': value}

    def model_turn_context(self, db, actor, event_id):
        epoch = [actor.get('threadId'), actor.get('compactions', 0)]
        row = db.execute("SELECT m.record FROM runtime_event_meta m JOIN runtime_events e ON e.id=m.id WHERE e.agent=? AND e.status='delivered' AND json_extract(m.record,'$.contextManifest') IS NOT NULL ORDER BY coalesce(json_extract(m.record,'$.contextManifest.sequence'),0) DESC, e.created DESC LIMIT 1", (actor['id'],)).fetchone()
        old = json.loads(row[0]).get('contextManifest', {}) if row else {}
        known = old.get('versions', {}) if old.get('epoch') == epoch else {}
        versions, blocks = {}, []
        role_skill = self.role_guidance(actor)
        versions['roleSkill'] = digest(role_skill)
        if known.get('roleSkill') != versions['roleSkill']:
            blocks.append(role_skill)
        plan = self.reported_plan(db, actor['rootId'])
        if plan is not None or 'plan' in known:
            version = digest(plan); versions['plan'] = version
            if known.get('plan') != version:
                content = packed(plan) if plan and (plan['steps'] or plan['explanation']) else 'The agent plan is empty. Discard the previous agent plan.'
                blocks.append('[Agent plan version ' + version + ']\n' + content)
        complaints = self.unanswered_complaints(db, actor['id'])
        changed = []
        for complaint in complaints:
            key = 'complaint:' + complaint['id']; version = digest([complaint['text'], complaint['status'], complaint.get('version')]); versions[key] = version
            if known.get(key) != version:
                changed.append(complaint)
        if changed:
            blocks.append('[Complaint messages requiring a response]\n' + self.complaint_message(db, changed))
        unchanged = [c for c in complaints if c not in changed]
        if unchanged:
            blocks.append('[Complaints still requiring a response] ' + ', '.join(c['id'] for c in unchanged)
                          + '. The full text was delivered earlier. Read orchestration_context topic=complaints if needed.')
        meta = db.execute('SELECT record FROM runtime_event_meta WHERE id=?', (event_id,)).fetchone()
        metadata = json.loads(meta[0]) if meta else {}
        metadata['contextManifest'] = {'epoch': epoch, 'versions': versions, 'sequence': old.get('sequence', 0) + 1}
        db.execute('INSERT OR REPLACE INTO runtime_event_meta VALUES (?,?)', (event_id, packed(metadata)))
        return ('\n\n' + '\n\n'.join(blocks)) if blocks else ''

    @staticmethod
    def progress_only(rows):
        if not rows:
            return False
        for row in rows:
            if row['kind'] != 'agent_message':
                return False
            try:
                value = json.loads(row['text'])
                if not isinstance(value, dict) or value.get('importance') != 'progress':
                    return False
            except (ValueError, TypeError):
                return False
        return True

    @staticmethod
    def progress_batch_ready(rows, now=None):
        # User, question, blocker, failure and unclassified events bypass delay.
        now = time.time() if now is None else now
        return not EfficiencyMixin.progress_only(rows) or now - min(r['created'] for r in rows) >= 1.0

    @staticmethod
    def model_event_text(rows):
        # Only versioned progress for the same sender, room and topic supersedes an
        # earlier update. Keep every original event and receipt in the database.
        latest, counts, versions, conflicts = {}, {}, {}, set()
        for row in rows:
            if row['kind'] != 'agent_message':
                continue
            try:
                value = json.loads(row['text'])
                if (not isinstance(value, dict) or value.get('importance') != 'progress'
                        or not isinstance(value.get('progress_key'), str)
                        or type(value.get('progress_version')) is not int):
                    continue
                key = (value['sender'], value['room'], value['progress_key'])
            except (ValueError, KeyError, TypeError):
                continue
            version = value['progress_version']
            seen = versions.setdefault(key, set())
            if version in seen:
                conflicts.add(key)  # Ambiguous revisions retain every message.
            seen.add(version)
            if key not in latest or version > latest[key][0]:
                latest[key] = (version, row['id'])
            counts[key] = counts.get(key, 0) + 1
        parts = []
        for row in rows:
            if row['kind'] == 'complaint':
                continue
            text = row['text']
            if row['kind'] == 'agent_message':
                try:
                    value = json.loads(text)
                    key = (value['sender'], value['room'], value.get('progress_key'))
                    if value.get('importance') == 'progress' and key in latest and key not in conflicts:
                        if row['id'] != latest[key][1]:
                            continue
                        if counts[key] > 1:
                            text = packed({**value, 'earlierProgressUpdates': counts[key] - 1,
                                           'history': 'Read earlier progress with orchestration_chat_read in this room.'})
                except (ValueError, KeyError, TypeError):
                    pass
            parts.append(text if row['kind'] == 'user' else '[Orchestration event: ' + row['kind'] + ']\n' + text)
        return '\n\n'.join(parts)
