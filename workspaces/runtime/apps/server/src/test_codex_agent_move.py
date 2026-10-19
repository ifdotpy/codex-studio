"""A move target imports the agent record without state that only the source server can honor."""
import importlib
import unittest

importlib.import_module('test_isolation').isolate_supervisor_environment()

from codex_agent_move import SOURCE_OPERATION_FIELDS, imported_record


class ImportedRecord(unittest.TestCase):
    def test_source_operation_projections_are_not_imported(self):
        # Fields of the lead that moved to igor-mbp on 2026-10-18 while its
        # subagent transfer was pending on the source server. The target had no
        # transfer row, so every scheduler pass failed with "Unknown account transfer".
        source = {
            'id': 'lead', 'accountKey': 'claude-profile-source', 'threadId': 'native-thread', 'epoch': 0,
            'accountTransfer': {'id': 'abde1958-090d-4a79-8516-3780b6469e81', 'status': 'pending',
                                'scope': 'subagents', 'targetAccountKey': 'default'},
            'usageResume': {'id': 'source-resume', 'accountKey': 'claude-profile-source', 'cause': 'auth',
                            'status': 'started', 'waitingForAuth': True},
            'capacityRetry': {'id': 'source-retry', 'accountKey': 'claude-profile-source', 'status': 'scheduled'},
            'remoteAdmission': {'id': 'admission', 'epoch': 0, 'state': 'granted'},
            'startAttempt': {'id': 'attempt'}, 'executionMove': {'id': 'move'}, 'movedTo': {'server': 'b'},
            'worktreeReady': True,
            'executionArchives': [{'move': 'move', 'server': 'source'}],
        }

        record = imported_record(source)

        self.assertEqual(SOURCE_OPERATION_FIELDS, {'accountTransfer', 'usageResume', 'capacityRetry'})
        for field in (*SOURCE_OPERATION_FIELDS, 'remoteAdmission', 'startAttempt', 'executionMove',
                      'movedTo', 'worktreeReady'):
            self.assertNotIn(field, record)
        self.assertEqual(record['executionArchives'], source['executionArchives'])
        self.assertEqual({k: record[k] for k in ('id', 'accountKey', 'threadId', 'epoch')},
                         {'id': 'lead', 'accountKey': 'claude-profile-source', 'threadId': 'native-thread', 'epoch': 0})
        # The source record keeps its own operations.
        self.assertEqual(source['accountTransfer']['status'], 'pending')


if __name__ == '__main__':
    unittest.main()
