#!/usr/bin/env python3
"""Apply the provider update to the prior source, preserving live method objects."""
import subprocess
import sys
import tempfile
from pathlib import Path
from types import FunctionType

ROOT=Path(__file__).resolve().parents[1]
sys.path.insert(0,str(ROOT/'scripts'))
import codex_runtime
import codex_claude
import codex_claude_update as update
from codex_active_task_update import signature
from codex_progress_update import source_function

with tempfile.TemporaryDirectory() as root:
    runtime=codex_runtime.Runtime(Path(root))
    originals=[]
    try:
        for target,allowed in update.EXPECTED.items():
            module_name,*path=target.split('.')
            name=path[-1]
            module=sys.modules[module_name]
            owner=getattr(module,path[0]) if len(path)==2 else module
            descriptor=vars(owner)[name]
            live=descriptor.__func__ if isinstance(descriptor,staticmethod) else descriptor
            originals.append((live,live.__code__,live.__defaults__,live.__kwdefaults__))
            # Fixed baseline before Claude support.
            source=(ROOT/'scripts'/f'{module_name}.py').read_text() if module_name=='codex_claude' else subprocess.check_output(['git','show','04ce45adf2cbfa97f8d7bc120259af86acb4d047:scripts/'+module_name+'.py'],cwd=ROOT,text=True)
            old,_=source_function(source,path,vars(module))
            assert signature(old) in allowed
            live.__code__,live.__defaults__,live.__kwdefaults__=old.__code__,old.__defaults__,old.__kwdefaults__
        servers=runtime.servers
        assert update.apply(runtime)['status']=='applied'
        assert update.apply(runtime)['status']=='applied'
        assert runtime.servers is servers
        for target,allowed in update.EXPECTED.items():
            module_name,*path=target.split('.')
            name=path[-1]
            owner=getattr(sys.modules[module_name],path[0]) if len(path)==2 else sys.modules[module_name]
            assert signature(getattr(owner,name))==allowed[1]
        live=originals[-1][0]
        correct=live.__code__
        live.__code__=(lambda *args,**kwargs:None).__code__
        try:
            try:update.apply(runtime)
            except RuntimeError as error:assert 'Unknown live provider' in str(error)
            else:raise AssertionError('Unknown baseline accepted')
        finally:live.__code__=correct
        print('PASS: prior source, repeat application, object identity, unknown baseline rejection')
    finally:
        for live,code,defaults,kwdefaults in originals:
            live.__code__,live.__defaults__,live.__kwdefaults__=code,defaults,kwdefaults
        runtime.close()
