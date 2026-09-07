"""Bounded NDJSON input for panel state, with no model or event-queue dependency."""

import json
import threading
import time

MAX_FRAME_BYTES = 65536


class PanelFeedConsumer:
    """Drain stdout quickly and validate only the latest complete frame."""

    def __init__(self, update, status, interval_ms=1000, done=None):
        self.update = update
        self.status = status
        self.interval = interval_ms / 1000
        self.done = done
        self.condition = threading.Condition()
        self.buffer = bytearray()
        self.discard_line = False
        self.pending = None
        self.pending_status = None
        self.sequence = 0
        self.last_error = None
        self.ending = False
        self.suspended = False
        self.closed = False
        self.worker = threading.Thread(target=self._run, name="panel-feed", daemon=True)
        self.worker.start()

    def feed(self, chunk):
        with self.condition:
            if self.closed or self.ending or self.suspended:
                return
            start = 0
            while start < len(chunk):
                end = chunk.find(b"\n", start)
                stop = len(chunk) if end < 0 else end
                length = stop - start
                if not self.discard_line:
                    if len(self.buffer) + length > MAX_FRAME_BYTES:
                        self.buffer.clear()
                        self.discard_line = True
                        self.pending = (self.sequence + 1, None, "Panel feed frame exceeds 65536 bytes")
                    else:
                        self.buffer.extend(chunk[start:stop])
                if end < 0:
                    break
                self.sequence += 1
                if not self.discard_line and self.buffer.strip():
                    self.pending = (self.sequence, bytes(self.buffer), None)
                self.buffer.clear()
                self.discard_line = False
                start = end + 1
            self.condition.notify()

    def report(self, status, error=None):
        with self.condition:
            if self.closed or self.ending:
                return
            self.pending_status = (status, error, None)
            self.condition.notify()

    def suspend(self, status="stopping"):
        with self.condition:
            if self.closed or self.ending:
                return
            self.suspended = True
            self.pending = None
            self.buffer.clear()
            self.pending_status = (status, None, None)
            self.condition.notify()

    def finish(self, status, error=None, discard=False):
        with self.condition:
            if self.closed or self.ending:
                return
            # A final line requires a newline. Never apply a truncated write.
            if (self.buffer or self.discard_line) and not error and not discard:
                error = "Panel feed ended with an incomplete NDJSON frame"
            self.buffer.clear()
            self.discard_line = False
            if discard:
                self.pending = None
            self.pending_status = (status, error, None)
            self.ending = True
            self.condition.notify()

    def close(self):
        with self.condition:
            self.closed = True
            self.pending = None
            self.pending_status = None
            self.buffer.clear()
            self.condition.notify()

    @staticmethod
    def _parse(raw):
        def reject_constant(value):
            raise ValueError("Panel feed requires finite JSON values")
        value = json.loads(raw.decode("utf-8"), parse_constant=reject_constant)
        if not isinstance(value, dict):
            raise ValueError("Panel feed frames must be JSON objects")
        return value

    def _report(self, status, error, sequence):
        try:
            self.status(status, error, sequence)
        except Exception:
            # The owning panel can be deleted or replaced while validation runs.
            # A status failure must never start a model turn or replay a command.
            pass

    def _run(self):
        next_update = 0
        try:
            while True:
                frame = None
                report = None
                with self.condition:
                    while not self.closed:
                        now = time.monotonic()
                        if self.pending is not None and now >= next_update:
                            frame, self.pending = self.pending, None
                            next_update = now + self.interval
                            break
                        if self.pending_status is not None and (not self.ending or self.pending is None):
                            report, self.pending_status = self.pending_status, None
                            break
                        if self.ending and self.pending is None:
                            return
                        self.condition.wait(max(0, next_update - now) if self.pending is not None else None)
                    if self.closed:
                        return
                if frame is not None:
                    sequence, raw, error = frame
                    try:
                        if error:
                            raise ValueError(error)
                        self.update(self._parse(raw), sequence)
                        self.last_error = None
                    except Exception as cause:
                        self.last_error = str(cause)[:1000]
                        self._report("running", self.last_error, sequence)
                elif report is not None:
                    status, error, sequence = report
                    if self.ending and error is None:
                        error = self.last_error
                    self._report(status, error, sequence)
        finally:
            if self.done:
                self.done()
