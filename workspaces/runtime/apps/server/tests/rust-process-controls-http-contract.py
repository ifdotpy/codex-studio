#!/usr/bin/env python3
"""Process-panel HTTP receipts and owner fences on either backend."""
from __future__ import annotations
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()
import json
import sqlite3
import unittest
from urllib.parse import urlencode
from http_backend_harness import TimedHTTPContractTest, start_contract_backend, no_write_snapshot
from studio_api.persisted_models import PersistedAgentRecord, PersistedMonitorRecord

class ProcessControlsHTTP(TimedHTTPContractTest, unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.backend = start_contract_backend(cls)

    @classmethod
    def tearDownClass(cls):
        cls.backend.stop()

    def headers(self):
        return {'Origin': self.backend.origin or '', 'X-Canvas-Token': self.backend.session['token'],
                'X-Studio-API-Schema': self.backend.schema_hash}

    def setUp(self):
        super().setUp()
        self.owner = self._testMethodName
        self.monitor = self.owner + '-monitor'
        agent = {'id': self.owner, 'rootId': self.owner, 'epoch': 1, 'status': 'paused',
                 'name': 'Process contract', 'isLead': True, 'role': 'orchestrator',
                 'accountKey': 'default', 'cwd': str(self.backend.cwd), 'autoWake': False,
                 'approvalPolicy': 'on-request', 'sandbox': {'type': 'workspace-write'}}
        monitor = {'id': self.monitor, 'agent': self.owner, 'epoch': 1, 'status': 'approval',
                   'command': 'never-run', 'created': 1.0, 'bytes': 0, 'tail': '',
                   'log': str(self.backend.state / 'monitor-logs' / (self.monitor + '.log'))}
        PersistedAgentRecord.model_validate(agent)
        PersistedMonitorRecord.model_validate(monitor)
        with sqlite3.connect(self.backend.state / 'canvas.sqlite3') as db:
            db.execute('INSERT INTO runtime_agents(id,record) VALUES(?,?)', (self.owner, json.dumps(agent)))
            db.execute('INSERT INTO runtime_monitors(id,record) VALUES(?,?)', (self.monitor, json.dumps(monitor)))

    def listing(self):
        response = self.backend.request('GET', '/api/processes?' + urlencode({'agent': self.owner}), headers=self.headers())
        self.assertEqual(response.status, 200, response.body[:1000])
        return response.json()

    def stop_request(self, process):
        return {'action': 'stop_selected', 'agent': self.owner, 'accountKey': 'default', 'epoch': 1,
                'requestId': self.owner + '-stop', 'process': {k: process[k] for k in
                    ('key', 'generation', 'agent', 'accountKey', 'epoch')}}

    def post(self, body):
        return self.backend.request('POST', '/api/processes/action', body=body, headers=self.headers())

    def test_listing_and_missing_receipt_are_read_only(self):
        before = no_write_snapshot(self.backend.state)
        listing = self.listing()
        self.assertIsNone(listing['responseTarget'])
        self.assertEqual(len(listing['processes']), 1)
        self.assertTrue(listing['processes'][0]['capabilities']['stop'])
        query = {'agent': self.owner, 'accountKey': 'default', 'epoch': 1, 'requestId': 'absent'}
        response = self.backend.request('GET', '/api/processes/action?' + urlencode(query), headers=self.headers())
        self.assertEqual(response.status, 200, response.body[:1000])
        self.assertFalse(response.json()['found'])
        self.assertEqual(no_write_snapshot(self.backend.state), before)

    def test_stop_receipt_survives_retry_and_is_scoped(self):
        body = self.stop_request(self.listing()['processes'][0])
        response = self.post(body)
        self.assertEqual(response.status, 200, response.body[:1000])
        result = response.json()
        self.assertEqual(result['processes'][0]['status'], 'stopped', result)
        again = self.post(body)
        self.assertEqual(again.status, 200, again.body[:1000])
        self.assertEqual(again.json()['processes'], result['processes'])
        query = {k: body[k] for k in ('agent', 'accountKey', 'epoch', 'requestId')}
        saved = self.backend.request('GET', '/api/processes/action?' + urlencode(query), headers=self.headers())
        self.assertTrue(saved.json()['found'])
        query['epoch'] = 2
        self.assertFalse(self.backend.request('GET', '/api/processes/action?' + urlencode(query), headers=self.headers()).json()['found'])
        self.assertEqual(self.post({**body, 'includeDescendants': True}).status, 400)

    def test_stale_generation_and_invalid_scope_cannot_stop(self):
        body = self.stop_request(self.listing()['processes'][0])
        body['process']['generation'] = 'stale'
        response = self.post(body)
        self.assertEqual(response.status, 200, response.body[:1000])
        self.assertEqual(response.json()['processes'][0]['status'], 'stale')
        self.assertEqual(self.listing()['processes'][0]['status'], 'approval')
        invalid = {**body, 'requestId': self.owner + '-bad', 'action': 'stop_response'}
        self.assertEqual(self.post(invalid).status, 400)

if __name__ == '__main__':
    unittest.main()
