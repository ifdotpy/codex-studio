import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

from codex_vm_account_switch import _COPY


class SessionCopy(unittest.TestCase):
    def test_copies_the_native_session_its_files_and_memory_into_the_target_profile(self) -> None:
        with tempfile.TemporaryDirectory() as home:
            root = Path(home)
            sessions = root / '.codex/studio-bridges/agent-1/sessions'
            sessions.mkdir(parents=True)
            (sessions / 'thread-1.json').write_text(json.dumps({'session': {'id': 'thread-1', 'nativeId': 'native-1'}}))
            (sessions / 'thread-1.meta.json').write_text(json.dumps({'id': 'thread-1', 'cwd': '/line'}))
            source = root / '.claude/studio-accounts/aaa/projects/-line'
            (source / 'native-1' / 'subagents').mkdir(parents=True)
            (source / 'native-1.jsonl').write_text('turns\n')
            (source / 'native-1' / 'subagents' / 'a.jsonl').write_text('sub\n')
            (source / 'memory').mkdir()
            (source / 'memory' / 'MEMORY.md').write_text('note\n')
            (source / 'other-chat.jsonl').write_text('not this chat\n')
            target = root / '.claude/studio-accounts/bbb/projects/-line'
            target.mkdir(parents=True)
            (target / 'native-1.jsonl').write_text('older copy\n')
            output = subprocess.run(
                [sys.executable, '-c', _COPY, 'agent-1', '.claude/studio-accounts/aaa', '.claude/studio-accounts/bbb'],
                env={**os.environ, 'HOME': home}, capture_output=True, text=True, check=True).stdout
            self.assertEqual(json.loads(output), {'copied': ['native-1']})
            self.assertEqual((target / 'native-1.jsonl').read_text(), 'turns\n')
            self.assertEqual((target / 'native-1' / 'subagents' / 'a.jsonl').read_text(), 'sub\n')
            self.assertEqual((target / 'memory' / 'MEMORY.md').read_text(), 'note\n')
            self.assertFalse((target / 'other-chat.jsonl').exists())
            # The source stays for a switch back and as the evidence of the earlier account.
            self.assertEqual((source / 'native-1.jsonl').read_text(), 'turns\n')


if __name__ == '__main__':
    unittest.main()
