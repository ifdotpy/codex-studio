"""Actual AccountStore writes with fixture-only transport and auth reconciliation."""
import copy
import json
from pathlib import Path
import sys
import tempfile
from types import SimpleNamespace
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).parent / "turn_lifecycle"))
from oracle_guard import install_guard
install_guard(block_processes=True)
sys.path.insert(0, str(Path(__file__).resolve().parents[3] / "apps/server/src"))
from codex_accounts import AccountStore

request = "4b154e4b-80bb-4f1f-b07f-dcaa15563190"
response = {"type": "chatgptDeviceCode", "loginId": "fixture-login", "verificationUrl": "https://example.invalid/device", "userCode": "fixture-code", "expiresAt": 100.0}
cases = []
with tempfile.TemporaryDirectory(prefix="wave7-p18-registry-") as temporary:
    for existing in (False, True):
        for outcome in ("written", "not_sent", "unknown"):
            root = Path(temporary) / f"{existing}-{outcome}"
            store = AccountStore(root)
            store.data["pendingAccountsChange"] = True
            store.data["accounts"]["default"].update(accountId="fixture-subscription", email="fixture@example.invalid", status="ready")
            store._save()
            initial = json.loads(store.path.read_text())
            store.login_receipts = lambda: []

            def call(method, params, timeout):
                if outcome == "unknown":
                    raise TimeoutError("fixture lost response")
                return response

            def connect(*args, **kwargs):
                if outcome == "not_sent":
                    raise RuntimeError("fixture not sent")
                return SimpleNamespace(call=call)

            with patch("codex_accounts.time.time", return_value=10.0):
                result = store.start_login(SimpleNamespace(connect=connect), request, "default" if existing else None)
            cases.append({"initial": initial, "existing": existing, "outcome": outcome,
                          "response": response, "result": result,
                          "stored": json.loads(store.path.read_text()),
                          "home": str(store.root / ("login-" + request))})
print(json.dumps(cases))
