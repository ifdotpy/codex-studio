from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import codex_claude


class HistoryReadTransport(unittest.TestCase):
    """An account transfer reads the history of a Claude profile whose login changed."""

    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.profile = {'id': 'claude-local', 'provider': 'claude', 'status': 'changed',
                        'email': 'first@example.com', 'accountId': 'claude:first@example.com'}
        self.addCleanup(patch.stopall)
        patch.object(codex_claude, 'installed', return_value='/usr/local/bin/claude').start()
        patch.object(codex_claude, 'node_executable', return_value='/usr/local/bin/node').start()
        patch.object(codex_claude, 'subscription_env', return_value={}).start()
        patch.object(codex_claude, 'bridge_options', return_value={}).start()
        patch.object(Path, 'is_file', return_value=True).start()
        # The profile folder is now signed in to another account.
        patch.object(codex_claude, 'auth_metadata', return_value={
            'status': 'ready', 'email': 'second@example.com', 'accountId': 'claude:second@example.com'}).start()

    def test_a_changed_login_still_refuses_a_normal_bridge(self) -> None:
        with self.assertRaisesRegex(ValueError, 'account changed'):
            codex_claude.transport(self.root, self.profile)

    def test_a_history_read_pins_the_registered_identity(self) -> None:
        _command, env = codex_claude.transport(self.root, {**self.profile, '_historyRead': True})
        # The bridge refuses every turn whose login differs from this identity.
        self.assertEqual(env['STUDIO_CLAUDE_ACCOUNT'], 'first@example.com')

    def test_a_history_read_needs_a_changed_profile_with_its_identity(self) -> None:
        for profile in ({**self.profile, 'status': 'ready'}, {**self.profile, 'accountId': 'claude:other@example.com'}):
            with self.assertRaises(ValueError):
                codex_claude.transport(self.root, {**profile, '_historyRead': True})


if __name__ == '__main__':
    unittest.main()
