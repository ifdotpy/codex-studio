"""Shared expectations for Rust HTTP runtime contract tests."""

ANSWER_REFUSAL_DEPENDENCIES = (
    "provider-session/question-answer",
)


def answer_refusal_error() -> str:
    # Native callback delivery needs the live retained provider session.
    return "Not implemented: requires provider-session/question-answer"
