import type { Agent, Room, Snapshot } from "./types";

// Private rooms predate rootId metadata. Their participants define the scope.
// A direct cross-team conversation appears only in its participants' lead chats.
export function roomLeadIds(room: Room, agents: Agent[]): string[] {
  if (room.kind === "broadcast")
    return room.rootId && room.rootId !== "all" ? [room.rootId] : [];
  const participants = new Set(room.members);
  return [
    ...new Set(
      agents
        .filter((agent) => participants.has(agent.id))
        .map((agent) => (agent.isLead ? agent.id : agent.rootId))
        .filter((id): id is string => !!id),
    ),
  ];
}

// A conversation belongs to one lead tree, even when two chats use the same folder.
// A missing selection must never expand into all sessions.
export function chatSnapshot(
  data: Snapshot | null,
  rootId?: string,
): Snapshot | null {
  if (!data) return null;
  const members = data.threads.filter(
    (agent) => !!rootId && (agent.id === rootId || agent.rootId === rootId),
  );
  const ids = new Set(members.map((agent) => agent.id));
  const owns = (id?: string) => !!id && ids.has(id);
  return {
    ...data,
    threads: members,
    chats: [],
    runtime: {
      ...data.runtime,
      agents: data.runtime.agents.filter((agent) => owns(agent.id)),
      rooms: data.runtime.rooms.filter(
        (room) => !!rootId && roomLeadIds(room, data.threads).includes(rootId),
      ),
      requests: data.runtime.requests.filter((request) => owns(request.agent)),
      complaints: data.runtime.complaints.filter(
        (complaint) => !!rootId && complaint.leadId === rootId,
      ),
      monitors: data.runtime.monitors.filter((monitor) => owns(monitor.agent)),
      tasks: data.runtime.tasks?.filter((task) => owns(task.agent)),
      work: data.runtime.work?.filter(
        (task) => !!rootId && task.rootId === rootId,
      ),
      userTasks: data.runtime.userTasks?.filter((task) => owns(task.agent)),
      rules: data.runtime.rules?.filter((rule) => owns(rule.agent)),
    },
  };
}
