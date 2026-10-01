"""Volatile output-token telemetry. Never enters agents or sync projections."""
from collections import OrderedDict, deque
import math
import threading
import time

WINDOW = 4.0
BUCKET = .25


def count(value):
    return value if type(value) in (int, float) and math.isfinite(value) and value >= 0 else None


class TurnRate:
    def __init__(self, turn, now):
        self.turn = turn
        self.started = self.anchor = now
        self.active = True
        self.bins = deque()
        self.pending = 0.0
        self.actual = 0.0
        self.has_actual = False
        self.estimated = False
        self.messages = {}
        self.baseline = None
        self.last = None

    def prune(self, now):
        while self.bins and self.bins[0][0] <= now - WINDOW:
            self.bins.popleft()

    def text(self, text, now):
        if not self.active or not text:
            return
        self.prune(now)
        tokens = len(text) / 4.0
        stamp = math.floor(now / BUCKET) * BUCKET
        if not self.bins or self.bins[-1][0] != stamp:
            self.bins.append([stamp, 0.0, 0.0])  # confirmed, estimated
        self.bins[-1][2] += tokens
        self.pending += tokens
        self.estimated = True

    def correct(self, total, now):
        if not self.active or total < self.actual or (self.has_actual and total == self.actual):
            return
        self.prune(now)
        delta = total - self.actual
        if self.pending:
            # Correct the whole interval, including estimates outside the window.
            # A late usage notice must not look like a new burst of output.
            ratio = delta / self.pending
            for sample in self.bins:
                sample[1] += sample[2] * ratio
                sample[2] = 0.0
        elif delta:
            duration = max(BUCKET, now - self.anchor)
            start = max(now - duration, now - WINDOW)
            stamp = start
            while stamp < now:
                end = min(stamp + BUCKET, now)
                self.bins.append([end, delta * (end - stamp) / duration, 0.0])
                stamp = end
        self.actual, self.pending, self.anchor = total, 0.0, now
        self.estimated = False
        self.has_actual = True

    def snapshot(self, now):
        if not self.active:
            return self.last
        self.prune(now)
        tokens = sum(sample[1] + sample[2] for sample in self.bins)
        value = {
            'turnId': self.turn, 'active': True, 'estimated': self.estimated,
            'rate': round(tokens / min(WINDOW, max(BUCKET, now - self.started)), 2),
            'outputTokens': round(self.actual + self.pending, 2),
        }
        return value

    def finish(self, now):
        self.last = {**self.snapshot(now), 'active': False}
        self.active = False


class TokenRates:
    def __init__(self, clock=time.monotonic):
        self.clock = clock
        self.lock = threading.RLock()
        self.entries = OrderedDict()
        self.agents = {}

    def observe(self, agent, method, params, account, connection):
        key = (account, connection, agent.get('threadId'))
        now = self.clock()
        with self.lock:
            entry = self.entries.setdefault(key, {'agent': agent['id'], 'rate': None, 'lifetime': None})
            entry['root'] = agent.get('rootId') or agent['id']
            self.agents[agent['id']] = key
            self.entries.move_to_end(key)
            while len(self.entries) > 1024:
                old_key, old = self.entries.popitem(last=False)
                if self.agents.get(old['agent']) == old_key:
                    self.agents.pop(old['agent'], None)
            turn = params.get('turnId') or (params.get('turn') or {}).get('id')
            rate = entry['rate']
            if method == 'turn/started' or (method == 'item/started' and agent.get('inFlight') and turn == agent.get('turnId')):
                if turn and (rate is None or turn != rate.turn):
                    rate = entry['rate'] = TurnRate(turn, now)
                    rate.baseline = entry['lifetime']
            if method in {'item/started', 'item/completed'} and isinstance(params.get('tokenRateUsage'), dict):
                self.stream('provider/outputUsage', {**params['tokenRateUsage'],
                            'threadId': agent.get('threadId'), 'turnId': turn}, account, connection)
            if method == 'thread/tokenUsage/updated':
                usage = params.get('tokenUsage') or {}
                total = count((usage.get('total') or {}).get('outputTokens'))
                last = count((usage.get('last') or {}).get('outputTokens'))
                if rate and rate.active and (not turn or turn == rate.turn):
                    turn_output = count(params.get('turnOutputTokens'))
                    if turn_output is not None:
                        rate.correct(turn_output, now)
                    elif total is not None:
                        if rate.baseline is None and last is not None:
                            rate.baseline = total - last
                        if rate.baseline is not None and total >= rate.baseline:
                            rate.correct(total - rate.baseline, now)
                    elif last is not None:
                        identity = params.get('responseId') or (usage.get('total') or {}).get('totalTokens')
                        if identity is not None:
                            rate.messages[identity] = last
                            rate.correct(sum(rate.messages.values()), now)
                if total is not None and (not rate or not turn or turn == rate.turn):
                    entry['lifetime'] = total
            if method == 'turn/completed' and rate and rate.active and turn == rate.turn:
                rate.finish(now)

    def stream(self, method, params, account, connection):
        with self.lock:
            entry = self.entries.get((account, connection, params.get('threadId')))
            rate = entry['rate'] if entry else None
            if not rate or not rate.active or params.get('turnId') != rate.turn:
                return
            now = self.clock()
            if method in {'item/agentMessage/delta', 'item/reasoning/textDelta'}:
                delta = params.get('delta')
                if isinstance(delta, str):
                    rate.text(delta, now)
            elif method == 'provider/outputUsage':
                total = count(params.get('turnOutputTokens'))
                output = count(params.get('outputTokens'))
                response = params.get('responseId')
                if total is not None:
                    rate.correct(total, now)
                elif output is not None and isinstance(response, str):
                    rate.messages[response] = max(output, rate.messages.get(response, 0))
                    rate.correct(sum(rate.messages.values()), now)

    def team_snapshot(self, root_id):
        with self.lock:
            now = self.clock()
            return {entry['agent']: entry['rate'].snapshot(now)
                    for key, entry in self.entries.items()
                    if entry.get('root') == root_id and entry['agent'] != root_id
                    and self.agents.get(entry['agent']) == key
                    and entry['rate'] is not None and entry['rate'].active}

    def snapshot(self, agent_id):
        with self.lock:
            entry = self.entries.get(self.agents.get(agent_id))
            return entry['rate'].snapshot(self.clock()) if entry and entry['rate'] else None


def token_rates(runtime):
    rates = runtime.__dict__.get('_token_rates')
    return rates if rates is not None else runtime.__dict__.setdefault('_token_rates', TokenRates())
