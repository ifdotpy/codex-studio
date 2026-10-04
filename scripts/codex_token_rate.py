"""Volatile per-response output-token rates. Never enters agents or sync projections."""
from collections import OrderedDict, deque
import logging
import math
import threading
import time

MIN_DURATION = .25
MAX_RATE = 1000.0
RECENT_OUTPUT_SECONDS = 2.5
_logged_implausible_correction = False
NON_TOOLS = {'userMessage', 'agentMessage', 'reasoning', 'plan', 'contextCompaction', 'compactionSnapshot'}


def count(value):
    return value if type(value) in (int, float) and math.isfinite(value) and value >= 0 else None


def event_time(message, method=None):
    """Select a provider event time, then the supervisor receive time."""
    params = message.get('params') if isinstance(message, dict) else None
    params = params if isinstance(params, dict) else {}
    item = params.get('item') if isinstance(params.get('item'), dict) else {}
    turn = params.get('turn') if isinstance(params.get('turn'), dict) else {}
    if method in {'item/completed', 'turn/completed'}:
        native_fields = ('completedAt', 'timestamp', 'time', 'createdAt')
    elif method in {'item/started', 'turn/started'}:
        native_fields = ('startedAt', 'timestamp', 'time', 'createdAt')
    else:
        native_fields = ('completedAt', 'timestamp', 'time', 'createdAt')
    for source in (params, item, turn):
        for field in native_fields:
            value = count(source.get(field))
            if value is not None:
                return value / 1000 if value > 100_000_000_000 else value
    for field in ('_studioSupervisorReceivedAt', '_studioSupervisorTime', '_studioSequenceAt', '_studioReceivedAt'):
        value = count(message.get(field)) if isinstance(message, dict) else None
        if value is not None:
            return value / 1000 if value > 100_000_000_000 else value
    return None


class TurnRate:
    def __init__(self, turn, now, timing_valid=True):
        self.turn = turn
        self.active = True
        self.last = None
        self.rate = 0.0
        self.actual = 0.0
        self.pending = 0.0
        self.estimated = False
        self.baseline = None
        self.messages = {}
        self.timing_valid = timing_valid
        self.generation_started = now if timing_valid else None
        self.generation_elapsed = 0.0
        self.tools = set()
        self.text_samples = deque()
        self.text_window_tokens = 0.0
        self.last_output_at = None
        self.response_durations = deque(maxlen=1024)
        self.generation_response_id = None

    def text(self, text, now):
        if not self.active or not text:
            return
        self.pending += len(text) / 4.0
        self.estimated = True
        self.last_output_at = now
        self.text_samples.append((now, len(text) / 4.0))
        self.text_window_tokens += len(text) / 4.0
        while self.text_samples and now - self.text_samples[0][0] > 4:
            self.text_window_tokens -= self.text_samples.popleft()[1]
        if self.timing_valid:
            window = max(1.0, now - self.text_samples[0][0])
            self.rate = round(min(MAX_RATE, self.text_window_tokens / window), 2)

    def start_tool(self, item_id, now):
        if not self.active or item_id in self.tools:
            return
        if not self.tools and self.generation_started is not None:
            self.generation_elapsed += max(0.0, now - self.generation_started)
            # Native usage can arrive after this tool, or after later responses.
            self.response_durations.append((self.generation_elapsed, self.pending))
            self.generation_elapsed = 0.0
            self.generation_started = None
            self.pending = 0.0
            self.text_samples.clear()
            self.text_window_tokens = 0.0
            self.estimated = False
        self.tools.add(item_id)
        self.last_output_at = None

    def finish_tool(self, item_id, now):
        if item_id not in self.tools:
            return
        self.tools.remove(item_id)
        if not self.tools:
            self.generation_started = now
            self.timing_valid = True
            self.last_output_at = None

    def generation_time(self, now):
        if self.tools:
            return self.generation_elapsed
        if self.generation_started is None:
            return None
        return self.generation_elapsed + max(0.0, now - self.generation_started)

    def generation_started_at(self, now, response_id=None):
        if (self.active and not self.tools and
                (not response_id or response_id != self.generation_response_id)):
            self.generation_started = now
            self.generation_elapsed = 0.0
            self.timing_valid = True
            self.generation_response_id = response_id

    def response(self, output, now, identity=None):
        if not self.active or output is None:
            return False
        prior = self.messages.get(identity) if identity is not None else None
        if prior and output <= prior[0]:
            return False
        queued = bool(self.response_durations) and not prior
        current_estimate = (self.rate, self.estimated, self.last_output_at)
        duration = (prior[1] if prior and prior[1] is not None else
                    self.response_durations[0][0] if queued else self.generation_time(now))
        global _logged_implausible_correction
        if duration is not None and output / max(MIN_DURATION, duration) > MAX_RATE:
            if not _logged_implausible_correction:
                logging.getLogger(__name__).warning('Capped token rate above %.0f tok/s', MAX_RATE)
                _logged_implausible_correction = True
        current = prior[0] if prior else 0.0
        self.rate = (round(min(MAX_RATE, output / max(MIN_DURATION, duration)), 2)
                     if duration is not None and self.timing_valid else 0.0)
        self.actual += output - current
        if identity is not None:
            self.messages[identity] = (output, duration, self.rate)
        if queued:
            self.response_durations.popleft()
        if queued or prior:
            if self.pending:
                self.rate, self.estimated, self.last_output_at = current_estimate
            else:
                self.estimated = False
                self.last_output_at = now
        elif duration is not None:
            self.pending = 0.0
            self.estimated = False
            self.text_samples.clear()
            self.text_window_tokens = 0.0
            self.last_output_at = now
            self.generation_elapsed = 0.0
            self.generation_started = None if self.tools else now
            self.timing_valid = True
        else:
            self.pending = 0.0
            self.estimated = False
            self.text_samples.clear()
            self.text_window_tokens = 0.0
            self.last_output_at = now
        return True

    def correct(self, total, now):
        if not self.active or total < self.actual:
            return
        delta = total - self.actual
        if delta:
            self.response(delta, now)

    def reconcile(self, total):
        if not self.active or total < self.actual:
            return
        self.actual = total

    def snapshot(self, now):
        if not self.active:
            return self.last
        generating = (not self.tools and self.last_output_at is not None and
                      0 <= now - self.last_output_at <= RECENT_OUTPUT_SECONDS and self.rate > 0)
        return {'turnId': self.turn, 'active': True, 'estimated': self.estimated,
                'generating': generating, 'rate': self.rate,
                'outputTokens': round(self.actual + self.pending +
                                      sum(pending for _, pending in self.response_durations), 2)}

    def finish(self, now):
        self.last = {**self.snapshot(now), 'active': False, 'generating': False}
        self.active = False


class TokenRates:
    def __init__(self, clock=time.time):
        self.clock = clock
        self.lock = threading.RLock()
        self.entries = OrderedDict()
        self.agents = {}
        self.requests = {}
        self.response_rates = OrderedDict()
        self.pending_response_rates = OrderedDict()

    def request_started(self, account, connection, thread, request_id, at=None):
        now = at if count(at) is not None else self.clock()
        key = (account, connection, thread)
        with self.lock:
            entry = self.entries.get(key)
            rate = entry.get('rate') if entry else None
            if rate and rate.active:
                span = 'request:' + str(request_id)
                rate.start_tool(span, now)
                self.requests[(account, connection, str(request_id))] = (key, rate.turn, span)

    def request_finished(self, account, connection, request_id, at=None):
        now = at if count(at) is not None else self.clock()
        with self.lock:
            request = self.requests.pop((account, connection, str(request_id)), None)
            if request:
                key, turn, span = request
                entry = self.entries.get(key)
                rate = entry.get('rate') if entry else None
                if rate and rate.active and rate.turn == turn:
                    rate.finish_tool(span, now)

    def observe(self, agent, method, params, account, connection, at=None):
        key = (account, connection, agent.get('threadId'))
        now = at if count(at) is not None else self.clock()
        with self.lock:
            entry = self.entries.setdefault(key, {'agent': agent['id'], 'rate': None, 'lifetime': None})
            entry['root'] = agent.get('rootId') or agent['id']
            self.agents[agent['id']] = key
            self.entries.move_to_end(key)
            while len(self.entries) > 1024:
                old_key, old = self.entries.popitem(last=False)
                if self.agents.get(old['agent']) == old_key:
                    self.agents.pop(old['agent'], None)
                self.requests = {key: value for key, value in self.requests.items() if value[0] != old_key}
            turn = params.get('turnId') or (params.get('turn') or {}).get('id')
            rate = entry['rate']
            if method == 'turn/started' or (method == 'item/started' and agent.get('inFlight') and turn == agent.get('turnId')):
                if turn and (rate is None or turn != rate.turn):
                    rate = entry['rate'] = TurnRate(turn, now, timing_valid=method == 'turn/started')
                    rate.baseline = entry['lifetime']
                    self.requests = {request_id: value for request_id, value in self.requests.items()
                                     if value[0] != key or value[1] == turn}
            item = params.get('item') or {}
            if method in {'item/started', 'item/completed'} and rate and rate.active and turn == rate.turn:
                item_id = item.get('id') or params.get('itemId')
                if method == 'item/started' and item.get('type') == 'agentMessage' and not rate.timing_valid:
                    rate.generation_started = now
                    rate.generation_elapsed = 0.0
                    rate.timing_valid = True
                if isinstance(item_id, str) and item.get('type') not in NON_TOOLS:
                    if method == 'item/started':
                        rate.start_tool(item_id, now)
                    else:
                        rate.finish_tool(item_id, now)
                usage = params.get('tokenRateUsage')
                if isinstance(usage, dict):
                    self._response(rate, usage.get('outputTokens'), now, usage.get('responseId'), agent['id'], agent.get('threadId'))
            if method == 'thread/tokenUsage/updated':
                usage = params.get('tokenUsage') or {}
                total = count((usage.get('total') or {}).get('outputTokens'))
                last = count((usage.get('last') or {}).get('outputTokens'))
                if (rate is None or rate.turn != turn) and turn and agent.get('inFlight') and turn == agent.get('turnId'):
                    rate = entry['rate'] = TurnRate(turn, now, timing_valid=False)
                    rate.baseline = entry['lifetime']
                if rate and rate.active and (not turn or turn == rate.turn):
                    turn_output = count(params.get('turnOutputTokens'))
                    response_id = params.get('responseId')
                    response_output = count(params.get('responseOutputTokens'))
                    if response_output is None:
                        response_output = count((params.get('tokenUsage') or {}).get('last', {}).get('outputTokens'))
                    if response_id and response_output is not None:
                        self._response(rate, response_output, now, response_id, agent['id'], agent.get('threadId'))
                    if turn_output is not None:
                        rate.reconcile(turn_output)
                    elif total is not None:
                        if rate.baseline is None:
                            rate.baseline = max(0, total - last) if last is not None else total
                            rate.segment_turn_base = 0
                        if entry['lifetime'] is not None and total < entry['lifetime']:
                            rate.baseline = max(0, total - last) if last is not None else total
                            rate.segment_turn_base = rate.actual
                        turn_output = (getattr(rate, 'segment_turn_base', 0)
                                       + max(0, total - rate.baseline))
                        delta = turn_output - rate.actual
                        identity = response_id or ('total', total)
                        if delta > 0:
                            self._response(rate, delta, now, identity, agent['id'], agent.get('threadId'))
                    elif last is not None:
                        identity = response_id or ('last', (usage.get('last') or {}).get('totalTokens'), last)
                        self._response(rate, last, now, identity, agent['id'], agent.get('threadId'))
                if total is not None and (not rate or not turn or turn == rate.turn):
                    entry['lifetime'] = total
            if method == 'turn/completed' and rate and rate.active and turn == rate.turn:
                rate.finish(now)
                self.requests = {request_id: value for request_id, value in self.requests.items()
                                 if value[0] != key or value[1] != turn}

    def _response(self, rate, output, now, identity, agent_id, thread):
        output = count(output)
        if output is None or not rate.response(output, now, identity):
            return
        sample = rate.messages.get(identity) if identity is not None else None
        if isinstance(identity, str) and sample and sample[1] is not None:
            key = (agent_id, thread, rate.turn, identity)
            self.response_rates[key] = {'rate': sample[2], 'outputTokens': sample[0], 'durationSeconds': sample[1]}
            self.response_rates.move_to_end(key)
            while len(self.response_rates) > 4096:
                self.response_rates.popitem(last=False)
        elif isinstance(identity, tuple) and sample and sample[1] is not None:
            key = (agent_id, thread, rate.turn, identity)
            self.pending_response_rates[key] = {'rate': sample[2], 'outputTokens': sample[0],
                                                'durationSeconds': sample[1], 'responseId': None}
            self.pending_response_rates.move_to_end(key)
            while len(self.pending_response_rates) > 4096:
                self.pending_response_rates.popitem(last=False)

    def associate_response_rate(self, agent_id, thread, turn, response_id, output_tokens):
        output_tokens = count(output_tokens)
        if not isinstance(response_id, str) or not response_id or output_tokens is None:
            return False
        with self.lock:
            matches = [(key, sample) for key, sample in self.pending_response_rates.items()
                       if key[:3] == (agent_id, thread, turn) and sample['outputTokens'] == output_tokens
                       and sample['responseId'] is None]
            if len(matches) != 1:
                return False
            key, sample = matches[0]
            sample['responseId'] = response_id
            self.response_rates[(agent_id, thread, turn, response_id)] = {
                'rate': sample['rate'], 'outputTokens': sample['outputTokens'],
                'durationSeconds': sample['durationSeconds']}
            self.response_rates.move_to_end((agent_id, thread, turn, response_id))
            while len(self.response_rates) > 4096:
                self.response_rates.popitem(last=False)
            del self.pending_response_rates[key]
            return True

    def response_rate(self, agent_id, thread, turn, response_id):
        if not all(isinstance(value, str) and value for value in (agent_id, turn, response_id)):
            return None
        with self.lock:
            sample = self.response_rates.get((agent_id, thread, turn, response_id))
            return dict(sample) if sample else None

    def stream(self, method, params, account, connection, at=None):
        with self.lock:
            entry = self.entries.get((account, connection, params.get('threadId')))
            rate = entry['rate'] if entry else None
            if not rate or not rate.active or params.get('turnId') != rate.turn:
                return
            now = at if count(at) is not None else self.clock()
            if method == 'provider/generationStarted':
                rate.generation_started_at(now, params.get('responseId'))
            elif method in {'item/agentMessage/delta', 'item/reasoning/textDelta'}:
                delta = params.get('delta')
                if isinstance(delta, str):
                    rate.text(delta, now)
            elif method == 'provider/outputUsage':
                turn_output = count(params.get('turnOutputTokens'))
                output = turn_output - rate.actual if turn_output is not None else count(params.get('outputTokens'))
                if output is not None:
                    identity = params.get('responseId') or (('turn', turn_output) if turn_output is not None else None)
                    self._response(rate, output, now, identity, entry['agent'], params.get('threadId'))

    def team_snapshot(self, root_id):
        with self.lock:
            now = self.clock()
            return {entry['agent']: entry['rate'].snapshot(now)
                    for key, entry in self.entries.items()
                    if entry.get('root') == root_id and entry['agent'] != root_id
                    and self.agents.get(entry['agent']) == key and entry['rate'] is not None}

    def workspace_snapshot(self):
        with self.lock:
            now = self.clock()
            rates, teams = {}, {}
            for key, entry in self.entries.items():
                if self.agents.get(entry['agent']) != key or entry['rate'] is None:
                    continue
                value = entry['rate'].snapshot(now)
                rates[entry['agent']] = value
                root = entry.get('root')
                if root and root != entry['agent']:
                    teams.setdefault(root, {})[entry['agent']] = value
            return {'rates': rates, 'teams': teams}

    def snapshot(self, agent_id):
        with self.lock:
            entry = self.entries.get(self.agents.get(agent_id))
            return entry['rate'].snapshot(self.clock()) if entry and entry['rate'] else None


def token_rates(runtime):
    rates = runtime.__dict__.get('_token_rates')
    return rates if rates is not None else runtime.__dict__.setdefault('_token_rates', TokenRates())
