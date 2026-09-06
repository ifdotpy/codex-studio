"""Redeem an exact reset credit with durable native idempotency."""

import json
import time
import uuid

OUTCOMES = {"reset", "nothingToReset", "noCredit", "alreadyRedeemed"}


def _identity(value, name):
    if not isinstance(value, str) or not value or len(value) > 1024:
        raise ValueError("Supply " + name)
    return value


def _read_limits(runtime):
    try:
        data = runtime.connect().call("account/rateLimits/read", {}, timeout=10)
        if not isinstance(data, dict):
            raise ValueError("The account usage response is invalid")
        value = {"data": data, "at": time.time(), "error": None}
    except Exception as error:
        with runtime.lock:
            runtime.rate_limits = {**runtime.rate_limits, "error": str(error)}
        raise
    with runtime.lock:
        runtime.rate_limits = value
    return value


def _setup(db):
    db.executescript("""
        CREATE TABLE IF NOT EXISTS runtime_limit_reset_attempts
            (id TEXT PRIMARY KEY, record TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS runtime_limit_reset_requests
            (id TEXT PRIMARY KEY, attempt_id TEXT NOT NULL);
    """)


def _save(db, attempt):
    db.execute(
        "INSERT INTO runtime_limit_reset_attempts VALUES (?,?) "
        "ON CONFLICT(id) DO UPDATE SET record=excluded.record",
        (attempt["id"], json.dumps(attempt)),
    )


def _result(attempt, limits):
    result = {
        "outcome": attempt.get("outcome") or "uncertain",
        "request_id": attempt["id"],
        "credit_id": attempt["credit_id"],
        "account_id": attempt["account_id"],
        "limits": limits,
    }
    if attempt.get("error"):
        result["error"] = attempt["error"]
    return result


def consume_reset(runtime, data):
    """Serialize redemptions without holding the runtime notification lock.

    A pending attempt means the native call may have completed before a crash.
    Retry that exact credit with the same idempotency key, even if its fresh
    detail no longer says available. Never select a replacement credit.
    """
    account = _identity(data.get("account_id"), "the account id from current usage")
    credit = _identity(data.get("credit_id"), "an exact reset credit id")
    request = data.get("request_id") or str(uuid.uuid4())
    try:
        request = str(uuid.UUID(request))
    except (ValueError, TypeError, AttributeError):
        raise ValueError("Supply a UUID request_id") from None

    with runtime.limits_lock:
        with runtime.lock, runtime.db() as db:
            _setup(db)
            alias = db.execute(
                "SELECT attempt_id FROM runtime_limit_reset_requests WHERE id=?",
                (request,),
            ).fetchone()
            attempts = [
                json.loads(row[0])
                for row in db.execute("SELECT record FROM runtime_limit_reset_attempts")
            ]
            attempt = next((a for a in attempts if alias and a["id"] == alias[0]), None)
            if attempt and (
                attempt["account_id"] != account or attempt["credit_id"] != credit
            ):
                raise ValueError(
                    "This reset request belongs to a different account or credit"
                )
            if not attempt:
                attempt = next(
                    (
                        a
                        for a in reversed(attempts)
                        if a["account_id"] == account
                        and a["credit_id"] == credit
                        and (
                            a["status"] in {"pending", "uncertain"}
                            or a.get("outcome") in {"reset", "alreadyRedeemed"}
                        )
                    ),
                    None,
                )

        limits = _read_limits(runtime)
        if limits["data"].get("accountId") != account:
            raise ValueError(
                "The account changed. Reload usage before using a reset credit"
            )

        if not attempt:
            summary = limits["data"].get("rateLimitResetCredits") or {}
            available = next(
                (
                    row
                    for row in summary.get("credits") or []
                    if row.get("id") == credit
                    and row.get("status") == "available"
                    and row.get("resetType") == "codexRateLimits"
                    and (row.get("expiresAt") is None or row["expiresAt"] > time.time())
                ),
                None,
            )
            if not available:
                raise ValueError(
                    "This exact reset credit is no longer available. Reload usage"
                )
            attempt = {
                "id": request,
                "account_id": account,
                "credit_id": credit,
                "idempotency_key": str(uuid.uuid4()),
                "status": "pending",
                "created": time.time(),
            }
        with runtime.lock, runtime.db() as db:
            _save(db, attempt)
            db.execute(
                "INSERT OR IGNORE INTO runtime_limit_reset_requests VALUES (?,?)",
                (request, attempt["id"]),
            )
        if attempt["status"] == "completed":
            return _result(attempt, limits)

        try:
            response = runtime.connect().call(
                "account/rateLimitResetCredit/consume",
                {"creditId": credit, "idempotencyKey": attempt["idempotency_key"]},
                timeout=20,
            )
            outcome = response.get("outcome") if isinstance(response, dict) else None
            if outcome not in OUTCOMES:
                raise RuntimeError(
                    "The reset result is unknown. Retry the same attempt"
                )
            attempt.update(status="completed", outcome=outcome, error=None)
        except Exception as error:
            attempt.update(status="uncertain", error=str(error))
        attempt["updated"] = time.time()
        with runtime.lock, runtime.db() as db:
            _save(db, attempt)
        # A failed refresh cannot turn a confirmed reset into an unknown spend.
        try:
            limits = _read_limits(runtime)
        except Exception:
            with runtime.lock:
                limits = runtime.rate_limits.copy()
        return _result(attempt, limits)
