"""Read the current PROGRESS.md display for an agent."""


class PanelMixin:
    def get_panel(self, agent_id):
        from codex_progress import read_progress
        with self.lock, self.db() as db:
            agent = self.checked_actor(db, agent_id)
            identity = agent["id"]
        # File access must not hold the shared runtime or database lock.
        return read_progress(self.root, identity)
