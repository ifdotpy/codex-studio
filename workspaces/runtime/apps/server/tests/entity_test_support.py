"""Shared complete fixtures for values validated inside Runtime.put()."""


def context_repair_wait(
    error="Context repair waits for a native input receipt",
    scope="native",
    *,
    source=None,
    events=None,
    next_check_at=0,
    **extra,
):
    """Build the persisted wait shape written by restart/context recovery."""
    value = {
        "error": error,
        "scope": scope,
        "source": source or {
            "id": "fixture-agent",
            "accountKey": "default",
            "epoch": 1,
            "threadId": "fixture-thread",
            "attemptId": "fixture-attempt",
        },
        "events": list(events or ["fixture-event"]),
        "nextCheckAt": next_check_at,
    }
    value.update(extra)
    return value


def capacity_retry(*, status="scheduled", attempt=1, due_at=1.0, **extra):
    """Build the full capacity retry created and persisted by Runtime."""
    value = {
        "id": "fixture-capacity-retry",
        "threadId": "fixture-thread",
        "turnId": "fixture-turn",
        "accountKey": "default",
        "epoch": 1,
        "cause": "serverOverloaded",
        "status": status,
        "dueAt": due_at,
        "attempt": attempt,
        "maxAttempts": 4,
        "cwd": "/fixture",
        "settings": {
            "model": None,
            "effort": None,
            "nativeEffort": None,
            "fastMode": None,
            "yoloMode": None,
            "profileInstructions": None,
            "role": None,
        },
        "taskClaims": [],
        "updatedAt": 1.0,
    }
    value.update(extra)
    return value


def usage_resume(*, status="scheduled", auth_attempt=0, **extra):
    """Build the full usage resume created and persisted by Runtime."""
    value = {
        "id": "fixture-usage-resume",
        "status": status,
        "accountKey": "default",
        "threadId": "fixture-thread",
        "epoch": 1,
        "turnId": "fixture-turn",
        "cause": "usage_limit",
        "failedAt": 1.0,
        "authAttempt": auth_attempt,
        "authRefreshMarker": None,
        "dueAt": 2.0,
        "plannedAt": None,
        "resetAt": None,
        "reason": None,
        "taskClaims": [],
        "updatedAt": 1.0,
    }
    value.update(extra)
    return value
