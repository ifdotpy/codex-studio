#!/usr/bin/env python3
"""Opt-in isolated Claude Runtime monitor smoke. No model turns are sent."""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import tempfile
import time
from unittest.mock import patch

spec=importlib.util.spec_from_file_location('account_fixture',Path(__file__).with_name('runtime-accounts-contract.py'))
f=importlib.util.module_from_spec(spec);spec.loader.exec_module(f)
from codex_runtime import AppServer


def eventually(check, timeout=40):
    end=time.monotonic()+timeout
    while time.monotonic()<end:
        value=check()
        if value:return value
        time.sleep(.05)
    raise AssertionError('Native monitor did not reach the expected state')


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--run',action='store_true',help='Run with the installed Claude and Codex executables')
    if not parser.parse_args().run:
        parser.error('Pass --run for this native test')
    with tempfile.TemporaryDirectory(prefix='studio-claude-monitor-native-') as temporary:
        root=Path(temporary).resolve()
        with patch.dict(os.environ,{'CODEX_HOME':str(root/'codex-home')}):
            rt=f.ControlledRuntime(root/'state',AppServer)
            try:
                rt.accounts.discover()
                account=rt.accounts.get('claude-local')
                assert account['status']=='ready','Claude Code must have an active native subscription login'
                lead=rt.new_lead({'cwd':str(root),'account_key':'claude-local'})
                with rt.lock,rt.db() as db:
                    lead.update(autoWake=True,yoloMode=False)
                    rt.put(db,'agents',lead)
                def monitor_record(key):
                    with rt.db() as db:
                        return next(m for m in rt.records(db,'monitors') if m['id']==key)
                monitor=rt.monitor(lead['id'],{'command':"printf 'CLAUDE_MONITOR_READY\\n'; read answer; printf 'RECEIVED:%s\\n' \"$answer\"; sleep 60",'interactive':True})
                assert monitor['status']=='approval'
                with rt.db() as db:
                    request=next(r for r in rt.records(db,'requests') if r.get('params',{}).get('monitorId')==monitor['id'])
                rt.answer(request['id'],{'decision':'accept'})
                def ready():
                    record=monitor_record(monitor['id'])
                    assert record['status'] not in {'failed','lost'},record.get('error')
                    return 'CLAUDE_MONITOR_READY' in record.get('tail','')
                eventually(ready)
                rt.monitor_input(monitor['id'],{'text':'native-input\n'},owner=lead['id'])
                eventually(lambda:'RECEIVED:native-input' in monitor_record(monitor['id']).get('tail',''))
                rt.monitor_input(monitor['id'],{'rows':30,'cols':100},owner=lead['id'])
                rt.cancel_monitor(monitor['id'],owner=lead['id'])
                eventually(lambda:monitor_record(monitor['id'])['status']=='cancelled')
                state=rt.connect('claude-local').call('claude/state',{'threadId':rt.agent(lead['id'])['threadId']})
                assert state['turns']==[], 'The monitor must not create a paid Claude turn'
                assert 'default' not in rt.servers, 'The monitor must stay on the Claude provider connection'
                print(json.dumps({'result':'PASS','approval':True,'sandbox':'workspaceWrite','output':True,'input':True,'resize':True,'cancel':True,'modelTurns':0}))
            finally:
                rt.close()

if __name__=='__main__':main()
