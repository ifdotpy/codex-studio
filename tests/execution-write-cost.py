#!/usr/bin/env python3
"""Count extra execution SQL through Runtime.put with temporary state."""
import importlib.util
import json
from pathlib import Path
import sys
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parent))
spec = importlib.util.spec_from_file_location('contract', Path(__file__).with_name('execution-identity-contract.py'))
contract_module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(contract_module)
import codex_execution


def measure():
    contract = contract_module.ExecutionIdentityContract()
    contract.setUp()
    counts = {}
    original = codex_execution.reconcile_effect

    def count(name, table, record):
        statements = []

        def traced(runtime, db, *args):
            db.set_trace_callback(statements.append)
            try:
                return original(runtime, db, *args)
            finally:
                db.set_trace_callback(None)

        with contract.runtime.lock, contract.runtime.db() as db, patch('codex_execution.reconcile_effect', traced):
            contract.runtime.put(db, table, record)
        counts[name] = {
            'statements': len(statements),
            'reads': sum(sql.startswith('SELECT') for sql in statements),
            'writes': sum(sql.startswith(('INSERT', 'UPDATE', 'DELETE')) for sql in statements),
        }

    try:
        agent = contract.lead()
        contract_module.fixture.eventually(lambda: contract.records(agent)[0]['attempts'][0]['submission'] == 'accepted')
        current = contract.runtime.agent(agent['id'])
        current['tail'] = 'delta'
        count('agent_delta', 'agents', current)
        current = contract.runtime.agent(agent['id'])
        current['prepareAttempt'] = 'new-preparation'
        count('agent_accepted_attempt_change', 'agents', current)
        receipt = {'id': 'sql-receipt', 'agent': agent['id'], 'turnId': agent['turnId'],
                   'callId': 'sql-call', 'stage': 'reserved', 'created': 1}
        count('tool_request_new', 'tool_requests', receipt)
        count('tool_request_unchanged', 'tool_requests', receipt)
        receipt.update(stage='completed', finished=2)
        count('tool_request_terminal', 'tool_requests', receipt)
        current = contract.runtime.agent(agent['id'])
        current.pop('startAttempt', None)
        current.update(turnId=None, prepareAttempt=None)
        with contract.runtime.db() as db:
            contract.runtime.put(db, 'agents', current)
        current['startAttempt'] = {
            'id': 'sql-unsent', 'epoch': agent['epoch'], 'accountKey': agent['accountKey'],
            'threadId': agent['threadId'], 'events': ['sql-input'], 'created': 3,
        }
        count('agent_first_unsent_attempt', 'agents', current)
        return {'extraStatementsPerPut': counts}
    finally:
        contract.tearDown()


if __name__ == '__main__':
    print(json.dumps(measure(), indent=2))
