"""Append immutable clock metadata without changing earlier prompt content."""

from datetime import datetime, timezone


def clock_stamp(at):
    """Use an explicit offset and UTC; never read a clock during a replay."""
    value = datetime.fromtimestamp(at, timezone.utc)
    return value.isoformat(timespec="milliseconds").replace("+00:00", "Z")


def message_clock(message_id, at):
    return {"message_id": message_id, "accepted_at_utc": clock_stamp(at)}


def append_message_clocks(text, clocks):
    if not clocks:
        return text
    lines = [
        f"Message {clock['message_id']} accepted at {clock['accepted_at_utc']}"
        for clock in clocks
    ]
    return text + "\n\n[Time awareness, message receipt time]\n" + "\n".join(lines)


def stamp_tool_result(result, at):
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
