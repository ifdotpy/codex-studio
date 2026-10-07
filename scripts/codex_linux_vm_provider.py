"""Use the native supervisor protocol through the Linux VM guest client.

Request remap, initialization, operation identities, and event ACK rules remain
in codex_process_supervisor.ProcessProxy. This adapter changes only transport
and the journal read used to repair an acknowledged delta gap.
"""
from __future__ import annotations

import json
from pathlib import Path
import threading
import uuid

from codex_process_supervisor import ProcessProxy, _Input, _Stdout


class GuestProcessProxy(ProcessProxy):
    def __init__(self, client, handle, opened, *, root, stderr_sink=None):
        self.client = client
        self.root = Path(root)
        self.handle = handle
        self.write_lock = threading.Lock()
        self.event_lock = threading.RLock()
        self.request_id = 0
        self.stdout = _Stdout(self)
        self.stdin = _Input(self)
        self.stderr = None
        self.stderr_sink = stderr_sink
        self._returncode = opened.get('returnCode')
        self.detached = False
        self.initialize_result = opened.get('initResult') if opened.get('resumed') else None
        self.resumed = opened.get('resumed') is True
        self.generation = opened['generation']
        self.cursor = opened['acknowledged']
        self.read_cursor = self.cursor
        self.ack_pending = set()
        self.sequence = opened['sequence']
        self.remote_to_local = {}

    def call(self, action, **values):
        with self.write_lock:
            self.request_id += 1
            return self.client.request('provider.rpc', {
                'handle': self.handle, 'action': action, **values},
                request_id=str(uuid.uuid4()), timeout=15)

    def ack_applied_deltas(self, applied):
        with self.__dict__.setdefault('_ack_lock', threading.RLock()):
            if not self.ack_pending or self.cursor + 1 in self.ack_pending:
                return 0
            result = self.call('replay', cursor=self.cursor, limit=128)
            missing = []
            contiguous = self.cursor
            for event in result['events']:
                sequence = event['sequence']
                if sequence > self.read_cursor or sequence != contiguous + 1:
                    break
                if sequence not in self.ack_pending:
                    message = json.loads(event['payload'])
                    if (event['kind'] != 'stdout' or not isinstance(message, dict)
                            or 'id' in message or message.get('method') != 'item/agentMessage/delta'):
                        break
                    missing.append(sequence)
                contiguous = sequence
            if not missing or not applied(missing[-1]):
                return 0
            self.ack_pending.update(missing)
            self.ack(contiguous)
            return len(missing)

    def detach(self):
        with self.event_lock:
            if self.detached:
                return
            self.detached = True
            try:
                self.call('detach')
            except Exception as error:
                self.detach_error = str(error)[:500]
