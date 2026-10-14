"""Actual Python discovery/settings registry writes without a provider process."""
import json
from pathlib import Path
import sys
import tempfile
from unittest.mock import patch
sys.path.insert(0,str(Path(__file__).parent/'turn_lifecycle'))
from oracle_guard import install_guard
install_guard(block_processes=True)
sys.path.insert(0,str(Path(__file__).resolve().parents[3]/'apps/server/src'))
from codex_accounts import AccountStore
metadata={'status':'ready','accountId':'claude:fixture@example.invalid','email':'fixture@example.invalid','plan':'max'}
results=[]
with tempfile.TemporaryDirectory(prefix='p18-profile-registry-') as root:
    store=AccountStore(Path(root))
    store.data['pendingAccountsChange']=True
    store._save()
    initial=json.loads(store.path.read_text())
    with patch('codex_claude.installed',return_value='fixture'),patch('codex_claude.auth_metadata',return_value=metadata):
        store.discover()
        discovered=json.loads(store.path.read_text())
        results.append({'kind':'discover','initial':initial,'expected':discovered,'home':str(Path.home()/'.claude')})
        options={'customModels':[{'id':'fixture','label':'Fixture'}],'launchArgs':'--verbose'}
        store.update_claude('claude-local',options,label='  Updated  ')
        results.append({'kind':'update','initial':discovered,'expected':json.loads(store.path.read_text()),'options':options})
print(json.dumps(results))
