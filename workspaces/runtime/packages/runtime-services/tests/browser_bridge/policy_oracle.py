"""Run the owner of the browser URL rules (vm-guest/browser_policy.py) on a JSON corpus.

The Rust port (src/browser_policy.rs) must give the same result for every case: the same
accepted fields, or the same refusal message.
"""
import json
from pathlib import Path
import sys

HERE = Path(__file__).resolve()
VM_GUEST = HERE.parents[2] / "../../apps/vm-guest"
sys.path.insert(0, str(VM_GUEST.resolve()))
import browser_policy  # noqa: E402


def provider_fields(value):
    return {"kind": "provider", "url": value.url, "host": value.host,
            "redirect": value.redirect.record() if value.redirect else None,
            "clientId": value.client_id, "scope": value.scope, "state": value.state,
            "userCode": value.user_code}


def run(case):
    providers = frozenset(case.get("httpProviders", []))
    try:
        if case["rule"] == "classify":
            value = browser_policy.classify(case["url"], http_providers=providers)
            if isinstance(value, browser_policy.StartPage):
                return {"ok": {"kind": "start", "url": value.url, "port": value.port}}
            return {"ok": provider_fields(value)}
        if case["rule"] == "provider":
            return {"ok": provider_fields(browser_policy.provider(case["url"], http_providers=providers))}
        value = browser_policy.pasted(case["url"], {"port": case["port"], "path": case["path"]}, case.get("state"))
        return {"ok": {"kind": "callback", "target": value.target, "state": value.state, "error": value.error}}
    except browser_policy.PolicyError as error:
        return {"error": str(error), "type": "PolicyError"}
    except ValueError as error:
        return {"error": str(error), "type": "ValueError"}


corpus = json.loads(Path(sys.argv[1]).read_text())
print(json.dumps([run(case) for case in corpus]))
