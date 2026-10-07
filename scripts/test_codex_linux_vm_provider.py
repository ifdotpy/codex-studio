"""Native guest adapter contracts use a fake guest and no model process."""
import base64
import json
import unittest

from codex_linux_vm_provider import GuestProcessProxy


class FakeGuest:
    def __init__(self):
        self.requests = []
        self.events = []
        self.fail_write = False

    def request(self, method, params, **options):
        self.requests.append((method, params, options))
        if method != 'provider.rpc':
            raise AssertionError(method)
        action = params['action']
        if action == 'write':
            if self.fail_write:
                raise ConnectionError('VM connection lost; input outcome unknown')
            return {'durableMs': 1, 'remoteId': 91}
        if action == 'ack':
            return {'acknowledged': params['sequence']}
        if action == 'next':
            return {'event': self.events.pop(0) if self.events else None, 'returnCode': 0}
        if action == 'replay':
            return {'events': self.events}
        if action == 'detach':
            return {'detached': True}
        raise AssertionError(action)


class GuestNativeProxy(unittest.TestCase):
    def setUp(self):
        self.client = FakeGuest()
        self.opened = {'resumed': True, 'initResult': {'version': 'fixture'},
                       'generation': 2, 'acknowledged': 0, 'sequence': 0, 'returnCode': None}
        self.proxy = GuestProcessProxy(self.client, 'linux-worker:fixture', self.opened,
                                       root='/var/lib/codex-studio/guest/native-supervisor')

    def event(self, sequence, payload, kind='stdout', generation=2):
        return {'sequence': sequence, 'payload': json.dumps(payload),
                'kind': kind, 'generation': generation}

    def large_event(self, *, sequence=1):
        payload = json.dumps({'method':'item/agentMessage/delta', 'params':{'delta':'é'*(1024*1024)}}).encode()
        descriptor = {'sequence':sequence, 'generation':2, 'kind':'stdout',
                      'payload':None, 'payloadBytes':len(payload)}
        original = self.client.request
        def request(method, params, **options):
            if params['action'] != 'eventRead':
                return original(method, params, **options)
            self.client.requests.append((method, params, options))
            offset = params['offset']
            data = payload[offset:offset+params['maxBytes']]
            return {'sequence':sequence, 'generation':2, 'offset':offset,
                'nextOffset':offset+len(data), 'bytes':len(payload),
                'data':base64.b64encode(data).decode(), 'eof':offset+len(data)==len(payload)}
        self.client.request = request
        return descriptor, payload

    def test_large_next_event_is_complete_before_cursor_or_ack_changes(self):
        descriptor, payload = self.large_event()
        self.client.events = [descriptor]
        sequence, line = self.proxy.next_event()
        self.assertEqual(sequence,1)
        self.assertEqual(json.loads(line),json.loads(payload))
        self.assertEqual(self.proxy.cursor,0)
        self.assertEqual(self.proxy.read_cursor,1)
        self.assertEqual([r[1]['offset'] for r in self.client.requests if r[1]['action']=='eventRead'],
                         list(range(0,len(payload),1024*1024)))
        self.assertFalse(any(r[1]['action']=='ack' for r in self.client.requests))

    def test_large_replay_keeps_the_whole_event_sequence(self):
        descriptor, payload = self.large_event()
        self.client.events = [descriptor]
        result = self.proxy.call('replay',cursor=0,limit=128)
        self.assertEqual(result['events'][0]['payload'],payload.decode())
        self.assertEqual(self.proxy.cursor,0)

    def test_large_replay_has_an_aggregate_allocation_limit(self):
        self.client.events = [{'payload':None,'payloadBytes':200*1024*1024}]*2
        with self.assertRaisesRegex(ValueError,'replay exceeds its size limit'):
            self.proxy.call('replay',cursor=0,limit=128)
        self.assertEqual([r[1]['action'] for r in self.client.requests],['replay'])

    def test_bad_large_event_chunk_never_advances_or_acknowledges(self):
        descriptor, _ = self.large_event()
        original = self.client.request
        for field, invalid in [('sequence',2),('generation',1),('offset',1),
                               ('bytes',1),('nextOffset',0),('data','!'),('eof',True)]:
            def request(method, params, **options):
                result = original(method, params, **options)
                if params['action']=='eventRead': result[field]=invalid
                return result
            self.client.request=request
            self.client.events=[dict(descriptor)]
            with self.subTest(field=field), self.assertRaisesRegex(ValueError,'chunk is invalid'):
                self.proxy.next_event()
            self.assertEqual(self.proxy.cursor,0)
            self.assertEqual(self.proxy.read_cursor,0)
        self.assertFalse(any(r[1]['action']=='ack' for r in self.client.requests))

    def test_large_event_descriptor_bounds_are_checked_before_reads(self):
        for size in [0,-1,True,256*1024*1024+1]:
            with self.subTest(size=size), self.assertRaisesRegex(ValueError,'descriptor is invalid'):
                self.proxy.hydrate({'payload':None,'payloadBytes':size,'sequence':1,'generation':2})
        self.assertEqual(self.client.requests,[])

    def test_open_preserves_native_initialization_and_generation(self):
        self.assertTrue(self.proxy.resumed)
        self.assertEqual(self.proxy.initialize_result, {'version': 'fixture'})
        self.assertEqual(self.proxy.generation, 2)
        self.assertEqual(self.proxy.handle, 'linux-worker:fixture')

    def test_request_remap_reuses_native_proxy_behavior(self):
        self.proxy.send_write({'id': 5, 'method': 'thread/read', 'params': {'threadId': 't'}},
                              operation_id='read-exact')
        request = self.client.requests[-1]
        self.assertEqual(request[1]['operationId'], 'read-exact')
        self.assertEqual(request[1]['nativeId'], 5)
        self.assertEqual(self.proxy.remote_to_local, {91: 5})
        self.client.events = [self.event(1, {'id': 91, 'result': {'ready': True}})]
        sequence, line = self.proxy.next_event()
        self.assertEqual(sequence, 1)
        self.assertEqual(json.loads(line), {'id': 5, 'result': {'ready': True}})

    def test_ack_waits_for_contiguous_committed_events(self):
        self.proxy.read_cursor = 2
        self.proxy.ack(2)
        self.assertEqual(self.client.requests, [])
        self.proxy.ack(1)
        self.assertEqual(self.client.requests[-1][1]['sequence'], 2)
        self.assertEqual(self.proxy.cursor, 2)

    def test_delta_repair_requires_durable_runtime_evidence(self):
        self.proxy.read_cursor = 2
        self.proxy.ack_pending = {2}
        self.client.events = [self.event(1, {'method': 'item/agentMessage/delta', 'params': {}}),
                              self.event(2, {'method': 'item/completed', 'params': {}})]
        self.assertEqual(self.proxy.ack_applied_deltas(lambda _: False), 0)
        self.assertEqual(self.proxy.cursor, 0)
        self.assertEqual(self.proxy.ack_applied_deltas(lambda sequence: sequence == 1), 1)
        self.assertEqual(self.proxy.cursor, 2)
        self.assertEqual(self.client.requests[0][1]['limit'], 128)

    def test_delta_repair_does_not_ack_an_uncommitted_request(self):
        self.proxy.read_cursor = 2
        self.proxy.ack_pending = {2}
        self.client.events = [self.event(1, {'id': 7, 'method': 'tool/call', 'params': {}})]
        self.assertEqual(self.proxy.ack_applied_deltas(lambda _: True), 0)
        self.assertEqual(self.proxy.cursor, 0)

    def test_detach_keeps_the_guest_provider_alive(self):
        self.proxy.detach()
        self.proxy.detach()
        self.assertEqual([r[1]['action'] for r in self.client.requests], ['detach'])
        self.assertTrue(self.proxy.detached)

    def test_lost_vm_receipt_is_bounded_and_never_replayed(self):
        self.client.fail_write = True
        with self.assertRaisesRegex(ConnectionError, 'VM connection lost'):
            self.proxy.send_write({'id': 3, 'method': 'turn/start', 'params': {}},
                                  operation_id='turn-exact')
        self.assertEqual(len(self.client.requests), 1)
        self.assertEqual(self.client.requests[0][2]['timeout'], 15)

    def test_old_generation_reply_is_not_delivered_to_a_new_child(self):
        self.client.events = [self.event(1, {'id': 91, 'result': {}}, generation=1),
                              self.event(2, {'method': 'thread/started', 'params': {}})]
        sequence, line = self.proxy.next_event()
        self.assertEqual(sequence, 2)
        self.assertEqual(json.loads(line)['method'], 'thread/started')
        self.assertEqual(self.proxy.cursor, 1)


if __name__ == '__main__':
    unittest.main()
