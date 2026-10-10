"""Local aliases for paired servers. Device identities do not change."""
from __future__ import annotations

from collections.abc import Iterable
import json
import re
import sqlite3
from typing import Any
from urllib.parse import urlsplit

DEFAULT_LOCAL_SERVER_ALIAS = "LOC"


def default_alias(server: dict[str, Any], used: Iterable[str]) -> str:
    taken = set(used)
    address = urlsplit(server.get("origin") or "")
    host = (address.hostname or "").split(".")[0]
    preferred = (DEFAULT_LOCAL_SERVER_ALIAS if server["id"] == "local" else
                 (re.sub("[^a-zA-Z]", "", host)[:3] or
                  re.sub("[^a-zA-Z]", "", server.get("label", ""))[:3] or "SRV").upper())
    if preferred not in taken:
        return preferred
    for number in range(26 ** 3):
        candidate = "".join(chr(65 + index) for index in (number // 676, number // 26 % 26, number % 26))
        if candidate not in taken:
            return candidate
    raise ValueError("All server aliases are in use.")


def ensure_aliases(db: sqlite3.Connection, local: dict[str, Any]) -> dict[str, str]:
    row = db.execute("SELECT alias FROM runtime_access_local_alias WHERE id=1").fetchone()
    local_alias = row[0] if row else DEFAULT_LOCAL_SERVER_ALIAS
    peers = [json.loads(row[0]) for row in db.execute(
        "SELECT record FROM runtime_access_clients WHERE json_extract(record,'$.kind')='server' ORDER BY id")]
    used = {local_alias, *(peer["alias"] for peer in peers if peer.get("alias"))}
    aliases = {"local": local_alias, local["serverId"]: local_alias}
    for peer in peers:
        if not peer.get("alias"):
            peer["alias"] = default_alias(peer, used)
            db.execute("UPDATE runtime_access_clients SET record=? WHERE id=?",
                       (json.dumps(peer, ensure_ascii=False, separators=(",", ":")), peer["id"]))
        used.add(peer["alias"])
        aliases[peer["id"]] = peer["alias"]
    return aliases
