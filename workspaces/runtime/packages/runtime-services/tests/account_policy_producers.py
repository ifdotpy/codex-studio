"""Emit persisted account and limits documents from the real Python writers."""

import base64
import json
import os
import sqlite3
import sys
import tempfile
from pathlib import Path

server = Path(sys.argv[1])
sys.path.insert(0, str(server))

from codex_accounts import AccountStore  # noqa: E402
from codex_runtime import Runtime  # noqa: E402
from codex_usage_resume import UsageResumeMixin  # noqa: E402
from studio_api.accounts.models import UsageLimitsResponse  # noqa: E402
from pydantic import ValidationError  # noqa: E402


def encoded(value):
    return json.dumps(value, separators=(",", ":"))


def token(account_id):
    claims = {"https://api.openai.com/auth": {"chatgpt_account_id": account_id,
                                               "chatgpt_plan_type": "pro"},
              "email": "producer@example.invalid"}
    payload = base64.urlsafe_b64encode(encoded(claims).encode()).decode().rstrip("=")
    return f"head.{payload}.sig"


def auth_doc(account_id):
    return {"tokens": {"account_id": account_id, "access_token": token(account_id),
                       "id_token": token(account_id), "refresh_token": "fixture-refresh"}}


def write_auth(home, value):
    home.mkdir(parents=True, exist_ok=True)
    (home / "auth.json").write_text(encoded(value), encoding="utf-8")


def restore_limits(snapshot):
    try:
        return UsageLimitsResponse.model_validate(snapshot).model_dump(mode="json", exclude_unset=True)
    except ValidationError:
        return None


def restored_limit_map(database):
    runtime = Runtime.__new__(Runtime)
    runtime.rate_limits_by_account = {}
    runtime.rate_limits = {}
    runtime.restore_rate_limits(database)
    result = dict(runtime.rate_limits_by_account)
    return result


def registry_case(name, store):
    document = json.loads(store.path.read_text(encoding="utf-8"))
    return {"name": name, "registry": encoded(document),
            "python": store.snapshot(refresh=False)}


with tempfile.TemporaryDirectory(prefix="runtime-account-producers-") as temporary:
    root = Path(temporary)
    fake_home = root / "user"
    codex_home = fake_home / ".codex"
    codex_home.mkdir(parents=True)
    os.environ.update(HOME=str(fake_home), CODEX_HOME=str(codex_home),
                      XDG_STATE_HOME=str(root / "state"),
                      STUDIO_CLAUDE_BIN=str(root / "bin" / "claude"))
    produced = []

    def capture(name, store):
        document = json.loads(store.path.read_text(encoding="utf-8"))
        produced.append({"name": name, "registry": encoded(document),
                         "python": store.snapshot(refresh=False)})

    # Constructor with missing auth, then real add/register and rename writes.
    store = AccountStore(root / "constructor")
    capture("constructor-empty-auth", store)
    api_key_home = root / "profiles" / "default-api-key"
    write_auth(api_key_home, {"OPENAI_API_KEY": "fixture-default-api-key"})
    os.environ["CODEX_HOME"] = str(api_key_home)
    api_key_store = AccountStore(root / "constructor-api-key")
    capture("constructor-api-key-identity", api_key_store)
    api_key_store.set_name(
        "default", " " * 1_048_576 + "placeholder",
        "00000000-0000-4000-8000-000000000121",
    )
    capture("oversized-untrimmed-rename-receipt", api_key_store)
    os.environ["CODEX_HOME"] = str(codex_home)
    profile = root / "profiles" / "added"
    write_auth(profile, auth_doc("added-account"))
    added_key = store.register(str(profile))
    capture("register-add", store)
    store.set_name(added_key, "Renamed", "00000000-0000-4000-8000-000000000101")
    capture("rename-receipt", store)
    store.set_name(added_key, r"\ud800", "00000000-0000-4000-8000-000000000107")
    capture("producer-literal-surrogate-label", store)

    # Persist a delete receipt while leaving the application default valid.
    second = root / "profiles" / "second"
    write_auth(second, auth_doc("second-account"))
    second_key = store.register(str(second))
    store.default(second_key)
    store.delete(second_key, "00000000-0000-4000-8000-000000000102")
    capture("delete-receipt", store)

    # Start and complete a real AccountStore login through a no-I/O fake bridge.
    class FakeServer:
        def call(self, *_args, **_kwargs):
            return {"type": "chatgptDeviceCode", "loginId": "fixture-login",
                    "verificationUrl": "https://example.invalid", "userCode": "FIXTURE"}

    class FakeRuntime:
        def connect(self, key, **_kwargs):
            capture("sign-in-start", store)
            write_auth(Path(store._row(key)["home"]), auth_doc("new-login"))
            return FakeServer()

    request = "00000000-0000-4000-8000-000000000103"
    store.start_login(FakeRuntime(), request, label="Signed in")
    capture("sign-in-acknowledged", store)
    login_key = "login-" + request
    store.login_completed(login_key, {"loginId": "fixture-login", "success": True})
    capture("sign-in-complete", store)

    class FailedRuntime:
        def connect(self, *_args, **_kwargs):
            raise RuntimeError("fixture failure")

    store.start_login(FailedRuntime(), "00000000-0000-4000-8000-000000000104")
    capture("sign-in-fail", store)

    # A second successful login for an existing identity writes duplicate status.
    class DuplicateRuntime:
        def connect(self, key, **_kwargs):
            write_auth(Path(store._row(key)["home"]), auth_doc("added-account"))
            return FakeServer()

    duplicate_request = "00000000-0000-4000-8000-000000000105"
    store.start_login(DuplicateRuntime(), duplicate_request)
    duplicate_key = "login-" + duplicate_request
    store.login_completed(duplicate_key, {"loginId": "fixture-login", "success": True})
    capture("duplicate-resolution", store)

    # Cancel a pending new sign-in: AccountStore persists the signed-out state.
    class SignOutServer:
        def __init__(self, cancel=False):
            self.cancel = cancel

        def call(self, *_args, **_kwargs):
            if self.cancel:
                return {"status": "canceled"}
            return {"type": "chatgptDeviceCode", "loginId": "signout-login",
                    "verificationUrl": "https://example.invalid", "userCode": "FIXTURE"}

    signout_calls = [0]

    class SignOutRuntime:
        def connect(self, key, **_kwargs):
            if not (Path(store._row(key)["home"]) / "auth.json").exists():
                write_auth(Path(store._row(key)["home"]), {})
            signout_calls[0] += 1
            return SignOutServer(cancel=signout_calls[0] > 1)

    signout_request = "00000000-0000-4000-8000-000000000106"
    store.start_login(SignOutRuntime(), signout_request)
    store.cancel_login(SignOutRuntime(), signout_request)
    capture("sign-out-cancelled-login", store)

    # Exercise registered-profile disconnect/reconnect and Claude mutations.
    ready_home = root / "profiles" / "reauth-default"
    write_auth(ready_home, auth_doc("base-account"))
    os.environ["CODEX_HOME"] = str(ready_home)
    ready_store = AccountStore(root / "ready-accounts")
    capture("ready-token-constructor", ready_store)
    secondary_home = root / "profiles" / "disconnect-reconnect"
    write_auth(secondary_home, auth_doc("secondary-account"))
    secondary_key = ready_store.register(str(secondary_home))
    ready_store.disconnect(secondary_key)
    capture("disconnect-secondary", ready_store)
    ready_store.reconnect(secondary_key)
    capture("reconnect-secondary", ready_store)

    import codex_claude  # noqa: E402

    claude_metadata = {
        "accountId": "claude:claude@example.invalid",
        "email": "claude@example.invalid",
        "plan": "max",
        "status": "ready",
        "_credentialIdentity": "claude:claude@example.invalid",
    }
    real_installed = codex_claude.installed
    real_claude_metadata = codex_claude.auth_metadata
    codex_claude.installed = lambda _profile=None: True
    codex_claude.auth_metadata = lambda *_args, **_kwargs: claude_metadata
    claude_store = AccountStore(root / "claude-accounts")
    claude_home = root / "claude-profile"
    claude_key = claude_store.register_claude(
        {"configDir": str(claude_home)}, verified_metadata=claude_metadata
    )
    capture("claude-registration", claude_store)
    claude_store.update_claude(
        claude_key,
        {"configDir": str(claude_home), "customModels": ["custom-model"]},
        label="Updated Claude",
    )
    capture("claude-update", claude_store)

    discover_store = AccountStore(root / "discovery-accounts")
    os.environ["CODEX_HOME"] = str(codex_home)
    write_auth(codex_home, auth_doc("discovered-codex"))
    discover_store.discover()
    capture("codex-and-claude-discovery", discover_store)
    codex_claude.installed = real_installed
    codex_claude.auth_metadata = real_claude_metadata

    # Reauthentication persists nativeCompleted before it reconciles the receipt.
    os.environ["CODEX_HOME"] = str(ready_home)
    reauth_request = "00000000-0000-4000-8000-000000000109"
    ready_store.start_login(FakeRuntime(), reauth_request, account_key="default")
    ready_store.login_completed("default", {"loginId": "fixture-login", "success": True})
    capture("reauth-native-completed", ready_store)

    # A submitted RPC with no acknowledgement persists uncertain state.
    class UncertainServer:
        def call(self, *_args, **_kwargs):
            raise RuntimeError("fixture acknowledgement loss")

    class UncertainRuntime:
        def connect(self, *_args, **_kwargs):
            return UncertainServer()

    ready_store.start_login(UncertainRuntime(), "00000000-0000-4000-8000-000000000110")
    capture("uncertain-login", ready_store)

    # Reconciliation of a wrong-email sign-in writes an error receipt and removes its temporary row.
    class WrongEmailRuntime:
        def connect(self, key, **_kwargs):
            write_auth(Path(ready_store._row(key)["home"]), auth_doc("wrong-email-account"))
            return FakeServer()

    ready_store.start_login(
        WrongEmailRuntime(),
        "00000000-0000-4000-8000-000000000111",
        email="expected@example.invalid",
    )
    capture("wrong-email-login", ready_store)
    ready_store.start_login(FakeRuntime(), "00000000-0000-4000-8000-000000000112")
    ready_store.login_completed("login-00000000-0000-4000-8000-000000000112",
                                {"loginId": "fixture-login", "success": False})
    capture("expired-login", ready_store)

    # Exercise the actual workspace patch/put/project/encode SQLite path.
    from codex_sync_entities import ensure_tables, put  # noqa: E402

    database = sqlite3.connect(root / "limits.sqlite")
    ensure_tables(database)
    initial = {"accountKey": "default", "at": None, "data": {
        "rateLimitResetCredits": {
            "availableCount": 1,
            "credits": [{"id": "prior-credit", "status": "available", "resetType": "weekly"}],
        }
    }}
    put(database, "workspace", "current", {
        "stateDir": str(root / "state"),
        "rateLimits": initial,
        "rateLimitsByAccount": {},
    })

    limits_runtime = Runtime.__new__(Runtime)
    limits_runtime.rate_limits_by_account = {}
    limits_runtime.rate_limits = initial
    limits_runtime.db = lambda: database
    limits_runtime.analytics_safe = lambda *_args: None
    limits_runtime.analytics_limit = lambda *_args: None
    limits_runtime._stage_resource_change = lambda *_args: None
    limits_runtime.connection_current = lambda *_args: True
    limits_runtime.usage_resume_limits_changed = lambda *_args: None

    limit_states = [
        ("notification-merge-reset-credits", "default", {
            "data": {"status": "ready", "accountId": "default",
                     "rateLimits": {"primary": {"usedPercent": 12.5, "providerWindow": "weekly"}},
                     "rateLimitsByLimitId": {"alpha": {"limitId": "alpha", "primary": {"usedPercent": 20, "windowDurationMins": 60}}},
                     "rateLimitResetCredits": {"availableCount": 2, "credits": [{"id": "credit-1", "status": "available", "resetType": "weekly", "grantedAt": 1.0, "expiresAt": None}]}},
            "at": 123.5, "readAt": 123.5, "checkedAt": 123.5, "processedAt": 123.5, "error": None}),
        ("partial-non-default-account", "secondary", {
            "data": {"status": "ready", "rateLimits": None,
                     "rateLimitResetCredits": None, "rateLimitsByLimitId": None},
            "at": 124.5, "error": None}),
        ("nullable-limit-payload", "nullable", {
            "data": {"status": "ready", "rateLimits": None, "rateLimitResetCredits": None,
                     "rateLimitsByLimitId": None}, "at": 125.5, "error": None}),
    ]
    limits = []
    for name, account_key, value in limit_states:
        if name == "notification-merge-reset-credits":
            Runtime.notification(
                limits_runtime,
                {
                    "method": "account/rateLimits/updated",
                    "params": {
                        "rateLimits": {"limitId": "primary", "primary": {"usedPercent": 12.5}},
                        "rateLimitsByLimitId": {
                            "alpha": {"limitId": "alpha", "primary": {"usedPercent": 20, "windowDurationMins": 60}}
                        },
                    },
                    "_studioReceivedAt": 123.5,
                },
                account_key,
                "disposable-connection",
            )
        else:
            limits_runtime.store_rate_limits(account_key, value)
        saved = database.execute(
            "SELECT payload FROM sync_entities WHERE collection='workspace' AND id='current'"
        ).fetchone()[0]
        document = json.loads(saved)
        snapshot = document["value"]["rateLimitsByAccount"][account_key]
        expected = restore_limits(snapshot)
        limits.append({"name": name, "accountKey": account_key, "payload": saved,
                       "python": expected, "pythonRestored": restored_limit_map(database)})

    # Change only cache timestamps after the real notification-produced cache.
    previous_row = database.execute(
        "SELECT seq,hash,payload FROM sync_entities WHERE collection='workspace' AND id='current'"
    ).fetchone()
    previous_document = json.loads(previous_row[2])
    timestamp_input = dict(limits_runtime.rate_limits_for("default"))
    timestamp_input.update(at=999.0, readAt=998.0, checkedAt=997.0, processedAt=996.0)
    limits_runtime.store_rate_limits("default", timestamp_input)
    timestamp_row = database.execute(
        "SELECT seq,hash,payload FROM sync_entities WHERE collection='workspace' AND id='current'"
    ).fetchone()
    assert timestamp_row[0] == previous_row[0]
    assert timestamp_row[1] != previous_row[1]
    assert timestamp_row[2] != previous_row[2]
    previous_value = previous_document["value"]
    timestamp_document = json.loads(timestamp_row[2])
    timestamp_value = timestamp_document["value"]
    for root_key in ("rateLimits",):
        before = dict(previous_value[root_key])
        after = dict(timestamp_value[root_key])
        for field in ("at", "readAt", "checkedAt", "processedAt"):
            before.pop(field, None)
            after.pop(field, None)
        assert before == after
    before_map = dict(previous_value["rateLimitsByAccount"]["default"])
    after_map = dict(timestamp_value["rateLimitsByAccount"]["default"])
    for field in ("at", "readAt", "checkedAt", "processedAt"):
        before_map.pop(field, None)
        after_map.pop(field, None)
    assert before_map == after_map
    assert timestamp_value["rateLimitsByAccount"]["default"]["at"] == 999.0
    timestamp_payload = database.execute(
        "SELECT payload FROM sync_entities WHERE collection='workspace' AND id='current'"
    ).fetchone()[0]
    timestamp_document = json.loads(timestamp_payload)
    timestamp_snapshot = timestamp_document["value"]["rateLimitsByAccount"]["default"]
    timestamp_expected = restore_limits(timestamp_snapshot)
    limits.append({"name": "timestamp-only-refresh", "accountKey": "default",
                   "payload": timestamp_payload, "python": timestamp_expected,
                   "pythonRestored": restored_limit_map(database)})

    # Invalid default API entry followed by a valid secondary entry documents
    # the one deliberate restore-map difference: Python aborts the full loop.
    exception_db = sqlite3.connect(root / "limits-restore-exception.sqlite")
    ensure_tables(exception_db)
    exception_runtime = Runtime.__new__(Runtime)
    exception_runtime.rate_limits_by_account = {}
    exception_runtime.rate_limits = initial
    exception_runtime.db = lambda: exception_db
    exception_runtime.analytics_safe = lambda *_args: None
    exception_runtime.analytics_limit = lambda *_args: None
    exception_runtime._stage_resource_change = lambda *_args: None
    exception_runtime.workspace_entity_view = lambda _db: {
        "stateDir": str(root / "state"), "rateLimits": exception_runtime.rate_limits,
        "rateLimitsByAccount": exception_runtime.rate_limits_by_account.copy(),
    }
    exception_runtime.store_rate_limits("default", {
        "data": {"rateLimits": {"primary": {"usedPercent": 1, "placeholder": "extension"}}},
        "at": 1.0,
    })
    exception_runtime.store_rate_limits("secondary", {
        "data": {"status": "ready"}, "at": 2.0,
    })
    exception_payload = exception_db.execute(
        "SELECT payload FROM sync_entities WHERE collection='workspace' AND id='current'"
    ).fetchone()[0]
    exception_snapshot = json.loads(exception_payload)["value"]["rateLimitsByAccount"]["secondary"]
    limits.append({"name": "restore-invalid-default-valid-secondary", "accountKey": "secondary",
                   "payload": exception_payload, "python": restore_limits(exception_snapshot),
                   "pythonRestored": restored_limit_map(exception_db)})
    exception_db.close()

    # The actual writer falls back to put when the workspace row does not exist.
    fallback_db = sqlite3.connect(root / "limits-fallback.sqlite")
    ensure_tables(fallback_db)
    fallback_runtime = Runtime.__new__(Runtime)
    fallback_runtime.rate_limits_by_account = {}
    fallback_runtime.rate_limits = initial
    fallback_runtime.db = lambda: fallback_db
    fallback_runtime.analytics_safe = lambda *_args: None
    fallback_runtime.analytics_limit = lambda *_args: None
    fallback_runtime._stage_resource_change = lambda *_args: None
    fallback_runtime.workspace_entity_view = lambda _db: {
        "stateDir": str(root / "state"),
        "rateLimits": fallback_runtime.rate_limits,
        "rateLimitsByAccount": fallback_runtime.rate_limits_by_account,
    }
    fallback_runtime.store_rate_limits("default", {
        "data": {"status": "ready", "rateLimits": None, "rateLimitResetCredits": None},
        "at": 130.5,
    })
    fallback_payload = fallback_db.execute(
        "SELECT payload FROM sync_entities WHERE collection='workspace' AND id='current'"
    ).fetchone()[0]
    fallback_document = json.loads(fallback_payload)
    fallback_snapshot = fallback_document["value"]["rateLimitsByAccount"]["default"]
    limits.append({
        "name": "missing-row-put-fallback",
        "accountKey": "default",
        "payload": fallback_payload,
        "python": restore_limits(fallback_snapshot),
        "pythonRestored": restored_limit_map(fallback_db),
    })

    # Persist recovery cases through the real usage_resume_save writer.
    recovery_db = sqlite3.connect(root / "usage-resume.sqlite")
    recovery_db.execute(
        "CREATE TABLE runtime_usage_resumes(id TEXT PRIMARY KEY, agent TEXT NOT NULL, record TEXT NOT NULL)"
    )
    recovery_agent = {"id": "producer-agent", "threadId": "producer-thread", "epoch": 4}
    recovery_cases = []
    for name, resume in (
        ("auth-recovery-expired", {"id": "auth-expired", "status": "scheduled", "cause": "auth",
                                    "accountKey": "default", "failedAt": 100.0}),
        ("auth-recovery-recent", {"id": "auth-recent", "status": "scheduled", "cause": "auth",
                                   "accountKey": "default", "failedAt": 4_500.0}),
        ("usage-recovery-old", {"id": "usage-old", "status": "scheduled", "cause": "usage_limit",
                                 "accountKey": "default", "failedAt": 100.0}),
        ("auth-recovery-cancelled", {"id": "auth-cancelled", "status": "cancelled", "cause": "auth",
                                      "accountKey": "default", "failedAt": 100.0}),
    ):
        UsageResumeMixin.usage_resume_save(None, recovery_db, recovery_agent, resume)
        recovery_cases.append({"name": name, "id": resume["id"],
                               "agent": recovery_agent["id"], "record": encoded(resume)})
    recovery_db.commit()
    recovery_db.close()
    fallback_db.close()
    database.close()

    # The real writer accepts this label and writes a JSON surrogate escape;
    # strict Rust UTF-8/JSON decoding must refuse the document without loss.
    store.set_name(added_key, chr(0xD800), "00000000-0000-4000-8000-000000000108")
    invalid_registry = json.loads(store.path.read_text(encoding="utf-8"))
    print(json.dumps({"accounts": produced,
                      "invalidRegistries": [{"name": "python-lone-surrogate-label",
                                             "registry": encoded(invalid_registry)}],
                      "limits": limits,
                      "usageResumes": {"now": 5000.0, "states": recovery_cases}}))
