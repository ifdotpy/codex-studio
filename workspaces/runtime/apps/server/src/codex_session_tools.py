"""Keep the public teleport name fixed for each native session."""
from typing import Any, Mapping

LEGACY_MOVE = 'orchestration_move'
TELEPORT = 'orchestration_teleport'
TELEPORT_TOOLS = {LEGACY_MOVE, TELEPORT}


def session_tool_name(actor: Mapping[str, Any]) -> str:
    saved = actor.get('nativeTeleportTool')
    if saved in TELEPORT_TOOLS:
        return str(saved)
    # Imported sessions carry the exact native definitions, including sessions
    # made by an older Studio version that did not save this marker.
    for tool in actor.get('frozenNativeParams', {}).get('dynamicTools', []):
        if tool.get('name') in TELEPORT_TOOLS:
            return str(tool['name'])
    return LEGACY_MOVE if actor.get('threadId') else TELEPORT
