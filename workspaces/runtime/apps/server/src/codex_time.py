"""Append immutable clock metadata without changing earlier prompt content."""

from datetime import datetime, timezone
from typing import NotRequired, TypedDict

from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from codex_records import JsonObject


class MessageClock(TypedDict):
    message_id: str
    accepted_at_utc: str


class ToolResult(TypedDict, total=False):
    contentItems: NotRequired[list["JsonObject"]]
    success: NotRequired[bool]


def clock_stamp(at: float) -> str:
    """Use an explicit offset and UTC; never read a clock during a replay."""
    value = datetime.fromtimestamp(at, timezone.utc)
    return value.isoformat(timespec="milliseconds").replace("+00:00", "Z")


def message_clock(message_id: str, at: float) -> MessageClock:
    return {"message_id": message_id, "accepted_at_utc": clock_stamp(at)}


def append_message_clocks(text: str, clocks: list[MessageClock]) -> str:
    if not clocks:
        return text
    lines = [
        f"Message {clock['message_id']} accepted at {clock['accepted_at_utc']}"
        for clock in clocks
    ]
    return text + "\n\n[Time awareness, message receipt time]\n" + "\n".join(lines)


def stamp_tool_result(result: ToolResult, at: float) -> dict[str, object]:
    """Keep structured result text valid JSON; append a separate text item."""
    return {
        **result,
        "contentItems": [
            *result.get("contentItems", []),
            {
                "type": "inputText",
                "text": "[Time awareness] Tool result finalized at " + clock_stamp(at),
            },
        ],
    }
