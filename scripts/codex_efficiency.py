"""Small model views over durable Studio records. UI storage remains complete."""
import base64
import difflib
import hashlib
import json
import os
from pathlib import Path
import time


def packed(value):
    return json.dumps(value, ensure_ascii=False, separators=(',', ':'))


def clip(text, size):
    return str(text).encode('utf-8')[:size].decode('utf-8', errors='ignore')


def digest(value):
    return hashlib.sha256(packed(value).encode()).hexdigest()[:24]


def model_text_bytes(value):
    # Match the generic tool projection, including JSON escaping inside text.
    return len(packed([{'type': 'inputText', 'text': json.dumps(value, ensure_ascii=False)}]).encode())


def efficiency_tools(tool, text):
    return [tool('orchestration_read',
        'Read a full saved tool response or orchestration event by output_ref. Offsets are Unicode characters; pages are bounded. '
        'Use contains to locate relevant output. Same-agent records only. Never rerun a mutation to recover its result.',
        {'output_ref': text, 'offset': {'type': 'integer', 'minimum': 0},
         'contains': text}, ['output_ref']),
        tool('orchestration_context',
        'Read versioned Studio guidance or team context on demand. Panel set still requires visual validation. '
        'Use background guidance for script-driven status with no model polling.',
        {'topic': {'type': 'string', 'enum': ['panel', 'background', 'tools', 'plan', 'complaints', 'profiles', 'monitor']},
         'id': text}, ['topic'])]


class EfficiencyMixin:
    def model_page(self, rows, args, scope, byte_limit=None):
        limit = args.get('limit', 20)
        if type(limit) is not int or not 1 <= limit <= 50:
            raise ValueError('limit must be 1 to 50')
        revision = digest([scope, rows])
        offset = 0
        if args.get('cursor'):
            try:
                cursor = json.loads(base64.urlsafe_b64decode(args['cursor']))
                offset = cursor['offset']
                if type(offset) is not int or not 0 <= offset <= len(rows) or cursor['revision'] != revision:
                    raise ValueError()
            except (ValueError, TypeError, KeyError):
                raise ValueError('List changed or cursor is invalid. Read the first page again.') from None
        page = rows[offset:offset + limit]
        if byte_limit:
            while len(page) > 1 and model_text_bytes(page) > byte_limit:
                page.pop()
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
                if scope != 'team':
                    raise ValueError('Agent discovery is limited to your team')
                from codex_peer_teams import peers_for
                peer_ids = {peer['id'] for peer in peers_for(self, db, actor)}
                rows = [{k: a.get(k) for k in ('id', 'name', 'role', 'rootId', 'parentId', 'status')}
                        for a in agents if a['rootId'] == actor['rootId'] or a['id'] in peer_ids]
                rows.sort(key=lambda a: a['id'])
                rooms = [{k: r.get(k) for k in ('id', 'kind', 'members', 'rootId', 'peerTeamId', 'peerTeamName')}
                         for r in self.chat_rooms(db, actor_id)]
                rooms.sort(key=lambda r: r['id'])
                # One cursor covers both collections. Room discovery continues
                # after the agents, without repeating rooms on each agent page.
                entries = [{'entry': 'agent', 'value': r} for r in rows]
                entries += [{'entry': 'room', 'value': r} for r in rooms]
                result = self.model_page(entries, args, [actor_id, scope], byte_limit=13000)
                page = result.pop('items')
                result.update(self=actor_id, lead=actor['rootId'], parent=actor.get('parentId'),
                              items=[r['value'] for r in page if r['entry'] == 'agent'],
                              rooms=[r['value'] for r in page if r['entry'] == 'room'],
                              total=len(rows), roomTotal=len(rooms), totalRecords=len(entries),
                              peerChatIds=sorted(peer_ids),
                              peerChatPolicy='Peer chats are equal and independent. Send explicit private messages only. Do not assign work or forward results automatically.')
                return result
            team = [a for a in agents if a['rootId'] == actor['rootId']]
            ids = {a['id'] for a in team}
            records = [{**{k: a.get(k) for k in ('id', 'name', 'status', 'inFlight', 'parentId')}, 'kind': 'agent',
                        'error': clip(a.get('error') or '', 600)} for a in team]
            records += [{**{k: m.get(k) for k in ('id', 'agent', 'status', 'exitCode')}, 'kind': 'monitor',
                         'error': clip(m.get('error') or '', 600)} for m in self.recent_monitors(db, actor["rootId"]) if m['agent'] in ids]
            records.sort(key=lambda r: (r['kind'], r['id']))
            defaults = self.worker_defaults(self.agent(actor['rootId'], db))
            from codex_native_errors import native_thread_block
            from codex_safety_buffering import active as safety_retry_active
            global_limit = max(1, min(64, int(os.environ.get('CODEX_CANVAS_CONCURRENCY', '32'))))
            active = [a for a in agents if a.get('inFlight') or a['status'] in {'running', 'starting', 'approval'}]
            team_active = sum(a['rootId'] == actor['rootId'] for a in active)
            root = self.agent(actor['rootId'], db)
            reservations = {str(Path(a['cwd']).resolve()): a['id'] for a in agents if a.get('workspaceOperation')}
            queued = []
            for a in team:
                if a['status'] != 'queued':
                    continue
                reasons = []
                if not a.get('autoWake'):
                    reasons.append('paused')
                for field in ('nativeFailureHold', 'accountTransferId'):
                    if a.get(field):
                        reasons.append(field)
                if safety_retry_active(a):
                    reasons.append('safety_retry')
                if native_thread_block(a):
                    reasons.append('native_thread_blocked')
                if a.get('browserRecovery', {}).get('stage') in {'pending', 'reconnecting'}:
                    reasons.append('browser_recovery')
                blocker = reservations.get(str(Path(a['cwd']).resolve()))
                if blocker:
                    reasons.append('workspace_operation')
                if len(active) >= global_limit:
                    reasons.append('global_concurrency')
                if team_active >= a['concurrency']:
                    reasons.append('team_concurrency')
                queued.append({'id': a['id'], 'reasons': reasons or ['awaiting_dispatch'],
                               **({'blockingAgent': blocker} if blocker else {})})
            capacity = {'teamLimit': root['concurrency'], 'globalLimit': global_limit,
                        'teamActive': team_active, 'globalActive': len(active),
                        'maxAgents': root['maxAgents'], 'queued': queued,
                        'configure': {'command': 'codex-control configure ' + actor['rootId'] + ' --concurrency N',
                                      'minimum': 1, 'maximum': 64,
                                      'note': 'Change team capacity only within user authorization. The global server limit still applies.'}}
            revision = digest([records, defaults, capacity])
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
                    'capacity': capacity,
                    'help': 'Use orchestration_context for tools, profiles or monitor details. Use orchestration_peers for rooms.'}

    def model_tool_result(self, actor, key, result):
        from codex_agent_modes import tool_mode_context
        result = tool_mode_context(self, actor, result, key)
        content = result.get('contentItems', [])
        texts = [c for c in content if c.get('type') == 'inputText' and not c.get('text', '').startswith(('[Time awareness]', '[Studio agent mode,'))]
        if len(packed(texts).encode()) <= 16000:
            return result
        # The full result was committed before this projection. Images and
        # immutable clock metadata stay intact.
        raw = '\n'.join(c.get('text', '') for c in texts)
        preview = {'requestId': key, 'outputRef': key, 'success': result.get('success'),
                   'truncated': True, 'textBytes': len(raw.encode()), 'excerpt': clip(raw, 4000),
                   'read': {'tool': 'orchestration_read', 'output_ref': key}}
        with self.db() as db:
            receipt = self.tool_request(key, db)
            preview['outcome'] = receipt.get('outcome', 'unknown') if receipt else 'unknown'
        # Keep operation identities outside an excerpt, even for large spawn batches.
        try:
            value = json.loads(raw)
            if isinstance(value, dict):
                preview['summary'] = {k: value[k] for k in
                    ('id', 'requestId', 'stage', 'outcome', 'status', 'exitCode', 'version', 'nextCursor', 'nextBefore', 'revision', 'total', 'nextOffset', 'agentIds', 'agent_ids') if k in value}
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
            if isinstance(key, str) and key.startswith(('event:', 'chat-message:')):
                return self.model_saved_message(db, actor, key, args)
            receipt = self.tool_request(key, db)
            native = None
            if receipt is None and isinstance(key, str) and key.startswith(actor['id'] + ':'):
                # Native item IDs use the stable Studio agent ID, including after
                # account transfer. Never authorize by a caller-supplied thread ID.
                row = db.execute('SELECT record FROM runtime_items WHERE id=? AND agent=?', (key, actor['id'])).fetchone()
                native = json.loads(row[0]) if row else None
                if native and native.get('title') == 'dynamicToolCall':
                    call_id = key[len(actor['id']) + 1:]
                    aliases = db.execute('SELECT request FROM runtime_tool_request_aliases WHERE agent=? AND alias=? LIMIT 2',
                                         (actor['id'], call_id)).fetchall()
                    if len(aliases) > 1:
                        raise ValueError('Output reference is ambiguous. Use the canonical request id.')
                    receipt = self.tool_request(aliases[0][0], db) if aliases else None
                    if receipt and receipt.get('agent') == actor['id']:
                        key = receipt['id']
                    native = None
                elif native and native.get('title') != 'commandExecution':
                    native = None
            if native:
                full = None
                if native.get('truncated') and db.execute("SELECT 1 FROM sqlite_master WHERE name='runtime_search'").fetchone():
                    # Runtime.item indexes the original text in the same transaction
                    # before it clips the transcript view. Reuse that durable body.
                    full = db.execute('SELECT s.body FROM runtime_search_rows r JOIN runtime_search s ON s.rowid=r.search_rowid '
                                      'WHERE r.id=? AND s.id=? AND s.agent=?', (key, key, actor['id'])).fetchone()
                try:
                    payload = json.loads(full[0] if full else native['text'])
                except (ValueError, TypeError, KeyError):
                    raise ValueError('Saved command output is truncated or unreadable. Do not repeat the command.') from None
                text = payload.get('aggregatedOutput') if isinstance(payload, dict) else None
                if not isinstance(text, str):
                    raise ValueError('Saved command output is unavailable. Do not repeat the command.')
                native = {**native, 'truncated': bool(payload.get('outputTruncated')) or bool(native.get('truncated') and not full)}
                result = {'success': payload.get('status') == 'completed' and payload.get('exitCode') == 0}
                receipt = {'outcome': 'unknown'}
            else:
                if receipt is None or receipt.get('agent') != actor['id'] or receipt.get('stage') == 'ambiguous':
                    raise ValueError('Output reference is not owned by this agent')
                key = receipt['id']
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
                    return {'outputRef': key, 'found': False, 'success': result.get('success'), 'outcome': receipt['outcome'],
                            **({'source': 'saved_native_output', 'truncated': bool(native.get('truncated'))} if native else {})}
                offset = max(offset, found - 200)
            excerpt = clip(text[offset:], 3000)
            end = offset + len(excerpt)
            return {'outputRef': key, 'success': result.get('success'), 'outcome': receipt['outcome'],
                    'offset': offset, 'nextOffset': end if end < len(text) else None,
                    'totalChars': len(text), 'text': excerpt,
                    **({'source': 'saved_native_output', 'truncated': bool(native.get('truncated')),
                        'commandStatus': payload.get('status'), 'exitCode': payload.get('exitCode')} if native else {})}

    def model_saved_message(self, db, actor, key, args):
        source, record_id = key.split(':', 1)
        if source == 'event':
            row = db.execute('SELECT * FROM runtime_events WHERE id=? AND agent=?',
                             (record_id, actor['id'])).fetchone()
            if row is None:
                raise ValueError('Event reference is not owned by this agent')
            from codex_team_isolation import assert_events
            assert_events(self, db, actor, [row])
            meta = db.execute('SELECT record FROM runtime_event_meta WHERE id=?', (record_id,)).fetchone()
            metadata = json.loads(meta[0]) if meta else {}
            identity = {k: row[k] for k in ('id', 'agent', 'kind', 'created', 'epoch', 'turn_id')}
            identity['metadata'] = {k: v for k, v in metadata.items() if k != 'contextManifest'}
        else:
            row = db.execute('SELECT * FROM runtime_chat_messages WHERE id=?', (record_id,)).fetchone()
            if row is None or not any(r['id'] == row['room'] for r in self.chat_rooms(db, actor['id'])):
                raise ValueError('Chat message is unavailable or you are not a participant')
            identity = {k: row[k] for k in ('id', 'room', 'sender', 'seq', 'created')}
        text = row['text']
        offset = args.get('offset', 0)
        if type(offset) is not int or not 0 <= offset <= len(text):
            raise ValueError('offset must be within the saved text')
        if args.get('contains'):
            found = text.find(args['contains'], offset)
            if found < 0:
                return {'outputRef': key, 'found': False, 'source': source, 'identity': identity}
            offset = max(offset, found - 200)
        excerpt = clip(text[offset:], 3000)
        end = offset + len(excerpt)
        return {'outputRef': key, 'source': source, 'identity': identity, 'offset': offset,
                'nextOffset': end if end < len(text) else None, 'totalChars': len(text), 'text': excerpt}

    def model_chat_page(self, room, messages, more):
        # UI reads keep complete pages. Model reads return recent messages first
        # within the byte budget; every clipped body has an authorized reader.
        room = {k: room[k] for k in ('id', 'kind', 'rootId', 'name') if k in room}
        page = []
        for message in reversed(messages):
            brief = dict(message)
            brief.pop('deliveries', None)
            if len(brief['text'].encode()) > 3000:
                brief.update(text=clip(brief['text'], 2400), truncated=True,
                             textChars=len(message['text']),
                             read={'tool': 'orchestration_read', 'output_ref': 'chat-message:' + message['id']})
            candidate = [brief, *page]
            if page and model_text_bytes({'room': room, 'messages': candidate}) > 13000:
                break
            page = candidate
        return {'room': room, 'messages': page,
                'nextBefore': page[0]['seq'] if page and (more or len(page) < len(messages)) else None}

    def remember_role_text(self, text):
        # A later role change is sent as a diff against the delivered version.
        # Without this snapshot the full role text is sent again.
        path = self.root / 'context-snapshots' / (digest(text) + '.txt')
        if not path.exists():
            path.parent.mkdir(parents=True, exist_ok=True)
            temporary = path.with_suffix('.tmp.' + str(os.getpid()))
            temporary.write_text(text, encoding='utf-8')
            os.replace(temporary, path)

    def role_update(self, previous_version, text):
        try:
            previous = (self.root / 'context-snapshots' / (previous_version + '.txt')).read_text(encoding='utf-8')
        except (OSError, TypeError):
            return text
        if digest(previous) != previous_version:
            return text
        diff = ''.join(difflib.unified_diff((previous + '\n').splitlines(True), (text + '\n').splitlines(True),
                                            'delivered', 'current', n=2))
        # A large rewrite is clearer as the complete current text.
        if not diff or len(diff) * 2 > len(text):
            return text
        header, source = text.split('\n', 2)[:2]
        return ('[Studio role skill update: ' + header.removeprefix('[Studio role skill: ').removesuffix(']') + ']\n'
                'Studio changed your role skill. Apply this diff to the role skill you received earlier. '
                'It is not a user message. ' + source + '\n' + diff + '[End Studio role skill update]')

    def preparation_context_versions(self, actor, params):
        from codex_progress import progress_context
        values = {'roleSkill': self.role_guidance(actor),
                  'progressFile': progress_context(self.root, actor['id'])}
        if values['roleSkill'] in params.get('developerInstructions', ''):
            self.remember_role_text(values['roleSkill'])
        # Guidance can change between construction and capture. Only acknowledge
        # a version whose exact text is present in the submitted instructions.
        instructions = params.get('developerInstructions', '')
        return {key: digest(value) for key, value in values.items() if value and value in instructions}

    def model_known_context(self, db, actor):
        epoch = [actor.get('threadId'), actor.get('compactions', 0)]
        row = db.execute("SELECT m.record FROM runtime_event_meta m JOIN runtime_events e ON e.id=m.id WHERE e.agent=? AND e.status='delivered' AND json_extract(m.record,'$.contextManifest') IS NOT NULL ORDER BY coalesce(json_extract(m.record,'$.contextManifest.sequence'),0) DESC, e.created DESC LIMIT 1", (actor['id'],)).fetchone()
        old = json.loads(row[0]).get('contextManifest', {}) if row else {}
        known = dict(old.get('versions', {})) if old.get('epoch') == epoch else {}
        prepared = actor.get('preparedContext') or {}
        # Native compaction retains developer instructions on the same thread.
        # Only preparation proves these blocks were submitted at that priority.
        prepared_epoch = prepared.get('epoch') or []
        if prepared_epoch and prepared_epoch[0] == epoch[0]:
            for key, version in prepared.get('versions', {}).items():
                known.setdefault(key, version)
        mode = actor.get('deliveredMode') or {}
        if mode.get('epoch') == epoch:
            from codex_agent_modes import guidance
            current = digest(guidance(self.agent(actor['rootId'], db)))
            if mode.get('version') == current:
                known['agentMode'] = current
        return epoch, old, known

    def confirm_model_tool_result(self, actor_id, key, result, delivery_epoch=None):
        # A projection is not a delivery. Call only after the native response
        # write succeeds. A failed write leaves the policy eligible for refresh.
        with self.lock, self.db() as db:
            actor = self.agent(actor_id, db)
            epoch = [actor.get('threadId'), actor.get('compactions', 0)]
            if delivery_epoch is not None and delivery_epoch != epoch:
                return
            table = db.execute("SELECT 1 FROM sqlite_master WHERE name='runtime_model_modes'").fetchone()
            if not table:
                return
            rows = db.execute('SELECT record FROM runtime_model_modes WHERE agent=? AND request=?',
                              (actor_id, key)).fetchall()
            texts = [c.get('text') for c in result.get('contentItems', []) if c.get('type') == 'inputText']
            for row in rows:
                record = json.loads(row[0])
                if record['epoch'] != epoch or not record.get('text') or record['text'] not in texts:
                    continue
                previous = actor.get('deliveredMode') or {}
                if previous.get('epoch') == epoch and previous.get('revision', -1) > record['revision']:
                    continue
                actor['deliveredMode'] = {k: record[k] for k in ('epoch', 'version', 'revision')}
                self.put(db, 'agents', actor)

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
                from codex_progress import progress_context
                value = progress_context(self.root, actor['id']) + '\n\n' + self.panel_guidance()
            elif topic == 'background':
                from codex_progress import progress_context
                value = {'workflow': progress_context(self.root, actor['id']),
                         'monitor': 'Use orchestration_monitor for commands needing a result. wake_on=failure suppresses a successful exit notification; errors still wake you.',
                         'polling': 'Let a script write PROGRESS.md when its facts change. Do not poll unchanged status through model calls.'}
            elif topic == 'tools':
                value = self.tool_definitions(actor)
            else:
                raise ValueError('Unknown context topic')
        return {'topic': topic, 'version': digest(value), 'content': value}

    def model_turn_context(self, db, actor, event_id):
        epoch, old, known = self.model_known_context(db, actor)
        versions, blocks = {}, []
        from codex_agent_modes import guidance
        mode = guidance(self.agent(actor['rootId'], db))
        versions['agentMode'] = digest(mode)
        if known.get('agentMode') != versions['agentMode']:
            blocks.append(mode)
        from codex_progress import progress_context
        progress = progress_context(self.root, actor['id'])
        versions['progressFile'] = digest(progress)
        if known.get('progressFile') != versions['progressFile']:
            self.progress_file(actor)
            blocks.append('[Studio progress file]\n' + progress)
        role_skill = self.role_guidance(actor)
        versions['roleSkill'] = digest(role_skill)
        if known.get('roleSkill') != versions['roleSkill']:
            self.remember_role_text(role_skill)
            blocks.append(self.role_update(known.get('roleSkill'), role_skill))
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
    def bounded_event(row, text, byte_limit):
        if len(text.encode()) <= byte_limit:
            return text
        ref = 'event:' + row['id']
        summary = {'eventId': row['id'], 'kind': row['kind'], 'truncated': True,
                   'textBytes': len(row['text'].encode()),
                   'read': {'tool': 'orchestration_read', 'output_ref': ref}}
        try:
            value = json.loads(text)
            if isinstance(value, dict):
                summary['identity'] = {k: value[k] for k in
                    ('id', 'agent_id', 'sender', 'room', 'message_id', 'turnId', 'turn_id',
                     'status', 'exitCode', 'importance', 'progress_key', 'progress_version',
                     'earlierProgressUpdates', 'complaint_id', 'lead', 'log', 'bytes') if k in value}
        except (ValueError, TypeError):
            pass
        # Preserve exact identities when they fit. The reference always points
        # to the full, access-checked event and its sender/turn metadata.
        if len(packed(summary).encode()) > byte_limit:
            summary.pop('identity', None)
        if len(packed(summary).encode()) > byte_limit:
            summary.pop('eventId', None)  # The exact id remains in output_ref.
        remaining = max(0, byte_limit - len(packed(summary).encode()) - 100)
        summary['excerpt'] = clip(text, remaining // 2)
        summary['tail'] = text.encode()[-(remaining // 2):].decode('utf-8', errors='ignore') if remaining > 1 else ''
        while len(packed(summary).encode()) > byte_limit and (summary['excerpt'] or summary['tail']):
            summary['excerpt'] = summary['excerpt'][:len(summary['excerpt']) // 2]
            summary['tail'] = summary['tail'][len(summary['tail']) // 2:] if len(summary['tail']) > 1 else ''
        return packed(summary)

    @staticmethod
    def model_event_text(rows):
        # Only versioned progress for the same sender, room and topic supersedes an
        # earlier update. Keep every original event and receipt in the database.
        latest, counts, versions, conflicts = {}, {}, {}, set()
        for row in rows:
            if row['kind'] != 'agent_message' or row.get('preserveProgress'):
                continue
            try:
                value = json.loads(row['text'])
                if (not isinstance(value, dict) or value.get('importance') != 'progress'
                        or not all(isinstance(value.get(k), str) and value[k]
                                   for k in ('sender', 'room', 'progress_key'))
                        or type(value.get('progress_version')) is not int
                        or value['progress_version'] < 0):
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
        synthetic_count = sum(row['kind'] not in {'user', 'followup'} for row in rows)
        event_limit = min(3000, 24000 // max(1, synthetic_count))
        for row in rows:
            if row['kind'] == 'complaint' and not row.get('preserveComplaint'):
                try:
                    complaints = json.loads(row['text']).get('complaints')
                    if (isinstance(complaints, list) and complaints
                            and all(isinstance(c, dict) and isinstance(c.get('complaint_id'), str) for c in complaints)):
                        continue
                except (ValueError, TypeError, AttributeError):
                    pass
            text = row['text']
            if row['kind'] == 'agent_message' and not row.get('preserveProgress'):
                try:
                    value = json.loads(text)
                    key = (value['sender'], value['room'], value.get('progress_key'))
                    if (value.get('importance') == 'progress'
                            and type(value.get('progress_version')) is int and value['progress_version'] >= 0
                            and all(isinstance(v, str) and v for v in key)
                            and key in latest and key not in conflicts):
                        if row['id'] != latest[key][1]:
                            continue
                        if counts[key] > 1:
                            text = packed({**value, 'earlierProgressUpdates': counts[key] - 1,
                                           'history': 'Read earlier progress with orchestration_chat_read in this room.'})
                except (ValueError, KeyError, TypeError):
                    pass
            if row['kind'] not in {'user', 'followup', 'radio_turn'}:
                text = EfficiencyMixin.bounded_event(row, text, event_limit)
            parts.append(text if row['kind'] == 'user' else '[Orchestration event: ' + row['kind'] + ']\n' + text)
        return '\n\n'.join(parts)
