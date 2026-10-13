"""Launch planning only, with all executable/auth/discovery effects injected."""
import sys
from pathlib import Path
sys.path.insert(0,str(Path(__file__).resolve().parents[1]/"turn_lifecycle"))
from oracle_guard import install_guard
install_guard(block_processes=True)
import json,os
from unittest.mock import patch
sys.path.insert(0,str(Path(__file__).resolve().parents[6]/"workspaces/runtime/apps/server/src"))
import codex_claude as claude
cases=[]
for args in ("","--chrome --add-dir 'relative folder' --betas test","--no-chrome --verbose","--debug --debug","--model invalid","--add-dir --verbose"):
    profile=dict(id="account",provider="claude",accountId="claude:fixture@example.test",email="fixture@example.test",status="ready",claudeOptions=dict(customModels=[],launchArgs=args))
    with patch.object(claude,"installed",return_value="/fixture/cli"),patch.object(claude,"node_executable",return_value="/fixture/node"),patch.object(claude,"CLAUDE_BRIDGE_ROOT",Path("/fixture/bridge")),patch.object(Path,"is_file",return_value=True),patch.object(claude,"auth_metadata",return_value=dict(status="ready",accountId="claude:fixture@example.test",email="fixture@example.test",plan="max")),patch.dict(os.environ,{"HOME":"/fixture/home","PATH":"/fixture/path","ANTHROPIC_API_KEY":"fixture-value"},clear=True):
        try:
            command,environment=claude.transport("/fixture/root",profile)
            expected=dict(command=command,environment=environment);expected["environment"]["STUDIO_CLAUDE_OPTIONS"]=json.loads(expected["environment"]["STUDIO_CLAUDE_OPTIONS"])
        except ValueError:expected=None
    cases.append(dict(profile=profile,expected=expected))
print(json.dumps(cases))
