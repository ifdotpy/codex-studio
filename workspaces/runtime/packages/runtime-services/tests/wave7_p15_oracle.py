"""P15 rule completion corpus: Python monitor writer and real completion consumer."""
from pathlib import Path
import copy
import importlib.util
import json
import sys
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).parent / "turn_lifecycle"))
from oracle_guard import install_guard
install_guard(block_processes=True)
spec = importlib.util.spec_from_file_location("rules_oracle", Path(__file__).parent / "rules_engine/oracle.py")
assert spec is not None and spec.loader is not None
oracle = importlib.util.module_from_spec(spec)
spec.loader.exec_module(oracle)
from codex_runtime import Runtime

class Host(oracle.Host):
    monitor = Runtime.monitor
    _finish_monitor = Runtime._finish_monitor
    monitor_auto_approved = staticmethod(Runtime.monitor_auto_approved)
    def assert_workspace_available(self, *args):
        pass
    def launch_monitor(self, *args):
        pass

cases = []
for code in (0, 1):
    for cancelled in (False, True):
        for error in (None, "native failure"):
            host = Host()
            host.root = Path(sys.argv[1])
            owner = {**oracle.OWNER, "cwd": str(host.root)}
            host.insert("agents", owner)
            rule = {**oracle.base(), "command": "printf marker", "checks": 1, "inFlight": True}
            host.put(host.connection, "rules", rule)
            with patch("codex_runtime.time.time", return_value=oracle.NOW), patch("codex_rules.time.time", return_value=oracle.NOW):
                monitor = host.monitor("owner", {"command": rule["command"], "stall_timeout_seconds": 0}, key="rule:rule:1", approved=True, epoch=7, rule=rule)
                monitor.update(status="running", tail="marker", bytes=6, cancelRequested=cancelled, operation={"agent":"owner", "epoch":7, "accountKey":"fixture", "connectionId":"fixture-connection"})
                host.put(host.connection, "monitors", monitor)
                before = copy.deepcopy(monitor)
                host._finish_monitor(monitor["id"], code, error, finished=oracle.NOW)
            cases.append(dict(name=f"rule-{code}-{cancelled}-{error}", actor=owner, before=before, rule=rule, code=code, error=error, now=oracle.NOW, after=host.records(host.connection,"monitors")[0], afterRule=host.records(host.connection,"rules")[0], wakes=host.wakes))
            host.connection.close()
print(json.dumps(cases))
