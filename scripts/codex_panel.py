"""Read the current PROGRESS.md display for an agent."""

from typing import TYPE_CHECKING, Protocol

from codex_records import RecordStore

if TYPE_CHECKING:
    import sqlite3
    from contextlib import AbstractContextManager
    from pathlib import Path

    from codex_records import AgentRecord


class PanelRuntime(RecordStore, Protocol):
    lock: "AbstractContextManager[object]"
    root: "Path"

    def db(self) -> "AbstractContextManager[sqlite3.Connection]": ...
    def checked_actor(self, db: "sqlite3.Connection", actor: str) -> "AgentRecord": ...


class PanelMixin:
    def get_panel(self: "PanelRuntime", agent_id: str) -> dict[str, object]:
        from codex_progress import read_progress
        with self.lock, self.db() as db:
            agent = self.checked_actor(db, agent_id)
            identity = agent["id"]
        # File access must not hold the shared runtime or database lock.
        return read_progress(self.root, identity)
