"""Authenticated local broker channel. A lost reply never triggers a retry."""
from __future__ import annotations

import asyncio
import json
from pathlib import Path

from common import GuestError, MAX_LINE

SOCKET = Path("/run/codex-studio/layr-admin.sock")


async def admin_request(request_id, method, params, emit=None, *, socket_path=SOCKET):
    sent = False
    writer = None
    try:
        reader, writer = await asyncio.wait_for(asyncio.open_unix_connection(
            str(socket_path), limit=MAX_LINE + 1), 3)
        payload = json.dumps({"id": request_id, "method": method, "params": params}).encode() + b"\n"
        if len(payload) > MAX_LINE:
            raise GuestError("invalid_params", "The admin request exceeds the frame limit")
        sent = True
        writer.write(payload)
        await asyncio.wait_for(writer.drain(), 3)
        while True:
            line = await asyncio.wait_for(reader.readline(), 1810)
            if not line:
                raise GuestError("outcome_unknown", "The admin connection closed before its reply")
            response = json.loads(line)
            if response.get("id") != request_id:
                raise GuestError("outcome_unknown", "The admin reply identity differs")
            if "error" in response:
                raise GuestError(response["error"]["code"], response["error"]["message"])
            if "result" in response:
                return response["result"]
            if emit is not None:
                await emit(response["event"], response.get("data"))
    except (OSError, asyncio.TimeoutError, ValueError) as exc:
        code = "outcome_unknown" if sent else "unavailable"
        raise GuestError(code, "The layr admin channel is unavailable") from exc
    finally:
        if writer is not None:
            writer.close()

