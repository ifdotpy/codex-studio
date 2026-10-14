"""Shared expectations for Rust HTTP runtime contract tests."""

ANSWER_REFUSAL_DEPENDENCIES = (
    "provider-session/question-answer",
    "question-answer/async-question",
    "question-answer/browser-open",
    "question-answer/monitor-approval",
)


def answer_refusal_error() -> str:
    return f"Not implemented: requires {', '.join(ANSWER_REFUSAL_DEPENDENCIES)}"
