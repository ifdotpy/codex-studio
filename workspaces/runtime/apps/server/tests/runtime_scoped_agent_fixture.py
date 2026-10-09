"""Reuse production Runtime scoped agent readers on lightweight test doubles."""

from codex_runtime import Runtime as ProductionRuntime


def install_scoped_agent_reads(runtime_type):
    """Install production readers without copying their filtering rules."""
    runtime_type.team_agents = ProductionRuntime.team_agents
    runtime_type.sync_agent_rooms = ProductionRuntime.sync_agent_rooms
    for name in (
        "account_agents",
        "thread_agents",
        "pending_restart_agents",
        "descendant_agents",
        "release_work_agents",
        "named_agents",
        "affected_agent_room_ids",
        "new_agent_room_ids",
        "project_room_ids",
    ):
        setattr(runtime_type, name, staticmethod(getattr(ProductionRuntime, name)))
    return runtime_type
