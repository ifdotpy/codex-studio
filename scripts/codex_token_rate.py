"""Volatile output-token telemetry. Never enters agents or sync projections."""
from collections import OrderedDict, deque
import logging
import math
import threading
import time

WINDOW = 4.0
BUCKET = .25
IDLE_GAP = 1.0
MAX_RATE = 1000.0
_logged_implausible_correction = False


def count(value):
    return value if type(value) in (int, float) and math.isfinite(value) and value >= 0 else None


class TurnRate:
    def __init__(self, turn, now):
        self.turn = turn
        self.started = self.last_activity = now
        self.active_time = 0.0
        self.anchor = 0.0
        self.has_activity = False
        self.active = True
        self.bins = deque()
        self.pending = 0.0
        self.actual = 0.0
        self.has_actual = False
        self.estimated = False
        self.messages = {}
        self.baseline = None
        self.last = None
        self.rate = 0.0

    def advance(self, now):
        elapsed = max(0.0, now - self.last_activity)
        if elapsed <= IDLE_GAP:
            self.active_time += elapsed
        else:
            self.active_time += BUCKET
        self.last_activity = now

    def prune(self, active_time):
        while self.bins and self.bins[0][0] < active_time - WINDOW:
            self.bins.popleft()

    def update_rate(self):
        self.prune(self.active_time)
        tokens = sum(sample[1] + sample[2] for sample in self.bins)
        self.rate = round(min(MAX_RATE, tokens / min(WINDOW, max(BUCKET, self.active_time))), 2)

    def text(self, text, now):
        if not self.active or not text:
            return
        self.advance(now)
        self.prune(self.active_time)
        tokens = len(text) / 4.0
        stamp = math.floor(self.active_time / BUCKET) * BUCKET
        if not self.bins or self.bins[-1][0] != stamp:
            self.bins.append([stamp, 0.0, 0.0])  # confirmed, estimated
        self.bins[-1][2] += tokens
        self.pending += tokens
        self.estimated = True
        self.has_activity = True
        self.update_rate()

    def correct(self, total, now):
        if not self.active or total < self.actual or (self.has_actual and total == self.actual):
            return
        delta = total - self.actual
        elapsed = max(0.0, now - self.last_activity)
        active_time = self.active_time + (
            elapsed if elapsed <= IDLE_GAP or not self.has_activity else BUCKET
        )
        duration = max(BUCKET, active_time - self.anchor)
        global _logged_implausible_correction
        if delta / duration > MAX_RATE:
            if not _logged_implausible_correction:
                logging.getLogger(__name__).warning(
                    'Ignored token-rate correction above %.0f tok/s', MAX_RATE)
                _logged_implausible_correction = True
            return
        # Replace text estimates with the provider's count over the full
        # interval. A receipt must not turn into a one-bucket output burst.
        self.active_time = active_time
        self.last_activity = now
        self.prune(self.active_time)
        self.bins = deque(sample for sample in self.bins if sample[0] <= self.anchor)
        for sample in self.bins:
            sample[2] = 0.0
        interval = self.active_time - self.anchor
        if delta and interval > 0:
            start = max(self.anchor, self.active_time - WINDOW)
            stamp = start
            while stamp < self.active_time:
                bucket = math.floor(stamp / BUCKET) * BUCKET
                end = min(bucket + BUCKET, self.active_time)
                overlap = max(0.0, end - max(stamp, self.anchor))
                if self.bins and self.bins[-1][0] == bucket:
                    self.bins[-1][1] += delta * overlap / interval
                else:
                    self.bins.append([bucket, delta * overlap / interval, 0.0])
                stamp = end
        self.actual, self.pending, self.anchor = total, 0.0, self.active_time
        self.estimated = False
        self.has_actual = True
        self.has_activity = True
        self.update_rate()

    def snapshot(self, now):
        if not self.active:
            return self.last
        value = {
            'turnId': self.turn, 'active': True, 'estimated': self.estimated,
            'rate': self.rate,
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
                if (rate is None or rate.turn != turn) and turn and agent.get('inFlight') and turn == agent.get('turnId'):
                    # A resumed native thread can report usage before it repeats
                    # turn/started. Rebuild the volatile meter from its current turn.
                    rate = entry['rate'] = TurnRate(turn, now)
                    rate.baseline = entry['lifetime']
                if rate and rate.active and (not turn or turn == rate.turn):
                    turn_output = count(params.get('turnOutputTokens'))
                    if turn_output is not None:
                        rate.correct(turn_output, now)
                    elif total is not None:
                        seeded_without_last = False
                        if rate.baseline is None:
                            # On restart, total is cumulative for the thread.
                            # Use the current response count to seed its base.
                            if last is None:
                                rate.baseline = total
                                seeded_without_last = True
                            else:
                                rate.baseline = max(0, total - last)
                        if not seeded_without_last and rate.baseline is not None and total >= rate.baseline:
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
                    and entry['rate'] is not None}

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
